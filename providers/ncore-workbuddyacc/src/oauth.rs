use crate::{
    ClientInfo, Credentials,
    api::{self, Sender},
    credentials::unix_ms,
};
use ncore::{Error, Transport};
use serde_json::{Value, json};

pub struct LoginStart {
    pub url: String,
    state: String,
    deadline: u64,
    pub poll_interval: std::time::Duration,
}
impl std::fmt::Debug for LoginStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginStart").finish_non_exhaustive()
    }
}
pub enum LoginPoll {
    Pending,
    Ready(Credentials),
}

pub async fn start_login(net: &dyn Transport, client: &ClientInfo) -> Result<LoginStart, Error> {
    let req = api::request(
        client,
        "POST",
        "/v2/plugin/auth/state?platform=workbuddy-ai",
        Some(json!({})),
    )?;
    let v = api::data(api::envelope(net.send(req).await.map_err(Error::from)?).await?)?;
    let url = v["authUrl"]
        .as_str()
        .ok_or_else(|| api::fail("missing login URL"))?;
    if !url.starts_with("https://www.workbuddy.ai/") {
        return Err(api::fail("unexpected WorkBuddy login origin"));
    }
    Ok(LoginStart {
        url: url.into(),
        state: v["state"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| api::fail("missing login state"))?
            .into(),
        deadline: unix_ms() + 300_000,
        poll_interval: std::time::Duration::from_secs(1),
    })
}

pub async fn poll_login(
    net: &dyn Transport,
    client: &ClientInfo,
    login: &mut LoginStart,
) -> Result<LoginPoll, Error> {
    if unix_ms() >= login.deadline {
        return Err(api::fail("WorkBuddy login expired"));
    }
    let req = api::request(
        client,
        "GET",
        &format!("/v2/plugin/auth/token?state={}", api::escape(&login.state)),
        None,
    )?;
    let v = api::envelope(net.send(req).await.map_err(Error::from)?).await?;
    if v["code"] == 11217 {
        return Ok(LoginPoll::Pending);
    }
    let token = api::data(v)?;
    let uid = token["uid"].as_str().map(str::to_owned);
    let uid = match uid {
        Some(uid) => uid,
        None => {
            let mut req = api::request(client, "GET", "/v2/plugin/accounts", None)?;
            req.headers.push((
                "authorization".into(),
                format!(
                    "Bearer {}",
                    token["accessToken"]
                        .as_str()
                        .ok_or_else(|| api::fail("missing access token"))?
                ),
            ));
            let accounts =
                api::data(api::envelope(net.send(req).await.map_err(Error::from)?).await?)?;
            accounts["accounts"]
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|a| a["lastLogin"] == true)
                        .or_else(|| a.first())
                })
                .and_then(|a| a["uid"].as_str())
                .ok_or_else(|| api::fail("missing login account uid"))?
                .into()
        }
    };
    let result = from_token(&token, &uid, None)?;
    login.deadline = 0;
    Ok(LoginPoll::Ready(result))
}

pub(crate) async fn refresh(
    sender: Sender<'_>,
    client: &ClientInfo,
    current: &Credentials,
) -> Result<Credentials, Error> {
    let token = current
        .refresh_token
        .as_ref()
        .ok_or_else(|| api::fail("reconnect WorkBuddy account: no refresh token"))?;
    if current.refresh_expires_at.is_some_and(|t| t <= unix_ms()) {
        return Err(api::fail(
            "reconnect WorkBuddy account: refresh token expired",
        ));
    }
    let mut req = api::request(
        client,
        "POST",
        "/v2/plugin/auth/token/refresh",
        Some(json!({})),
    )?;
    req.headers.push(("x-refresh-token".into(), token.clone()));
    req.headers
        .push(("x-auth-refresh-source".into(), "plugin".into()));
    let v = api::data(api::envelope(sender.send(req).await?).await?)?;
    from_token(&v, &current.uid, Some(current))
}

fn from_token(v: &Value, uid: &str, previous: Option<&Credentials>) -> Result<Credentials, Error> {
    let now = unix_ms();
    let c = Credentials {
        access_token: v["accessToken"]
            .as_str()
            .ok_or_else(|| api::fail("missing access token"))?
            .into(),
        refresh_token: v["refreshToken"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| previous.and_then(|c| c.refresh_token.clone())),
        expires_at: v["expiresAt"]
            .as_u64()
            .or_else(|| {
                v["expiresIn"]
                    .as_u64()
                    .map(|t| now.saturating_add(t.saturating_mul(1000)))
            })
            .ok_or_else(|| api::fail("missing token expiry"))?,
        refresh_expires_at: v["refreshExpiresAt"]
            .as_u64()
            .or_else(|| {
                v["refreshExpiresIn"]
                    .as_u64()
                    .map(|t| now.saturating_add(t.saturating_mul(1000)))
            })
            .or_else(|| previous.and_then(|c| c.refresh_expires_at)),
        uid: uid.into(),
        domain: v["domain"].as_str().unwrap_or("www.workbuddy.ai").into(),
    };
    c.validate().map_err(api::fail)?;
    Ok(c)
}
