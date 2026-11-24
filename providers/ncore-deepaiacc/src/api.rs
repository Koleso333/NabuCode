use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::core::{ErrorKind, ProviderError};
use ncore::{Error, HttpRequest, HttpResponse, Next, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Credentials;

pub(crate) const BASE: &str = "https://api.deepai.org/deepai_code";
pub(crate) const CHAT_URL: &str = "https://api.deepai.org/deepai_code/chat/completions";
pub(crate) const RESPONSES_URL: &str = "https://api.deepai.org/deepai_code/responses";

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: "deepaiacc".into(),
        message: message.into(),
    }
}

pub(crate) fn account_error(operation: &str, error: Error) -> Error {
    match error {
        Error::Plugin { name, message } if name == "deepaiacc" => {
            fail(format!("{operation}: {message}"))
        }
        Error::Provider(mut error) => {
            error.message = format!("{operation}: {}", error.message);
            Error::Provider(error)
        }
        Error::Cancelled => Error::Cancelled,
        Error::Transport(_) => fail(format!("{operation}: network request failed")),
        _ => fail(format!("{operation}: account request failed")),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientInfo {
    pub version: String,
    pub pi_active: Option<bool>,
    pub focused: Option<bool>,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self::new("0.3.40")
    }
}

impl ClientInfo {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            pi_active: None,
            focused: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version.is_empty()
            || self.version.len() > 64
            || !self
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        {
            return Err("invalid DeepAI client version".into());
        }
        Ok(())
    }

    pub(crate) fn beacon(&self, uptime: u64) -> String {
        let mut parts = vec![format!("v={}", self.version), format!("up={uptime}")];
        if let Some(active) = self.pi_active {
            parts.push(format!("pi={}", u8::from(active)));
        }
        if let Some(focused) = self.focused {
            parts.push(format!("focus={}", u8::from(focused)));
        }
        parts.join(";")
    }
}

pub(crate) fn headers(
    credentials: &Credentials,
    lane: u32,
) -> Result<Vec<(String, String)>, Error> {
    credentials.validate().map_err(fail)?;
    if !(1..=crate::lane::MAX_LANE).contains(&lane) {
        return Err(fail("invalid DeepAI lane"));
    }
    Ok(vec![
        ("content-type".into(), "application/json".into()),
        // This header value selects the full-context route, independently of the HTTP runtime.
        ("user-agent".into(), "node".into()),
        ("api-key".into(), credentials.api_key.clone()),
        ("x-deepai-code-theos".into(), lane.to_string()),
        (
            "x-deepai-code-instance".into(),
            credentials.instance.clone(),
        ),
        ("x-deepai-code-machine".into(), credentials.machine_digest()),
    ])
}

#[derive(Clone, Copy)]
pub(crate) enum Sender<'a> {
    Next(Next<'a>),
    Transport(&'a dyn Transport),
}

impl Sender<'_> {
    pub async fn send(self, request: HttpRequest) -> Result<HttpResponse, Error> {
        match self {
            Self::Next(next) => next.run(request).await,
            Self::Transport(net) => net.send(request).await.map_err(Error::from),
        }
    }
}

pub(crate) fn request(
    credentials: &Credentials,
    path: &str,
    lane: u32,
) -> Result<HttpRequest, Error> {
    Ok(HttpRequest {
        method: "GET".into(),
        url: format!("{BASE}{path}"),
        headers: headers(credentials, lane)?,
        body: Bytes::new(),
    })
}

pub(crate) async fn read(response: HttpResponse, limit: usize) -> Result<Bytes, Error> {
    let mut body = response.body;
    let mut bytes = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(fail("DeepAI response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.freeze())
}

pub(crate) async fn json_response(response: HttpResponse) -> Result<(u16, Value), Error> {
    let status = response.status;
    let bytes = read(response, 2 * 1024 * 1024).await?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| fail(format!("invalid DeepAI JSON (HTTP {status})")))?;
    Ok((status, value))
}

fn error_info(code: &str) -> Option<(ErrorKind, &'static str)> {
    Some(match code {
        "theos_insufficient" => (
            ErrorKind::Billing,
            "The account has insufficient Theos for this request.",
        ),
        "theos_exhausted" => (ErrorKind::Billing, "The account has exhausted its Theos."),
        "network_bound" => (
            ErrorKind::Billing,
            "The network free allowance is unavailable.",
        ),
        "machine_required" => (
            ErrorKind::PermissionDenied,
            "The server requires a machine identifier.",
        ),
        "check_in_refused" => (
            ErrorKind::PermissionDenied,
            "The server refused this check-in.",
        ),
        "check_in_unavailable" => (ErrorKind::Overloaded, "The DeepAI check-in is unavailable."),
        "email_unverified" => (ErrorKind::Auth, "The account email is not verified."),
        "social_login_required" => (
            ErrorKind::Auth,
            "The server requires a supported account login.",
        ),
        "lane_version_unsupported" => (
            ErrorKind::InvalidRequest,
            "The server does not support this client lane.",
        ),
        "lane_updated" => (
            ErrorKind::InvalidRequest,
            "DeepAI lane was updated; retry this turn explicitly. No model request was replayed.",
        ),
        "invalid_api_key" | "invalid_api_key_error" | "authentication_error" => {
            (ErrorKind::Auth, "The DeepAI account key was rejected.")
        }
        "rate_limit_exceeded" | "rate_limit_error" | "too_many_requests" => {
            (ErrorKind::RateLimited, "DeepAI rate limit reached.")
        }
        "context_length_exceeded" => (
            ErrorKind::RequestTooLarge,
            "The request exceeds the DeepAI context limit.",
        ),
        "billing_error" => (
            ErrorKind::Billing,
            "DeepAI refused the request due to billing.",
        ),
        "permission_error" => (ErrorKind::PermissionDenied, "DeepAI denied access."),
        "invalid_request_error" => (ErrorKind::InvalidRequest, "DeepAI rejected the request."),
        "request_too_large" => (
            ErrorKind::RequestTooLarge,
            "The DeepAI request is too large.",
        ),
        "overloaded_error" => (ErrorKind::Overloaded, "DeepAI is overloaded."),
        "api_error" | "server_error" => (ErrorKind::Server, "DeepAI reported a server error."),
        "timeout_error" => (ErrorKind::Timeout, "The DeepAI request timed out."),
        _ => return None,
    })
}

pub(crate) fn provider_error(status: Option<u16>, value: &Value) -> ProviderError {
    let classified = ["/error/code", "/code", "/error/type", "/type"]
        .into_iter()
        .filter_map(|path| value.pointer(path).and_then(Value::as_str))
        .find_map(|code| error_info(code).map(|(kind, message)| (code, kind, message)));
    let Some((code, kind, message)) = classified else {
        let message = status.map_or_else(
            || "DeepAI reported an unrecognized stream error.".into(),
            |status| format!("DeepAI request failed (HTTP {status})."),
        );
        let kind = status.map_or(ErrorKind::Other, ErrorKind::from_status);
        // The HTTP status alone does not identify an account limit.
        return make_error(status, "request_failed", "deepai_error", kind, message);
    };
    let error_type = match kind {
        ErrorKind::Billing => "billing_error",
        ErrorKind::Auth => "authentication_error",
        ErrorKind::PermissionDenied => "permission_error",
        ErrorKind::RateLimited => "rate_limit_error",
        ErrorKind::Overloaded => "overloaded_error",
        ErrorKind::RequestTooLarge => "request_too_large",
        ErrorKind::InvalidRequest => "invalid_request_error",
        ErrorKind::Server => "server_error",
        ErrorKind::Timeout => "timeout_error",
        _ => "deepai_error",
    };
    make_error(status, code, error_type, kind, message.into())
}

fn make_error(
    status: Option<u16>,
    code: &str,
    error_type: &str,
    kind: ErrorKind,
    message: String,
) -> ProviderError {
    let raw =
        Bytes::from(json!({"error":{"code":code,"type":error_type,"message":message}}).to_string());
    ProviderError {
        kind,
        status,
        code: Some(code.into()),
        message,
        raw,
    }
}

pub(crate) fn normalize_error(bytes: &mut Bytes, status: Option<u16>) {
    if status.is_some_and(|status| status >= 400) {
        let value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
        *bytes = provider_error(status, &value).raw;
        return;
    }
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    if value.get("error").is_some_and(|e| !e.is_null()) || value["type"] == "error" {
        let error = provider_error(status, &value);
        let mut normalized: Value = serde_json::from_slice(&error.raw).expect("serialized error");
        if value["type"] == "error" {
            normalized["type"] = "error".into();
        }
        *bytes = Bytes::from(normalized.to_string());
    } else if value["type"] == "response.failed" && !value["response"]["error"].is_null() {
        let error = provider_error(status, &json!({"error": value["response"]["error"]}));
        let normalized: Value = serde_json::from_slice(&error.raw).expect("serialized error");
        let mut value = value;
        value["response"]["error"] = normalized["error"].clone();
        *bytes = Bytes::from(value.to_string());
    }
}
