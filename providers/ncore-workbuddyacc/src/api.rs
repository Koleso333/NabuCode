use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::{Error, HttpRequest, HttpResponse, Next, Transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const ORIGIN: &str = "https://www.workbuddy.ai";
pub(crate) const CHAT_URL: &str = "https://www.workbuddy.ai/v2/chat/completions";

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: "workbuddyacc".into(),
        message: message.into(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientInfo {
    pub version: String,
}
impl ClientInfo {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }
    pub(crate) fn headers(&self) -> Result<Vec<(String, String)>, Error> {
        if self.version.is_empty()
            || !self
                .version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        {
            return Err(fail("invalid WorkBuddy client version"));
        }
        Ok(vec![
            ("user-agent".into(), format!("WorkBuddy/{}", self.version)),
            ("x-product".into(), "SaaS".into()),
            ("x-ide-type".into(), "WorkBuddy".into()),
            ("x-ide-name".into(), "WorkBuddy".into()),
            ("x-ide-version".into(), self.version.clone()),
            ("x-requested-with".into(), "XMLHttpRequest".into()),
            ("x-domain".into(), "www.workbuddy.ai".into()),
            ("content-type".into(), "application/json".into()),
        ])
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

pub(crate) fn request(
    client: &ClientInfo,
    method: &str,
    path: &str,
    value: Option<Value>,
) -> Result<HttpRequest, Error> {
    Ok(HttpRequest {
        method: method.into(),
        url: format!("{ORIGIN}{path}"),
        headers: client.headers()?,
        body: value
            .map(|v| Bytes::from(v.to_string()))
            .unwrap_or_default(),
    })
}

pub(crate) async fn read(response: HttpResponse, limit: usize) -> Result<Bytes, Error> {
    let mut body = response.body;
    let mut bytes = BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(fail("WorkBuddy response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes.freeze())
}

pub(crate) async fn envelope(response: HttpResponse) -> Result<Value, Error> {
    let status = response.status;
    let bytes = read(response, 8 * 1024 * 1024).await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| fail(format!("invalid WorkBuddy JSON (HTTP {status})")))?;
    if !(200..300).contains(&status) {
        return Err(fail(format!(
            "WorkBuddy account request failed (HTTP {status}, code {})",
            value["code"]
        )));
    }
    Ok(value)
}
pub(crate) fn data(value: Value) -> Result<Value, Error> {
    if value["code"] != 0 {
        return Err(fail(format!("WorkBuddy business error {}", value["code"])));
    }
    value
        .get("data")
        .cloned()
        .ok_or_else(|| fail("missing WorkBuddy response data"))
}

pub(crate) fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
