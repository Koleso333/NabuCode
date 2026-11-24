use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ncore::{Error, Transport};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{CallError, Sender, escape, fail, form_request, json_response};
use crate::credentials::{Credentials, account_id, claims, unix_ms};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Host opens the URL and handles the loopback callback or manual redirect paste.
pub struct LoginStart {
    pub url: String,
    state: String,
    verifier: String,
}

impl fmt::Debug for LoginStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginStart").finish_non_exhaustive()
    }
}

fn random() -> Result<String, Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| fail("system randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn start_login() -> Result<LoginStart, Error> {
    let state = random()?;
    let verifier = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let pairs = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "openid profile email offline_access"),
        ("state", &state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
    ];
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", escape(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(LoginStart {
        url: format!("https://auth.openai.com/oauth/authorize?{query}"),
        state,
        verifier,
    })
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
}

fn credentials(value: Value, old: Option<&Credentials>) -> Result<Credentials, Error> {
    let tokens: Tokens =
        serde_json::from_value(value).map_err(|_| fail("invalid OAuth token response"))?;
    if tokens.expires_in == 0 {
        return Err(fail("missing OAuth token lifetime"));
    }
    let access_claims = claims(&tokens.access_token);
    let id_claims = tokens.id_token.as_deref().and_then(claims);
    let account = account_id(access_claims.as_ref())
        .or_else(|| account_id(id_claims.as_ref()))
        .or_else(|| old.map(|c| c.account_id.as_str()))
        .ok_or_else(|| fail("OAuth response has no ChatGPT account id"))?;
    if old.is_some_and(|c| c.account_id != account) {
        return Err(fail(
            "ChatGPT account changed during token refresh; reconnect this account",
        ));
    }
    let result = Credentials {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .filter(|s| !s.trim().is_empty())
            .or_else(|| old.and_then(|c| c.refresh_token.clone())),
        expires_at: unix_ms().saturating_add(tokens.expires_in.saturating_mul(1000)),
        account_id: account.into(),
    };
    result.validate().map_err(fail)?;
    Ok(result)
}

/// Consumes the attempt, checks state before I/O. The host persists the result.
pub async fn finish_login(
    transport: &dyn Transport,
    start: LoginStart,
    code: &str,
    state: &str,
) -> Result<Credentials, Error> {
    if state != start.state || code.trim().is_empty() {
        return Err(fail("OAuth state mismatch or missing code"));
    }
    let request = form_request(
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", &start.verifier),
            ("redirect_uri", REDIRECT_URI),
        ],
    );
    let response = Sender::Transport(transport).send(request).await?;
    let result = credentials(json_response(response).await.map_err(|e| e.error)?, None)?;
    if result.refresh_token.is_none() {
        return Err(fail("OAuth login returned no refresh token"));
    }
    Ok(result)
}

pub(crate) async fn refresh(
    sender: Sender<'_>,
    old: &Credentials,
) -> Result<Credentials, CallError> {
    let token = old
        .refresh_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| fail("access token expired or rejected; reconnect this account"))?;
    let response = sender
        .send(form_request(
            TOKEN_URL,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", token),
                ("client_id", CLIENT_ID),
            ],
        ))
        .await?;
    credentials(json_response(response).await?, Some(old)).map_err(CallError::from)
}
