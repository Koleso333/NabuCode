use std::fmt;

use bytes::Bytes;
use ncore::{Error, HttpRequest, Transport};
use serde_json::{Value, json};

use crate::Credentials;
use crate::api::{fail, read};

const EXCHANGE_URL: &str = "https://api.deepai.org/deepai_code/desktop_auth/exchange/";
const MAX_BODY: usize = 64 * 1024;

/// Host opens the URL and forwards state and code from GET /auth-callback on loopback.
pub struct LoginStart {
    url: String,
    state: String,
}

impl LoginStart {
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for LoginStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginStart").finish_non_exhaustive()
    }
}

pub fn start_login(port: u16) -> Result<LoginStart, Error> {
    if port == 0 {
        return Err(fail("DeepAI sign-in requires a bound loopback port"));
    }
    let state = random_state()?;
    Ok(LoginStart {
        url: format!("https://api.deepai.org/deepai_code/desktop_auth/?state={state}&port={port}"),
        state,
    })
}

pub struct LoginAccount {
    pub credentials: Credentials,
    pub username: Option<String>,
    pub is_pro: bool,
}

impl fmt::Debug for LoginAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginAccount").finish_non_exhaustive()
    }
}

/// Consumes the login once; the host persists credentials to reuse their account identity.
pub async fn finish_login(
    net: &dyn Transport,
    login: LoginStart,
    state: &str,
    code: &str,
) -> Result<Credentials, Error> {
    finish_login_with_profile(net, login, state, code)
        .await
        .map(|account| account.credentials)
}

pub async fn finish_login_with_profile(
    net: &dyn Transport,
    login: LoginStart,
    state: &str,
    code: &str,
) -> Result<LoginAccount, Error> {
    if !same_state(&login.state, state) || code.is_empty() || code.len() > 4096 {
        return Err(fail("invalid DeepAI sign-in callback; start sign-in again"));
    }
    let response = net
        .send(HttpRequest {
            method: "POST".into(),
            url: EXCHANGE_URL.into(),
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("user-agent".into(), "node".into()),
            ],
            body: Bytes::from(json!({"code": code}).to_string()),
        })
        .await
        .map_err(|_| fail("could not reach DeepAI to finish sign-in; start sign-in again"))?;
    if !(200..300).contains(&response.status) {
        return Err(status_error(response.status));
    }
    let body = read(response, MAX_BODY)
        .await
        .map_err(|_| invalid_response())?;
    account(&body)
}

fn account(body: &[u8]) -> Result<LoginAccount, Error> {
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid_response())?;
    let key = value
        .get("api_key")
        .and_then(Value::as_str)
        .ok_or_else(invalid_response)?;
    let credentials = Credentials::new(key).map_err(|_| invalid_response())?;
    let username = value
        .get("username")
        .and_then(Value::as_str)
        .and_then(|name| {
            let name = name
                .chars()
                .filter(|c| !c.is_control())
                .take(128)
                .collect::<String>();
            let name = name.trim();
            (!name.is_empty()).then(|| name.to_owned())
        });
    Ok(LoginAccount {
        credentials,
        username,
        is_pro: value
            .get("is_pro")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn status_error(status: u16) -> Error {
    fail(match status {
        400 | 401 => "DeepAI sign-in link expired or was already used; start sign-in again",
        403 => "DeepAI sign-in was refused; start sign-in again",
        429 => "too many DeepAI sign-in attempts; try again later",
        _ => "DeepAI could not finish sign-in; try again later",
    })
}

fn invalid_response() -> Error {
    fail("invalid DeepAI sign-in response; start sign-in again")
}

fn random_state() -> Result<String, Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| fail("system randomness unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn same_state(expected: &str, received: &str) -> bool {
    let mut diff = expected.len() ^ received.len();
    // Compare every byte of the fixed-length state without short-circuiting.
    for (index, byte) in expected.bytes().enumerate() {
        diff |= usize::from(byte ^ received.as_bytes().get(index).copied().unwrap_or(0));
    }
    diff == 0
}
