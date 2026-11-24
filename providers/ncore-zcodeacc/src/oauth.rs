use std::fmt;
use std::time::Duration;

use ncore::{Error, Transport};
use serde_json::json;

use crate::api::{ClientInfo, escape, fail, json_request, json_response};
use crate::credentials::{Credentials, unix_ms};

/// Host opens url and polls at poll_interval; no listener or browser is started here.
pub struct LoginStart {
    pub url: String,
    pub expires_at: u64,
    pub poll_interval: Duration,
    flow_id: String,
    poll_token: String,
    next_poll: u64,
    finished: bool,
    user: Option<LoginUser>,
}

#[derive(Debug, Clone)]
pub struct LoginUser {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

impl LoginStart {
    pub fn user(&self) -> Option<&LoginUser> {
        self.user.as_ref()
    }
}

impl fmt::Debug for LoginStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginStart")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum LoginPoll {
    Pending,
    Ready(Credentials),
}

pub async fn start_login(
    transport: &dyn Transport,
    client: &ClientInfo,
) -> Result<LoginStart, Error> {
    client.validate()?;
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| fail("system randomness unavailable"))?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut request = json_request(
        "POST",
        "/api/v1/oauth/cli/init",
        client,
        Some(json!({"provider":"zai"})),
    );
    request
        .headers
        .push(("authorization".into(), format!("Bearer {token}")));
    let value = json_response(transport.send(request).await?)
        .await
        .map_err(|e| e.error)?;
    let flow_id = value["flow_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("missing login flow id"))?;
    let url = value["authorize_url"]
        .as_str()
        .ok_or_else(|| fail("missing login URL"))?;
    let uri: http::Uri = url.parse().map_err(|_| fail("invalid login URL"))?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
    {
        return Err(fail("invalid login URL"));
    }
    let expires_at = value["expires_at"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| fail("missing login expiry"))?
        .saturating_mul(1000);
    let interval = value["poll_interval_sec"]
        .as_u64()
        .filter(|s| (1..=300).contains(s))
        .ok_or_else(|| fail("invalid login poll interval"))?;
    if expires_at <= unix_ms() {
        return Err(fail("login flow already expired"));
    }
    Ok(LoginStart {
        url: url.into(),
        flow_id: flow_id.into(),
        poll_token: token,
        expires_at,
        poll_interval: Duration::from_secs(interval),
        next_poll: 0,
        finished: false,
        user: None,
    })
}

/// The host durably saves Ready credentials before attaching them to an endpoint.
pub async fn poll_login(
    transport: &dyn Transport,
    client: &ClientInfo,
    login: &mut LoginStart,
) -> Result<LoginPoll, Error> {
    client.validate()?;
    if login.finished || login.expires_at <= unix_ms() {
        return Err(fail("login flow ended; start a new login"));
    }
    if login.next_poll > unix_ms() {
        return Ok(LoginPoll::Pending);
    }
    login.next_poll = unix_ms().saturating_add(login.poll_interval.as_millis() as u64);
    let path = format!("/api/v1/oauth/cli/poll/{}", escape(&login.flow_id));
    let mut request = json_request("GET", &path, client, None);
    request.headers.push((
        "authorization".into(),
        format!("Bearer {}", login.poll_token),
    ));
    let value = json_response(transport.send(request).await?)
        .await
        .map_err(|e| e.error)?;
    match value["status"].as_str() {
        Some("pending") => Ok(LoginPoll::Pending),
        Some("ready") => {
            let token = value["token"]
                .as_str()
                .ok_or_else(|| fail("missing ZCode login JWT"))?;
            let credentials = Credentials::new(token).map_err(fail)?;
            credentials.validate().map_err(fail)?;
            login.user = value["user"]["user_id"].as_str().map(|id| LoginUser {
                id: id.to_owned(),
                email: value["user"]["email"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned),
                name: value["user"]["name"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned),
            });
            login.finished = true;
            Ok(LoginPoll::Ready(credentials))
        }
        Some("failed") => {
            login.finished = true;
            Err(fail("ZCode login failed; start a new login"))
        }
        _ => Err(fail("invalid ZCode login status")),
    }
}
