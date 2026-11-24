//! ChatGPT subscription compatibility adapter for the Codex Responses backend.
//! The host owns accounts, durable storage, routing and transport.

mod api;
pub mod credentials;
pub mod oauth;
pub mod request;
mod stream;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::lock::Mutex as AsyncMutex;
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware, Next,
    RequestCtx, Transport,
};
use serde_json::Value;

use api::{API_BASE, CallError, Sender, fail};
pub use credentials::{CredentialStore, Credentials, MemoryStore};

#[derive(Default)]
struct RefreshState {
    pending: Option<Credentials>,
    failure: Option<(String, Instant)>,
}

/// Raw provider usage, including new scoped windows and credits without data loss.
#[derive(Debug, Clone)]
pub struct UsageSnapshot {
    pub value: Value,
    pub fetched_at: u64,
    pub stale: bool,
    pub refresh_error: Option<String>,
}

#[derive(Default)]
struct UsageCache {
    snapshot: Option<UsageSnapshot>,
    next: Option<Instant>,
}

/// Share one instance per account, including metadata calls, to serialize refreshes.
pub struct ChatGptSub {
    store: Arc<dyn CredentialStore>,
    refresh: AsyncMutex<RefreshState>,
    usage: AsyncMutex<UsageCache>,
    limits: Mutex<Vec<(String, String)>>,
}

impl ChatGptSub {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self {
            store,
            refresh: AsyncMutex::default(),
            usage: AsyncMutex::default(),
            limits: Mutex::default(),
        }
    }

    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::OpenAiResponses, format!("{API_BASE}/codex"))
            .with_signature_namespace("chatgptsub")
    }

    /// Latest x-codex limit headers, including HTTP errors and initial SSE headers.
    pub fn last_limits(&self) -> Vec<(String, String)> {
        self.limits.lock().unwrap().clone()
    }

    async fn credentials(
        &self,
        sender: Sender<'_>,
        rejected: Option<&str>,
    ) -> Result<Credentials, Error> {
        let mut state = self.refresh.lock().await;
        if let Some(pending) = &state.pending {
            self.store
                .save(pending)
                .await
                .map_err(|_| fail("could not persist refreshed credentials"))?;
            state.pending = None;
        }
        let current = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load account credentials"))?;
        current.validate().map_err(fail)?;
        if current.valid() && rejected != Some(current.access_token.as_str()) {
            return Ok(current);
        }
        if state
            .failure
            .as_ref()
            .is_some_and(|(token, until)| token == &current.access_token && *until > Instant::now())
        {
            return Err(fail(
                "OAuth refresh is cooling down; retry later or reconnect this account",
            ));
        }
        let fresh = match oauth::refresh(sender, &current).await {
            Ok(fresh) => fresh,
            Err(error) => {
                state.failure = Some((current.access_token, Instant::now() + error.delay));
                return Err(error.error);
            }
        };
        state.failure = None;
        // Keep a rotated token until storage succeeds, including cancellation during save.
        state.pending = Some(fresh.clone());
        self.store
            .save(&fresh)
            .await
            .map_err(|_| fail("could not persist refreshed credentials"))?;
        state.pending = None;
        Ok(fresh)
    }

    async fn authorized(
        &self,
        sender: Sender<'_>,
        mut request: HttpRequest,
    ) -> Result<HttpResponse, Error> {
        validate_target(&request)?;
        let current = self.credentials(sender, None).await?;
        authenticate(&mut request, &current);
        let mut response = sender.send(request.clone()).await?;
        self.observe(&response);
        if response.status == 401 {
            drop(response);
            let fresh = self
                .credentials(sender, Some(&current.access_token))
                .await?;
            authenticate(&mut request, &fresh);
            response = sender.send(request).await?;
            self.observe(&response);
        }
        Ok(response)
    }

    fn observe(&self, response: &HttpResponse) {
        let found: Vec<_> = response
            .headers
            .iter()
            .filter(|(k, _)| {
                let k = k.to_ascii_lowercase();
                k.starts_with("x-codex-")
                    && (k.contains("used-percent")
                        || k.contains("reset")
                        || k.contains("window-minutes"))
            })
            .cloned()
            .collect();
        if !found.is_empty() {
            *self.limits.lock().unwrap() = found;
        }
    }

    async fn get(&self, transport: &dyn Transport, path: &str) -> Result<Value, CallError> {
        let request = HttpRequest {
            method: "GET".into(),
            url: format!("{API_BASE}{path}"),
            headers: Vec::new(),
            body: Bytes::new(),
        };
        api::json_response(
            self.authorized(Sender::Transport(transport), request)
                .await?,
        )
        .await
    }

    /// Native authenticated catalog; client version is explicit, never inferred from local Codex files.
    pub async fn models(
        &self,
        transport: &dyn Transport,
        client_version: &str,
    ) -> Result<Value, Error> {
        if client_version.trim().is_empty() {
            return Err(fail("model discovery requires a client version"));
        }
        let value = self
            .get(
                transport,
                &format!(
                    "/codex/models?client_version={}",
                    api::escape(client_version)
                ),
            )
            .await
            .map_err(|e| e.error)?;
        if !value["models"].is_array() {
            return Err(fail("invalid Codex model catalog"));
        }
        Ok(value)
    }

    /// Usage only; never generates tokens. Concurrent polls share a 60-second cache.
    pub async fn usage(&self, transport: &dyn Transport) -> Result<UsageSnapshot, Error> {
        let mut cache = self.usage.lock().await;
        if cache.next.is_some_and(|next| next > Instant::now()) {
            return cache
                .snapshot
                .clone()
                .ok_or_else(|| fail("usage polling is cooling down"));
        }
        let result = self.get(transport, "/wham/usage").await.and_then(|value| {
            if value.is_object()
                && value.get("plan_type").is_some()
                && !value.get("error").is_some_and(|v| !v.is_null())
            {
                Ok(value)
            } else {
                Err(fail("invalid ChatGPT usage response").into())
            }
        });
        match result {
            Ok(value) => {
                let snapshot = UsageSnapshot {
                    value,
                    fetched_at: credentials::unix_ms(),
                    stale: false,
                    refresh_error: None,
                };
                cache.snapshot = Some(snapshot.clone());
                cache.next = Some(Instant::now() + Duration::from_secs(60));
                Ok(snapshot)
            }
            Err(error) => {
                cache.next = Some(Instant::now() + error.delay);
                if let Some(snapshot) = &mut cache.snapshot {
                    snapshot.stale = true;
                    snapshot.refresh_error = Some(error.error.to_string());
                    Ok(snapshot.clone())
                } else {
                    Err(error.error)
                }
            }
        }
    }
}

fn validate_target(request: &HttpRequest) -> Result<(), Error> {
    let valid = match request.method.as_str() {
        "POST" => request.url == format!("{API_BASE}/codex/responses"),
        "GET" => {
            request.url == format!("{API_BASE}/wham/usage")
                || request
                    .url
                    .starts_with(&format!("{API_BASE}/codex/models?client_version="))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(fail("unsupported ChatGptSub API route"))
    }
}

fn authenticate(request: &mut HttpRequest, credentials: &Credentials) {
    request.headers.retain(|(key, _)| {
        ![
            "authorization",
            "x-api-key",
            "x-goog-api-key",
            "cookie",
            "proxy-authorization",
            "host",
            "content-length",
            "content-encoding",
            "transfer-encoding",
            "user-agent",
            "chatgpt-account-id",
            "originator",
            "accept",
            "session_id",
            "session-id",
            "x-codex-turn-state",
        ]
        .iter()
        .any(|name| key.eq_ignore_ascii_case(name))
    });
    request.headers.extend([
        (
            "authorization".into(),
            format!("Bearer {}", credentials.access_token),
        ),
        ("chatgpt-account-id".into(), credentials.account_id.clone()),
        ("originator".into(), "codex_cli_rs".into()),
        (
            "user-agent".into(),
            concat!("nabucore-chatgptsub/", env!("CARGO_PKG_VERSION")).into(),
        ),
        (
            "accept".into(),
            if request.method == "POST" {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
    ]);
    if let Ok(value) = serde_json::from_slice::<Value>(&request.body)
        && let Some(key) = value["prompt_cache_key"].as_str()
        && !key.is_empty()
        && key.bytes().all(|b| (33..=126).contains(&b))
    {
        request.headers.push(("session_id".into(), key.into()));
    }
}

impl Hook for ChatGptSub {
    fn name(&self) -> &str {
        "chatgptsub"
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
        request: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if ctx.format != Format::OpenAiResponses {
                return Err("ChatGptSub requires OpenAI Responses".into());
            }
            if request.url == format!("{API_BASE}/codex/v1/responses") {
                request.url = format!("{API_BASE}/codex/responses");
            }
            validate_target(request).map_err(|e| e.to_string())?;
            if request.method == "POST" {
                request.body = request::prepare(&request.body)?;
            }
            Ok(())
        })
    }
}

impl Middleware for ChatGptSub {
    fn name(&self) -> &str {
        "chatgptsub"
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            let inference = request.method == "POST";
            let response = self.authorized(Sender::Next(next), request).await?;
            if inference {
                stream::adapt(response, ctx.streaming).await
            } else {
                Ok(response)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use ncore::TransportError;
    use serde_json::json;
    use std::collections::VecDeque;

    struct Script(Mutex<VecDeque<HttpResponse>>);

    impl Transport for Script {
        fn send(&self, _: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async { Ok(self.0.lock().unwrap().pop_front().expect("unexpected poll")) })
        }
    }

    fn response(status: u16, value: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![("retry-after".into(), "120".into())],
            body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
        }
    }

    fn plugin() -> ChatGptSub {
        ChatGptSub::new(Arc::new(MemoryStore::new(Credentials {
            access_token: "access".into(),
            refresh_token: None,
            expires_at: u64::MAX,
            account_id: "account".into(),
        })))
    }

    #[tokio::test]
    async fn usage_failure_retains_stale_snapshot_and_honors_retry_after() {
        let transport = Script(Mutex::new(VecDeque::from([
            response(200, json!({"plan_type":"plus","rate_limit":null})),
            response(429, json!({"error":"slow down"})),
        ])));
        let plugin = plugin();
        let first = plugin.usage(&transport).await.unwrap();
        plugin.usage.lock().await.next = None;
        let stale = plugin.usage(&transport).await.unwrap();
        assert!(stale.stale);
        assert_eq!(stale.value, first.value);
        assert_eq!(stale.fetched_at, first.fetched_at);
        assert!(stale.refresh_error.as_deref().unwrap().contains("429"));
        assert!(
            plugin
                .usage
                .lock()
                .await
                .next
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(115)
        );
        assert!(plugin.usage(&transport).await.unwrap().stale);
    }

    #[tokio::test]
    async fn invalid_usage_without_snapshot_is_not_treated_as_zero_usage() {
        let transport = Script(Mutex::new(VecDeque::from([response(
            200,
            json!({"error":"not usage"}),
        )])));
        let plugin = plugin();
        assert!(plugin.usage(&transport).await.is_err());
        assert!(plugin.usage(&transport).await.is_err());
    }
}
