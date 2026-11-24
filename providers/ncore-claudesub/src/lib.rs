//! Claude subscription endpoint adapter. The host owns routing, runtime and durable secrets.
//! Uses native Anthropic Messages and the core's cache planner; never rewrites tool names.

mod api;
pub mod credentials;
pub mod oauth;
pub mod quota;
pub mod request;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::lock::Mutex as AsyncMutex;
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware, Next,
    RequestCtx, Transport,
};
use serde::de::DeserializeOwned;

use api::{API_BASE, CallError, Sender, fail};
pub use credentials::{CredentialStore, Credentials, MemoryStore};
pub use quota::{HeaderLimits, Profile, Snapshot, Usage};

#[derive(Debug, Clone)]
pub struct Config {
    pub usage_ttl: Duration,
    pub profile_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            usage_ttl: Duration::from_secs(65),
            profile_ttl: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Default)]
struct RefreshState {
    // Retain rotated credentials when a host save fails/is cancelled, so the next call
    // retries persistence rather than refreshing the now-invalid old refresh token.
    pending: Option<Credentials>,
    failure: Option<(String, Instant, String)>,
}

/// Bind one instance to one account's store; reuse it across that account's endpoints.
/// Choosing accounts and failover remain host policy, preserving conversation affinity.
pub struct ClaudeSub {
    store: Arc<dyn CredentialStore>,
    config: Config,
    refresh: AsyncMutex<RefreshState>,
    usage_cache: AsyncMutex<quota::Cache<Usage>>,
    profile_cache: AsyncMutex<quota::Cache<Profile>>,
    header_limits: Mutex<Option<HeaderLimits>>,
}

impl ClaudeSub {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self::with_config(store, Config::default())
    }

    pub fn with_config(store: Arc<dyn CredentialStore>, config: Config) -> Self {
        Self {
            store,
            config,
            refresh: AsyncMutex::default(),
            usage_cache: AsyncMutex::default(),
            profile_cache: AsyncMutex::default(),
            header_limits: Mutex::default(),
        }
    }

    /// Attach this same instance with `.with_plugin(plugin.clone())`.
    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::AnthropicMessages, API_BASE).with_signature_namespace("claudesub")
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Last account limit headers, including errors and the initial headers of SSE responses.
    /// No request is made. Monitor endpoint errors never block inference.
    pub fn last_limits(&self) -> Option<HeaderLimits> {
        self.header_limits.lock().unwrap().clone()
    }

    async fn token(&self, sender: Sender<'_>, rejected: Option<&str>) -> Result<String, Error> {
        let mut state = self.refresh.lock().await;
        if let Some(pending) = state.pending.clone() {
            self.store
                .save(&pending)
                .await
                .map_err(|_| fail("could not persist refreshed credentials"))?;
            state.pending = None;
        }
        let current = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load account credentials"))?;
        if current.valid() && rejected != Some(current.access_token.as_str()) {
            return Ok(current.access_token);
        }
        if let Some((token, until, message)) = &state.failure
            && token == &current.access_token
            && *until > Instant::now()
        {
            return Err(fail(message.clone()));
        }
        let fresh = match oauth::refresh(sender, &current).await {
            Ok(fresh) => fresh,
            Err(error) => {
                state.failure = Some((
                    current.access_token,
                    Instant::now() + error.retry_after,
                    api::message(&error.error),
                ));
                return Err(error.error);
            }
        };
        state.failure = None;
        state.pending = Some(fresh.clone());
        self.store
            .save(&fresh)
            .await
            .map_err(|_| fail("could not persist refreshed credentials"))?;
        state.pending = None;
        Ok(fresh.access_token)
    }

    fn observe_limits(&self, response: &HttpResponse) {
        let mut limits = self.header_limits.lock().unwrap();
        *limits = HeaderLimits::parse(&response.headers, limits.take());
    }

    async fn send(&self, mut req: HttpRequest, next: Next<'_>) -> Result<HttpResponse, Error> {
        validate_target(&req)?;
        let token = self.token(Sender::Next(next), None).await?;
        authenticate(&mut req, &token);
        let resp = next.run(req.clone()).await?;
        self.observe_limits(&resp);
        if resp.status != 401 {
            return Ok(resp);
        }
        // One refresh/replay only, before any response body or SSE event is exposed.
        drop(resp);
        let fresh = self.token(Sender::Next(next), Some(&token)).await?;
        authenticate(&mut req, &fresh);
        let resp = next.run(req).await?;
        self.observe_limits(&resp);
        Ok(resp)
    }

    async fn authorized_get(
        &self,
        transport: &dyn Transport,
        path: &str,
    ) -> Result<serde_json::Value, CallError> {
        let sender = Sender::Transport(transport);
        let token = self.token(sender, None).await?;
        let mut req = HttpRequest {
            method: "GET".into(),
            url: format!("{API_BASE}{path}"),
            headers: api::headers(&token, true),
            body: Bytes::new(),
        };
        let mut resp = sender.send(req.clone()).await?;
        if resp.status == 401 {
            drop(resp);
            let fresh = self.token(sender, Some(&token)).await?;
            req.headers = api::headers(&fresh, true);
            resp = sender.send(req).await?;
        }
        api::json_response(resp, path).await
    }

    async fn monitor<T: Clone + DeserializeOwned>(
        &self,
        transport: &dyn Transport,
        path: &str,
        cache: &AsyncMutex<quota::Cache<T>>,
        ttl: Duration,
    ) -> Result<Snapshot<T>, Error> {
        // The guard also coalesces concurrent polls. No std::sync lock is held over I/O.
        let mut cache = cache.lock().await;
        if let Some(value) = cache.current() {
            return value;
        }
        let result = match self.authorized_get(transport, path).await {
            Ok(value) => serde_json::from_value(value)
                .map_err(|_| CallError::from(fail("invalid account monitor payload"))),
            Err(error) => Err(error),
        };
        cache.update(result, ttl)
    }

    /// Five-hour, weekly, scoped and extra-usage limits. Does not generate model tokens.
    pub async fn usage(&self, transport: &dyn Transport) -> Result<Snapshot<Usage>, Error> {
        self.monitor(
            transport,
            "/api/oauth/usage",
            &self.usage_cache,
            self.config.usage_ttl,
        )
        .await
    }

    /// Subscription information, cached independently from usage and the prompt cache.
    pub async fn profile(&self, transport: &dyn Transport) -> Result<Snapshot<Profile>, Error> {
        self.monitor(
            transport,
            "/api/oauth/profile",
            &self.profile_cache,
            self.config.profile_ttl,
        )
        .await
    }
}

fn validate_target(req: &HttpRequest) -> Result<(), Error> {
    let path = req
        .url
        .strip_prefix(API_BASE)
        .and_then(|p| p.strip_prefix('/'))
        .ok_or_else(|| fail("ClaudeSub can only send to api.anthropic.com"))?;
    let path = path.split('?').next().unwrap_or(path);
    if matches!(
        (req.method.as_str(), path),
        ("POST", "v1/messages" | "v1/messages/count_tokens") | ("GET", "v1/models")
    ) {
        Ok(())
    } else {
        Err(fail("unsupported ClaudeSub API route"))
    }
}

fn authenticate(req: &mut HttpRequest, token: &str) {
    req.headers.retain(|(k, _)| {
        ![
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "user-agent",
            "content-length",
            "host",
            "cookie",
            "proxy-authorization",
        ]
        .iter()
        .any(|s| k.eq_ignore_ascii_case(s))
    });
    req.headers
        .push(("authorization".into(), format!("Bearer {token}")));
    req.headers
        .push(("user-agent".into(), api::USER_AGENT.into()));
    if !req
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
    {
        req.headers
            .push(("anthropic-version".into(), "2023-06-01".into()));
    }
}

impl Hook for ClaudeSub {
    fn name(&self) -> &str {
        "claudesub"
    }

    fn interests(&self) -> Interests {
        Interests {
            request: true,
            ..Interests::NONE
        }
    }

    fn on_request<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        req: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if ctx.format != Format::AnthropicMessages {
                return Err("ClaudeSub requires Anthropic Messages".into());
            }
            validate_target(req).map_err(|e| e.to_string())?;
            if req.method == "POST" {
                req.body = request::prepare(&req.body)?;
            }
            Ok(())
        })
    }
}

impl Middleware for ClaudeSub {
    fn name(&self) -> &str {
        "claudesub"
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
