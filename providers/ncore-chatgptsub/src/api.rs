use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::{Error, HttpRequest, HttpResponse, Next, Transport};
use serde_json::Value;

pub(crate) const API_BASE: &str = "https://chatgpt.com/backend-api";
pub(crate) const MAX_BODY: usize = 8 * 1024 * 1024;

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: "chatgptsub".into(),
        message: message.into(),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Sender<'a> {
    Next(Next<'a>),
    Transport(&'a dyn Transport),
}

impl Sender<'_> {
    pub async fn send(self, req: HttpRequest) -> Result<HttpResponse, Error> {
        match self {
            Self::Next(next) => next.run(req).await,
            Self::Transport(net) => net.send(req).await.map_err(Error::from),
        }
    }
}

pub(crate) fn retry_after(response: &HttpResponse) -> Duration {
    let raw = response.header("retry-after");
    raw.and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .or_else(|| {
            raw.and_then(|s| httpdate::parse_http_date(s).ok())
                .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
        })
        .unwrap_or(Duration::from_secs(60))
        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
}

pub(crate) struct CallError {
    pub error: Error,
    pub delay: Duration,
}

impl From<Error> for CallError {
    fn from(error: Error) -> Self {
        Self {
            error,
            delay: Duration::from_secs(10),
        }
    }
}

pub(crate) async fn json_response(response: HttpResponse) -> Result<Value, CallError> {
    if !(200..300).contains(&response.status) {
        return Err(CallError {
            error: fail(format!("account request failed (HTTP {})", response.status)),
            delay: retry_after(&response),
        });
    }
    let mut body = response.body;
    let mut bytes = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(fail("account response exceeds size limit").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| fail("invalid account response JSON").into())
}

pub(crate) fn form_request(url: &str, pairs: &[(&str, &str)]) -> HttpRequest {
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", escape(k), escape(v)))
        .collect::<Vec<_>>()
        .join("&");
    HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body: Bytes::from(body),
    }
}

pub(crate) fn escape(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
