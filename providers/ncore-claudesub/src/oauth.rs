use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ncore::Transport;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{CallError, Sender, fail, json_request, json_response};
use crate::credentials::{Credentials, unix_ms};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Browser URL and private PKCE state. Keep the complete object until finish_login.
pub struct LoginStart {
    pub url: String,
    verifier: String,
    state: String,
}

impl fmt::Debug for LoginStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginStart").finish_non_exhaustive()
    }
}

fn random() -> Result<String, ncore::Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| fail("system randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn start_login() -> Result<LoginStart, ncore::Error> {
    let verifier = random()?;
    let state = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let pairs = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    let query = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(LoginStart {
        url: format!("{AUTHORIZE_URL}?{query}"),
        verifier,
        state,
    })
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

fn credentials(
    v: serde_json::Value,
    previous: Option<&Credentials>,
) -> Result<Credentials, ncore::Error> {
    let t: Tokens = serde_json::from_value(v).map_err(|_| fail("invalid OAuth token response"))?;
    if t.access_token.trim().is_empty() || t.expires_in == 0 {
        return Err(fail("empty OAuth token or lifetime"));
    }
    Ok(Credentials {
        access_token: t.access_token,
        refresh_token: t
            .refresh_token
            .filter(|s| !s.trim().is_empty())
            .or_else(|| previous.and_then(|c| c.refresh_token.clone())),
        expires_at: unix_ms().saturating_add(t.expires_in.saturating_mul(1000)),
        subscription_type: previous.and_then(|c| c.subscription_type.clone()),
        rate_limit_tier: previous.and_then(|c| c.rate_limit_tier.clone()),
    })
}

/// Consume a login attempt and validate the state returned with the code (`code#state`).
/// The host opens the browser and extracts the two callback values. No files are written.
pub async fn finish_login(
    transport: &dyn Transport,
    start: LoginStart,
    code: &str,
    state: &str,
) -> Result<Credentials, ncore::Error> {
    if state != start.state || code.trim().is_empty() {
        return Err(fail("OAuth state mismatch or missing code"));
    }
    let body = json!({"grant_type":"authorization_code", "client_id":CLIENT_ID,
        "code":code, "state":state, "code_verifier":start.verifier, "redirect_uri":REDIRECT_URI});
    let resp = Sender::Transport(transport)
        .send(json_request(TOKEN_URL, &body))
        .await?;
    let v = json_response(resp, "OAuth login")
        .await
        .map_err(|e| e.error)?;
    let c = credentials(v, None)?;
    if c.refresh_token.is_none() {
        return Err(fail("OAuth login returned no refresh token"));
    }
    Ok(c)
}

pub(crate) async fn refresh(
    sender: Sender<'_>,
    old: &Credentials,
) -> Result<Credentials, CallError> {
    let refresh = old
        .refresh_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| fail("access token expired or rejected; reconnect this account"))?;
    let body =
        json!({"grant_type":"refresh_token", "client_id":CLIENT_ID, "refresh_token":refresh});
    let resp = sender.send(json_request(TOKEN_URL, &body)).await?;
    let v = json_response(resp, "OAuth refresh").await?;
    credentials(v, Some(old)).map_err(CallError::from)
}
