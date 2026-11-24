use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use ncore::{Clock, Error, HttpRequest, HttpResponse, Next, Transport};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::account::{Account, composite_token};
use crate::consts::{
    DEFAULT_PROJECT_ID, ENDPOINT_DAILY, ENDPOINT_PROD, OAUTH_AUTH_URL, OAUTH_CLIENT_ID,
    OAUTH_CLIENT_SECRET, OAUTH_SCOPES, OAUTH_TOKEN_URL, OAUTH_USERINFO_URL, client_headers,
    client_metadata,
};
use crate::request::hex;

/// Either the rest of a middleware chain or a bare transport.
#[derive(Clone, Copy)]
pub enum Sender<'a> {
    Next(Next<'a>),
    Transport(&'a dyn Transport),
}

impl Sender<'_> {
    pub async fn send(&self, req: HttpRequest) -> Result<HttpResponse, Error> {
        match self {
            Sender::Next(n) => n.run(req).await,
            Sender::Transport(t) => t.send(req).await.map_err(Error::from),
        }
    }

    async fn call(&self, req: HttpRequest) -> Result<(u16, Bytes), Error> {
        let resp = self.send(req).await?;
        Ok((resp.status, read_body(resp).await?))
    }
}

pub async fn read_body(resp: HttpResponse) -> Result<Bytes, Error> {
    let mut body = resp.body;
    let mut buf = BytesMut::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf.freeze())
}

fn fail(message: String) -> Error {
    Error::Plugin {
        name: "antigravity".into(),
        message,
    }
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> Bytes {
    let s = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Bytes::from(s)
}

fn post_form(url: &str, pairs: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url: url.into(),
        headers: vec![(
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        )],
        body: form(pairs),
    }
}

fn post_json(url: String, token: &str, body: &Value) -> HttpRequest {
    let mut headers = client_headers();
    headers.push(("authorization".into(), format!("Bearer {token}")));
    HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: Bytes::from(body.to_string()),
    }
}

fn json_of(status: u16, body: &[u8], what: &str) -> Result<Value, Error> {
    if status >= 400 {
        return Err(fail(format!(
            "{what} failed: {status} {}",
            String::from_utf8_lossy(body)
        )));
    }
    serde_json::from_slice(body).map_err(|e| fail(format!("{what}: {e}")))
}

/// Fresh access token and its lifetime.
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    pub expires_in: Duration,
}

pub async fn refresh(sender: Sender<'_>, refresh_token: &str) -> Result<AccessToken, Error> {
    let req = post_form(
        OAUTH_TOKEN_URL,
        &[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    );
    let (status, body) = sender.call(req).await?;
    let v = json_of(status, &body, "token refresh")?;
    let token = v["access_token"]
        .as_str()
        .ok_or_else(|| fail("token refresh: no access_token".into()))?
        .to_owned();
    let secs = v["expires_in"].as_u64().unwrap_or(300);
    Ok(AccessToken {
        token,
        expires_in: Duration::from_secs(secs),
    })
}

/// Browser step of the PKCE login; keep `verifier` and `state` for `finish_login`.
#[derive(Debug, Clone)]
pub struct LoginStart {
    pub url: String,
    pub verifier: String,
    pub state: String,
}

fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    b
}

pub fn start_login(redirect_uri: &str) -> LoginStart {
    let verifier = URL_SAFE_NO_PAD.encode(random(32));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = hex(&random(16));
    let scope = OAUTH_SCOPES.join(" ");
    let params = [
        ("client_id", OAUTH_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", scope.as_str()),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
    ];
    let query = String::from_utf8(form(&params).to_vec()).expect("ascii");
    LoginStart {
        url: format!("{OAUTH_AUTH_URL}?{query}"),
        verifier,
        state,
    }
}

/// Exchanges the callback code, looks up the email and discovers the project.
pub async fn finish_login(
    transport: &dyn Transport,
    clock: &dyn Clock,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Account, Error> {
    let sender = Sender::Transport(transport);
    let req = post_form(
        OAUTH_TOKEN_URL,
        &[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ],
    );
    let (status, body) = sender.call(req).await?;
    let t = json_of(status, &body, "token exchange")?;
    let access = t["access_token"]
        .as_str()
        .ok_or_else(|| fail("no access token received".into()))?;
    let refresh = t["refresh_token"]
        .as_str()
        .ok_or_else(|| fail("no refresh token received".into()))?;
    let (status, body) = sender
        .call(HttpRequest {
            method: "GET".into(),
            url: OAUTH_USERINFO_URL.into(),
            headers: vec![("authorization".into(), format!("Bearer {access}"))],
            body: Bytes::new(),
        })
        .await?;
    let email = json_of(status, &body, "user info")?["email"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let project = discover_project(sender, clock, access, None).await?;
    Ok(Account {
        email,
        refresh_token: composite_token(refresh, None, Some(&project)),
        project_id: Some(project),
        enabled: true,
    })
}

/// Finds the account's Cloud Code project, onboarding it on the default tier if needed.
pub async fn discover_project(
    sender: Sender<'_>,
    clock: &dyn Clock,
    token: &str,
    project: Option<&str>,
) -> Result<String, Error> {
    let mut metadata = client_metadata();
    if let Some(p) = project {
        metadata["duetProject"] = json!(p);
    }
    let mut loaded = None;
    for ep in [ENDPOINT_PROD, ENDPOINT_DAILY] {
        let req = post_json(
            format!("{ep}/v1internal:loadCodeAssist"),
            token,
            &json!({"metadata": metadata, "mode": 1}),
        );
        let Ok((status, body)) = sender.call(req).await else {
            continue;
        };
        let Ok(v) = json_of(status, &body, "loadCodeAssist") else {
            continue;
        };
        match &v["cloudaicompanionProject"] {
            Value::String(p) => return Ok(p.clone()),
            Value::Object(o) if o.get("id").and_then(Value::as_str).is_some() => {
                return Ok(o["id"].as_str().unwrap_or_default().to_owned());
            }
            _ => {}
        }
        loaded = Some(v);
        break;
    }
    if let Some(v) = loaded {
        let tiers = v["allowedTiers"].as_array().cloned().unwrap_or_default();
        let tier = tiers
            .iter()
            .find(|t| t["isDefault"].as_bool() == Some(true))
            .or(tiers.first())
            .and_then(|t| t["id"].as_str())
            .unwrap_or("free-tier")
            .to_owned();
        if let Some(p) = onboard(sender, clock, token, &tier, &metadata, project).await {
            return Ok(p);
        }
    }
    Ok(project.unwrap_or(DEFAULT_PROJECT_ID).to_owned())
}

async fn onboard(
    sender: Sender<'_>,
    clock: &dyn Clock,
    token: &str,
    tier: &str,
    metadata: &Value,
    project: Option<&str>,
) -> Option<String> {
    let body = json!({"tierId": tier, "metadata": metadata});
    for ep in [ENDPOINT_DAILY, ENDPOINT_PROD] {
        for attempt in 0..10 {
            let req = post_json(format!("{ep}/v1internal:onboardUser"), token, &body);
            let Ok((status, raw)) = sender.call(req).await else {
                break;
            };
            let Ok(v) = json_of(status, &raw, "onboardUser") else {
                break;
            };
            // `done` is final; an empty project there means none will be assigned.
            if v["done"].as_bool() == Some(true) {
                let id = v["response"]["cloudaicompanionProject"]["id"].as_str();
                return id.or(project).map(str::to_owned);
            }
            if attempt < 9 {
                clock.sleep(Duration::from_secs(5)).await;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Script(std::sync::Mutex<Vec<&'static str>>);

    impl Transport for Script {
        fn send(
            &self,
            _req: HttpRequest,
        ) -> ncore::rt::BoxFuture<'_, Result<HttpResponse, ncore::TransportError>> {
            let body = self.0.lock().unwrap().remove(0);
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: Box::pin(futures_util::stream::iter([Ok(Bytes::from(body))])),
                })
            })
        }
    }

    #[derive(Default)]
    struct Sleeps(std::sync::atomic::AtomicU32);

    impl Clock for Sleeps {
        fn sleep(&self, _d: Duration) -> ncore::rt::BoxFuture<'static, ()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async {})
        }
    }

    #[test]
    fn finished_onboarding_without_project_stops_at_once() {
        let t = Script(std::sync::Mutex::new(vec![
            r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#,
            r#"{"done":true,"response":{"cloudaicompanionProject":{}}}"#,
        ]));
        let clock = Sleeps::default();
        let p = futures_util::FutureExt::now_or_never(discover_project(
            Sender::Transport(&t),
            &clock,
            "tok",
            None,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(p, DEFAULT_PROJECT_ID);
        assert_eq!(clock.0.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(t.0.lock().unwrap().is_empty());
    }

    #[test]
    fn login_url_has_pkce() {
        let s = start_login("http://localhost:51321/oauth-callback");
        assert!(s.url.starts_with(OAUTH_AUTH_URL));
        assert!(s.url.contains("code_challenge_method=S256"));
        assert!(
            s.url
                .contains("redirect_uri=http%3A%2F%2Flocalhost%3A51321%2Foauth-callback")
        );
        assert_eq!(s.verifier.len(), 43);
        assert_eq!(s.state.len(), 32);
    }
}
