use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use ncore::{Error, HttpRequest, Transport};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::{self, Sender, fail};
use crate::credentials::{CLIENT_ID, unix_ms};
use crate::{Client, Credentials, Profile};

pub const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write workspaces:read workspaces:write";

/// The host binds a loopback port before opening the URL, and forwards code and state.
pub struct LoginStart {
    pub url: String,
    redirect: String,
    state: String,
    verifier: String,
}

pub fn start_login(port: u16) -> Result<LoginStart, Error> {
    if port == 0 {
        return Err(fail("OAuth requires a bound loopback port"));
    }
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let state = random()?;
    let verifier = random()?;
    let nonce = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let query = form(&[
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", &redirect),
        ("scope", SCOPE),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("referrer", "grok-build"),
    ]);
    Ok(LoginStart {
        url: format!("https://auth.x.ai/oauth2/authorize?{query}"),
        redirect,
        state,
        verifier,
    })
}

pub async fn finish_login(
    transport: &dyn Transport,
    client: &Client,
    start: LoginStart,
    code: &str,
    state: &str,
) -> Result<(Credentials, Profile), Error> {
    if code.trim().is_empty() || state != start.state {
        return Err(fail("OAuth state mismatch or missing code"));
    }
    let tokens = exchange(
        Sender::Transport(transport),
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", &start.redirect),
            ("code_verifier", &start.verifier),
        ],
    )
    .await?;
    let profile = client
        .profile_with(Sender::Transport(transport), &tokens.access_token)
        .await?;
    let credentials = tokens.credentials(profile.user_id.clone(), None)?;
    if credentials.refresh_token.is_none() {
        return Err(fail("Grok OAuth login returned no refresh token"));
    }
    Ok((credentials, profile))
}

pub(crate) async fn refresh(sender: Sender<'_>, old: &Credentials) -> Result<Credentials, Error> {
    let token = old
        .refresh_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| fail("Grok session expired; reconnect the account"))?;
    exchange(
        sender,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", token),
        ],
    )
    .await?
    .credentials(old.user_id.clone(), Some(token))
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

impl Tokens {
    fn credentials(
        self,
        user_id: String,
        previous_refresh: Option<&str>,
    ) -> Result<Credentials, Error> {
        let credentials = Credentials {
            access_token: self.access_token,
            refresh_token: self
                .refresh_token
                .filter(|token| !token.trim().is_empty())
                .or_else(|| previous_refresh.map(str::to_owned)),
            expires_at: unix_ms().saturating_add(self.expires_in.saturating_mul(1000)),
            user_id,
        };
        credentials.validate().map_err(fail)?;
        Ok(credentials)
    }
}

async fn exchange(sender: Sender<'_>, fields: &[(&str, &str)]) -> Result<Tokens, Error> {
    let tokens: Tokens = api::json(
        sender
            .send(HttpRequest {
                method: "POST".into(),
                url: TOKEN_URL.into(),
                headers: vec![
                    (
                        "content-type".into(),
                        "application/x-www-form-urlencoded".into(),
                    ),
                    ("accept".into(), "application/json".into()),
                ],
                body: Bytes::from(form(fields)),
            })
            .await?,
    )
    .await?;
    if tokens.expires_in == 0 || !crate::credentials::header_token(&tokens.access_token) {
        return Err(fail("invalid Grok OAuth token or expiry"));
    }
    Ok(tokens)
}

fn random() -> Result<String, Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| fail("system randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn form(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            body.push('&');
        }
        body.push_str(key);
        body.push('=');
        for b in value.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                body.push(b as char);
            } else {
                use std::fmt::Write;
                write!(body, "%{b:02X}").expect("writing to String");
            }
        }
    }
    body
}
