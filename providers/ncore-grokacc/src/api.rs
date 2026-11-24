use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::{Error, HttpRequest, HttpResponse, Next, Transport};
use serde::de::DeserializeOwned;

use crate::Credentials;

pub(crate) const BASE: &str = "https://cli-chat-proxy.grok.com";
pub(crate) const LIMIT: usize = 8 * 1024 * 1024;

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: "grokacc".into(),
        message: message.into(),
    }
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
            Self::Transport(net) => Ok(net.send(request).await?),
        }
    }
}

pub(crate) async fn read(mut response: HttpResponse) -> Result<Bytes, Error> {
    let mut body = BytesMut::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > LIMIT {
            return Err(fail("Grok response exceeds size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

pub(crate) async fn json<T: DeserializeOwned>(response: HttpResponse) -> Result<T, Error> {
    if !(200..300).contains(&response.status) {
        return Err(fail(format!(
            "Grok account request failed (HTTP {})",
            response.status
        )));
    }
    serde_json::from_slice(&read(response).await?).map_err(|_| fail("invalid Grok response JSON"))
}

pub(crate) fn authenticate(request: &mut HttpRequest, access: &str, version: &str) {
    request.headers.retain(|(key, _)| {
        ![
            "authorization",
            "proxy-authorization",
            "cookie",
            "x-api-key",
            "x-goog-api-key",
            "host",
            "content-length",
            "content-encoding",
            "transfer-encoding",
            "user-agent",
            "x-xai-token-auth",
            "x-grok-client-version",
            "x-grok-model-override",
            "x-grok-client-mode",
            "x-userid",
            "accept",
        ]
        .iter()
        .any(|name| key.eq_ignore_ascii_case(name))
    });
    request.headers.extend([
        ("authorization".into(), format!("Bearer {access}")),
        ("x-xai-token-auth".into(), "xai-grok-cli".into()),
        ("x-grok-client-version".into(), version.into()),
        (
            "user-agent".into(),
            concat!("nabucore-grokacc/", env!("CARGO_PKG_VERSION")).into(),
        ),
        (
            "accept".into(),
            if request.method == "POST" {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
    ]);
}

pub(crate) fn get(path: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url: format!("{BASE}{path}"),
        headers: Vec::new(),
        body: Bytes::new(),
    }
}

pub(crate) fn authorize(request: &mut HttpRequest, account: &Credentials, version: &str) {
    authenticate(request, &account.access_token, version);
    if request.url.ends_with("/billing?format=credits") || request.url.ends_with("/settings") {
        request
            .headers
            .push(("x-userid".into(), account.user_id.clone()));
        request
            .headers
            .push(("x-grok-client-mode".into(), "cli".into()));
    }
}
