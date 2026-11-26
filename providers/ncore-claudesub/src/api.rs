use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::{Error, HttpRequest, HttpResponse, Next, Transport};
use serde_json::Value;

pub const API_BASE: &str = "https://api.anthropic.com";
/// Interactive Claude Code CLI; it carries no machine details.
pub const USER_AGENT: &str = "claude-cli/2.1.322 (external, cli)";
const MAX_BODY: usize = 2 * 1024 * 1024;

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: "claudesub".into(),
        message: message.into(),
    }
}

pub(crate) fn message(error: &Error) -> String {
    match error {
        Error::Plugin { name, message } if name == "claudesub" => message.clone(),
        _ => error.to_string(),
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
            Self::Next(n) => n.run(req).await,
            Self::Transport(t) => t.send(req).await.map_err(Error::from),
        }
    }
}

pub(crate) fn headers(token: &str, monitoring: bool) -> Vec<(String, String)> {
    let mut h = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("user-agent".into(), USER_AGENT.into()),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
    ];
    if monitoring {
        h.push(("anthropic-beta".into(), "oauth-2025-04-20".into()));
    }
    h
}

pub(crate) fn retry_after(resp: &HttpResponse) -> Duration {
    let raw = resp.header("retry-after");
    let delay = raw
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| Duration::from_secs_f64(n.clamp(1.0, 3600.0)))
        .or_else(|| {
            raw.and_then(|s| httpdate::parse_http_date(s).ok())
                .map(|t| {
                    t.duration_since(SystemTime::now())
                        .unwrap_or(Duration::from_secs(1))
                })
        })
        .unwrap_or(Duration::from_secs(60));
    delay.clamp(Duration::from_secs(1), Duration::from_secs(3600))
}

pub(crate) struct CallError {
    pub error: Error,
    pub retry_after: Duration,
}

impl From<Error> for CallError {
    fn from(error: Error) -> Self {
        Self {
            error,
            retry_after: Duration::from_secs(10),
        }
    }
}

pub(crate) async fn json_response(resp: HttpResponse, what: &str) -> Result<Value, CallError> {
    if !(200..300).contains(&resp.status) {
        return Err(CallError {
            error: fail(format!("{what} failed (HTTP {})", resp.status)),
            retry_after: retry_after(&resp),
        });
    }
    let mut body = resp.body;
    let mut bytes = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY {
            return Err(fail(format!("{what} response exceeds size limit")).into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| fail(format!("{what} returned invalid JSON")).into())
}

pub(crate) fn json_request(url: &str, body: &Value) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("user-agent".into(), USER_AGENT.into()),
        ],
        body: Bytes::from(body.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_supports_seconds_http_dates_and_invalid_values() {
        for (raw, expected) in [
            ("120".to_owned(), 120),
            ("NaN".to_owned(), 60),
            ("0".to_owned(), 1),
            (
                httpdate::fmt_http_date(
                    SystemTime::UNIX_EPOCH + Duration::from_secs(4_102_444_800),
                ),
                3600,
            ),
        ] {
            let response = HttpResponse {
                status: 429,
                headers: vec![("Retry-After".into(), raw)],
                body: Box::pin(futures_util::stream::empty()),
            };
            assert_eq!(retry_after(&response), Duration::from_secs(expected));
        }
    }
}
