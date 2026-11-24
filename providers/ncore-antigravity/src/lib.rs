pub mod account;
pub mod consts;
pub mod oauth;
pub mod quota;
pub mod request;
pub mod schema;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ncore::core::SseEvent;
use ncore::core::json::{self, JsonPath};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, EventAction, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware,
    Next, RequestCtx,
};
use serde_json::Value;

pub use account::Account;
use consts::{ENDPOINT_DAILY, ENDPOINT_PROD, Family, GEMINI_MAX_OUTPUT_TOKENS, client_headers};
use oauth::Sender;

const NAME: &str = "antigravity";
const MAX_STICKY: usize = 1000;

/// Tunables; defaults match ReProxy.
#[derive(Debug, Clone)]
pub struct Config {
    /// Cloud Code hosts tried in order for each account.
    pub endpoints: Vec<String>,
    /// Cap on Gemini `maxOutputTokens`; `None` leaves it to the backend.
    pub gemini_max_output_tokens: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            endpoints: vec![ENDPOINT_DAILY.into(), ENDPOINT_PROD.into()],
            gemini_max_output_tokens: Some(GEMINI_MAX_OUTPUT_TOKENS),
        }
    }
}

struct Token {
    value: String,
    expires: Instant,
}

#[derive(Default)]
struct State {
    next_account: usize,
    tokens: HashMap<String, Token>,
    cooldowns: HashMap<(String, String), Instant>,
    /// Account each conversation last went through; thinking signatures only verify there.
    sticky: HashMap<String, String>,
}

/// Serves Gemini-format requests through Google Cloud Code with OAuth accounts and failover.
pub struct Antigravity {
    accounts: Vec<Account>,
    config: Config,
    state: Mutex<State>,
}

/// Statuses after which the next endpoint or account is tried.
pub fn is_failover(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 408 | 429) || status >= 500
}

pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error::Plugin {
        name: NAME.into(),
        message: message.into(),
    }
}

impl Antigravity {
    pub fn new(accounts: Vec<Account>) -> Self {
        Self::with_config(accounts, Config::default())
    }

    pub fn with_config(accounts: Vec<Account>, config: Config) -> Self {
        Self {
            accounts: accounts.into_iter().filter(|a| a.enabled).collect(),
            config,
            state: Mutex::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Gemini endpoint; the base URL is a placeholder, the middleware picks the real host.
    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::Gemini, ENDPOINT_PROD).with_signature_namespace(NAME)
    }

    /// The conversation's own account first, then round-robin so load spreads across accounts.
    fn ordered(&self, model: &str, conversation: &str) -> Vec<&Account> {
        let n = self.accounts.len();
        let start = {
            let mut s = self.state.lock().unwrap();
            let start = s.next_account % n.max(1);
            s.next_account = (start + 1) % n.max(1);
            start
        };
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.cooldowns.retain(|_, until| *until > now);
        let sticky = state.sticky.get(conversation).cloned();
        let mut ordered: Vec<&Account> = (0..n)
            .map(|i| &self.accounts[(start + i) % n])
            .filter(|a| {
                !state
                    .cooldowns
                    .contains_key(&(a.email.clone(), String::new()))
                    && !state
                        .cooldowns
                        .contains_key(&(a.email.clone(), model.to_owned()))
            })
            .collect();
        if let Some(i) = ordered.iter().position(|a| Some(&a.email) == sticky.as_ref()) {
            let own = ordered.remove(i);
            ordered.insert(0, own);
        }
        ordered
    }

    fn remember(&self, conversation: &str, email: &str) {
        let mut state = self.state.lock().unwrap();
        if state.sticky.len() >= MAX_STICKY && !state.sticky.contains_key(conversation) {
            state.sticky.clear();
        }
        state.sticky.insert(conversation.to_owned(), email.to_owned());
    }

    fn cool_down(&self, email: &str, model: &str, duration: Duration) {
        let key = (email.to_owned(), model.to_owned());
        let until = Instant::now() + duration;
        let mut state = self.state.lock().unwrap();
        let slot = state.cooldowns.entry(key).or_insert(until);
        *slot = (*slot).max(until);
    }

    fn retry_delay(response: &HttpResponse) -> Duration {
        response
            .header("retry-after")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| Duration::from_secs_f64(v.clamp(1.0, 300.0)))
            .unwrap_or(Duration::from_secs(30))
    }

    async fn token(&self, next: Next<'_>, account: &Account) -> Result<String, Error> {
        if let Some(t) = self.state.lock().unwrap().tokens.get(&account.email)
            && t.expires > Instant::now()
        {
            return Ok(t.value.clone());
        }
        let fresh = oauth::refresh(Sender::Next(next), account.oauth_refresh_token()).await?;
        let margin = Duration::from_secs(60);
        let ttl = fresh
            .expires_in
            .saturating_sub(margin)
            .max(Duration::from_secs(30));
        self.state.lock().unwrap().tokens.insert(
            account.email.clone(),
            Token {
                value: fresh.token.clone(),
                expires: Instant::now() + ttl,
            },
        );
        Ok(fresh.token)
    }

    fn invalidate(&self, email: &str) {
        self.state.lock().unwrap().tokens.remove(email);
    }

    fn headers(token: &str, model: &str, session: &str, streaming: bool) -> Vec<(String, String)> {
        let mut h = client_headers();
        h.push(("authorization".into(), format!("Bearer {token}")));
        h.push(("x-machine-session-id".into(), session.into()));
        if streaming {
            h.push(("accept".into(), "text/event-stream".into()));
        }
        if consts::family(model) == Family::Claude && consts::is_thinking(model) {
            h.push((
                "anthropic-beta".into(),
                "interleaved-thinking-2025-05-14".into(),
            ));
        }
        h
    }

    async fn send(&self, req: HttpRequest, next: Next<'_>) -> Result<HttpResponse, Error> {
        let (model, method) = request::parse_gemini_url(&req.url)
            .ok_or_else(|| fail(format!("not a Gemini model URL: {}", req.url)))?;
        let streaming = match method.as_str() {
            "streamGenerateContent" => true,
            "generateContent" => false,
            other => return Err(fail(format!("unsupported method {other}"))),
        };
        let google: Value =
            serde_json::from_slice(&req.body).map_err(|e| fail(format!("request body: {e}")))?;
        if self.accounts.is_empty() {
            return Err(fail("no accounts configured"));
        }
        if self.config.endpoints.is_empty() {
            return Err(fail("no endpoints configured"));
        }
        let query = if streaming { "?alt=sse" } else { "" };
        let mut last: Option<HttpResponse> = None;
        let mut last_err = None;
        let conversation = request::conversation(&google);
        for account in self.ordered(&model, &conversation) {
            let token = match self.token(next, account).await {
                Ok(t) => t,
                Err(e) => {
                    self.cool_down(&account.email, "", Duration::from_secs(30));
                    last_err = Some(e);
                    continue;
                }
            };
            let project = account
                .project()
                .unwrap_or_else(|| consts::DEFAULT_PROJECT_ID.into());
            let session = request::session_id(&google, &account.email);
            let body = request::envelope(google.clone(), &model, &project, &session);
            let body = Bytes::from(body.to_string());
            for base in &self.config.endpoints {
                let attempt = HttpRequest {
                    method: "POST".into(),
                    url: format!("{}/v1internal:{method}{query}", base.trim_end_matches('/')),
                    headers: Self::headers(&token, &model, &session, streaming),
                    body: body.clone(),
                };
                let resp = match next.run(attempt).await {
                    Ok(r) => r,
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                };
                if resp.status < 400 {
                    self.remember(&conversation, &account.email);
                    return Ok(resp);
                }
                let status = resp.status;
                if !is_failover(status) {
                    return Ok(resp);
                }
                let delay = Self::retry_delay(&resp);
                last = Some(resp);
                if matches!(status, 401 | 402 | 403 | 429) {
                    if status == 401 {
                        self.invalidate(&account.email);
                    }
                    self.cool_down(
                        &account.email,
                        if matches!(status, 401 | 402) {
                            ""
                        } else {
                            &model
                        },
                        delay,
                    );
                    break;
                }
            }
            self.cool_down(&account.email, &model, Duration::from_secs(5));
        }
        match (last, last_err) {
            (Some(r), _) => Ok(r),
            (None, Some(e)) => Err(e),
            (None, None) => {
                let body = Bytes::from_static(br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"All accounts are temporarily unavailable"}}"#);
                let state = self.state.lock().unwrap();
                let retry = state
                    .cooldowns
                    .iter()
                    .filter(|((_, m), _)| m.is_empty() || m == &model)
                    .map(|(_, until)| {
                        until
                            .saturating_duration_since(Instant::now())
                            .as_secs()
                            .saturating_add(1)
                    })
                    .min()
                    .unwrap_or(1);
                Ok(HttpResponse {
                    status: 429,
                    headers: vec![
                        ("retry-after".into(), retry.to_string()),
                        ("content-type".into(), "application/json".into()),
                    ],
                    body: Box::pin(futures_util::stream::once(async move { Ok(body) })),
                })
            }
        }
    }
}

// Cloud Code wraps every payload as `{"response": {...}}`; this returns the inner span.
fn unwrap_response(b: &[u8]) -> Option<std::ops::Range<usize>> {
    let path = JsonPath::parse("/response").ok()?;
    let span = json::find(b, &path).ok()??;
    (b.get(span.start) == Some(&b'{')).then_some(span.start..span.end)
}

impl Hook for Antigravity {
    fn name(&self) -> &str {
        NAME
    }

    fn interests(&self) -> Interests {
        Interests {
            request: true,
            body: true,
            stream: true,
            ..Interests::NONE
        }
    }

    fn on_request<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        req: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            let (model, _) = request::parse_gemini_url(&req.url)
                .ok_or_else(|| format!("not a Gemini model URL: {}", req.url))?;
            let mut body: Value =
                serde_json::from_slice(&req.body).map_err(|e| format!("request body: {e}"))?;
            request::prepare(&mut body, &model, self.config.gemini_max_output_tokens);
            req.body = Bytes::from(body.to_string());
            Ok(())
        })
    }

    fn on_body(&self, _ctx: &RequestCtx, status: u16, body: &mut Bytes) -> Result<(), String> {
        if status < 400
            && let Some(r) = unwrap_response(body)
        {
            *body = body.slice(r);
        }
        Ok(())
    }

    fn on_stream_event(&self, _ctx: &RequestCtx, ev: &mut SseEvent) -> Result<EventAction, String> {
        if ev.has_data()
            && let Some(r) = unwrap_response(ev.data.as_bytes())
        {
            let inner = ev.data[r].to_owned();
            ev.set_data(&inner);
        }
        Ok(EventAction::Keep)
    }
}

impl Middleware for Antigravity {
    fn name(&self) -> &str {
        NAME
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        req: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(self.send(req, next))
    }
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    fn plugin() -> Antigravity {
        Antigravity::new(vec![Account {
            email: "a".into(),
            refresh_token: "r".into(),
            project_id: None,
            enabled: true,
        }])
    }

    #[test]
    fn model_cooldowns_expire_and_do_not_block_other_models() {
        let plugin = plugin();
        plugin.cool_down("a", "opus", Duration::from_secs(30));
        assert!(plugin.ordered("opus", "").is_empty());
        assert_eq!(plugin.ordered("flash", "").len(), 1);
        plugin
            .state
            .lock()
            .unwrap()
            .cooldowns
            .insert(("a".into(), "opus".into()), Instant::now());
        assert_eq!(plugin.ordered("opus", "").len(), 1);
        plugin.cool_down("a", "", Duration::from_secs(30));
        assert!(plugin.ordered("flash", "").is_empty());
    }

    #[test]
    fn conversation_keeps_its_account_while_it_is_available() {
        let account = |email: &str| Account {
            email: email.into(),
            refresh_token: "r".into(),
            project_id: None,
            enabled: true,
        };
        let plugin = Antigravity::new(vec![account("a"), account("b"), account("c")]);
        plugin.remember("chat", "b");
        for _ in 0..3 {
            assert_eq!(plugin.ordered("opus", "chat")[0].email, "b");
        }
        plugin.cool_down("b", "opus", Duration::from_secs(30));
        let ordered = plugin.ordered("opus", "chat");
        assert_eq!(ordered.len(), 2);
        assert!(ordered.iter().all(|a| a.email != "b"));
    }

    #[test]
    fn retry_after_is_bounded_and_nonfinite_values_are_ignored() {
        for (value, seconds) in [("2.5", 2.5), ("0", 1.0), ("999999", 300.0), ("NaN", 30.0)] {
            let response = HttpResponse {
                status: 429,
                headers: vec![("retry-after".into(), value.into())],
                body: Box::pin(futures_util::stream::empty()),
            };
            assert_eq!(
                Antigravity::retry_delay(&response),
                Duration::from_secs_f64(seconds)
            );
        }
    }
}
