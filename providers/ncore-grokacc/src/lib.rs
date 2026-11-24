//! Grok Build account adapter. The host owns account routing and durable storage.

mod api;
mod credentials;
pub mod oauth;
pub mod quota;
mod stream;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{lock::Mutex as AsyncMutex, stream as streams};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware, Next,
    RequestCtx, Transport,
};
use serde::Deserialize;
use serde_json::{Value, json};

use api::{BASE, Sender, fail};
pub use credentials::{CredentialStore, Credentials, MemoryStore};
use credentials::{header_token, unix_ms};
pub use quota::{Billing, FreeQuota, FreeUsage};

const RESPONSES: &str = "https://cli-chat-proxy.grok.com/v1/responses";
const CACHE_HEADER: &str = "x-nabu-grok-cache-key";

/// Version of the Grok Build protocol this adapter implements.
pub const CLIENT_VERSION: &str = "1.0.77";

#[derive(Debug, Clone)]
pub struct Client {
    version: String,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            version: CLIENT_VERSION.into(),
        }
    }
}

impl Client {
    pub fn new(version: impl Into<String>) -> Result<Self, Error> {
        let version = version.into();
        if !header_token(&version) {
            return Err(fail("invalid Grok client version"));
        }
        Ok(Self { version })
    }

    async fn profile_with(&self, sender: Sender<'_>, access: &str) -> Result<Profile, Error> {
        let mut request = api::get("/v1/user?include=subscription");
        api::authenticate(&mut request, access, &self.version);
        let profile: Profile = api::json(sender.send(request).await?).await?;
        if !header_token(&profile.user_id) {
            return Err(fail("Grok profile is missing a user id"));
        }
        Ok(profile)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub user_id: String,
    pub email: Option<String>,
    pub team_id: Option<String>,
    pub subscription_tier: Option<String>,
}

#[derive(Deserialize)]
struct Settings {
    subscription_tier_display: Option<String>,
    subscription_tier: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_efforts: Vec<ReasoningEffort>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningEffort {
    pub id: String,
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Default)]
struct Refresh {
    pending: Option<Credentials>,
    retry_at: Option<Instant>,
}

pub struct GrokAcc {
    store: Arc<dyn CredentialStore>,
    client: Client,
    refresh: AsyncMutex<Refresh>,
    free: Arc<Mutex<FreeUsage>>,
}

impl GrokAcc {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self::with_state(store, Client::default(), FreeUsage::default())
    }

    pub fn with_state(store: Arc<dyn CredentialStore>, client: Client, usage: FreeUsage) -> Self {
        Self {
            store,
            client,
            refresh: AsyncMutex::default(),
            free: Arc::new(Mutex::new(usage)),
        }
    }

    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::OpenAiResponses, BASE).with_signature_namespace("grokacc")
    }

    /// Adds a stable per-conversation cache key through GenerateOptions::headers.
    pub fn cache_key(key: impl Into<String>) -> (String, String) {
        (CACHE_HEADER.into(), key.into())
    }

    pub fn free_usage(&self) -> FreeUsage {
        self.free.lock().unwrap().clone()
    }

    pub fn free_quota(&self) -> FreeQuota {
        self.free.lock().unwrap().snapshot(unix_ms())
    }

    pub fn free_usage_reported_exhausted(&self) -> bool {
        self.free.lock().unwrap().reported_exhausted(unix_ms())
    }

    pub async fn profile(&self, net: &dyn Transport) -> Result<Profile, Error> {
        let mut profile: Profile = self.get(net, "/v1/user?include=subscription").await?;
        if !header_token(&profile.user_id) {
            return Err(fail("Grok profile is missing a user id"));
        }
        if profile.subscription_tier.is_none()
            && let Ok(settings) = self.get::<Settings>(net, "/v1/settings").await
        {
            profile.subscription_tier = settings
                .subscription_tier_display
                .or(settings.subscription_tier);
        }
        Ok(profile)
    }

    pub async fn models(&self, net: &dyn Transport) -> Result<Vec<Model>, Error> {
        #[derive(Deserialize)]
        struct Catalog {
            data: Vec<Model>,
        }

        let mut models = self.get::<Catalog>(net, "/v1/models").await?.data;
        for model in &mut models {
            if model.id == "grok-4.7" {
                model.context_window = model.context_window.min(256_000);
            }
        }
        Ok(models)
    }

    pub async fn billing(&self, net: &dyn Transport) -> Result<Billing, Error> {
        #[derive(Deserialize)]
        struct Envelope {
            config: Billing,
        }

        Ok(self
            .get::<Envelope>(net, "/v1/billing?format=credits")
            .await?
            .config)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        net: &dyn Transport,
        path: &str,
    ) -> Result<T, Error> {
        api::json(
            self.authorized(Sender::Transport(net), api::get(path))
                .await?,
        )
        .await
    }

    async fn credentials(
        &self,
        sender: Sender<'_>,
        rejected: Option<&str>,
    ) -> Result<Credentials, Error> {
        let mut refresh = self.refresh.lock().await;
        if let Some(pending) = &refresh.pending {
            self.store
                .save(pending)
                .await
                .map_err(|_| fail("could not persist Grok OAuth refresh"))?;
            refresh.pending = None;
        }
        let current = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load Grok credentials"))?;
        current.validate().map_err(fail)?;
        if current.expires_at > unix_ms().saturating_add(60_000)
            && rejected != Some(&current.access_token)
        {
            return Ok(current);
        }
        if refresh.retry_at.is_some_and(|at| at > Instant::now()) {
            return Err(fail("Grok OAuth refresh is cooling down"));
        }
        let fresh = match oauth::refresh(sender, &current).await {
            Ok(fresh) => fresh,
            Err(error) => {
                refresh.retry_at = Some(Instant::now() + Duration::from_secs(15));
                return Err(error);
            }
        };
        refresh.retry_at = None;
        refresh.pending = Some(fresh.clone());
        self.store
            .save(&fresh)
            .await
            .map_err(|_| fail("could not persist Grok OAuth refresh"))?;
        refresh.pending = None;
        Ok(fresh)
    }

    async fn authorized(
        &self,
        sender: Sender<'_>,
        mut request: HttpRequest,
    ) -> Result<HttpResponse, Error> {
        let account = self.credentials(sender, None).await?;
        self.authenticate(&mut request, &account);
        let response = sender.send(request.clone()).await?;
        if response.status != 401 {
            return Ok(response);
        }
        drop(response);
        let fresh = self
            .credentials(sender, Some(&account.access_token))
            .await?;
        self.authenticate(&mut request, &fresh);
        sender.send(request).await
    }

    fn authenticate(&self, request: &mut HttpRequest, account: &Credentials) {
        api::authorize(request, account, &self.client.version);
        if request.url == RESPONSES
            && let Ok(body) = serde_json::from_slice::<Value>(&request.body)
            && let Some(model) = body["model"].as_str().filter(|value| header_token(value))
        {
            request
                .headers
                .push(("x-grok-model-override".into(), model.into()));
        }
    }
}

impl Hook for GrokAcc {
    fn name(&self) -> &str {
        "grokacc"
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
            if ctx.format != Format::OpenAiResponses
                || request.method != "POST"
                || request.url != RESPONSES
            {
                return Err("GrokAcc requires the Grok Responses endpoint".into());
            }
            let mut value: Value =
                serde_json::from_slice(&request.body).map_err(|_| "invalid Grok request JSON")?;
            let body = value.as_object_mut().ok_or("invalid Grok request object")?;
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| header_token(value));
            if model.is_none() {
                return Err("missing or invalid Grok model".into());
            }
            if body.contains_key("previous_response_id") {
                return Err("GrokAcc requires full conversation history".into());
            }
            body.insert("stream".into(), json!(true));
            body.insert("store".into(), json!(false));
            let include = body
                .entry("include")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or("invalid Grok include list")?;
            if !include
                .iter()
                .any(|entry| entry == "reasoning.encrypted_content")
            {
                include.push(json!("reasoning.encrypted_content"));
            }
            if let Some((_, key)) = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(CACHE_HEADER))
            {
                body.insert("prompt_cache_key".into(), json!(key));
            }
            request
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case(CACHE_HEADER));
            request.body = Bytes::from(value.to_string());
            Ok(())
        })
    }
}

impl Middleware for GrokAcc {
    fn name(&self) -> &str {
        "grokacc"
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            if request.method != "POST" || request.url != RESPONSES {
                return Err(fail("unsupported Grok inference route"));
            }
            let mut response = self.authorized(Sender::Next(next), request).await?;
            if response.status == 429 {
                let headers = response.headers.clone();
                let body = api::read(response).await?;
                if let Ok(error) = serde_json::from_slice::<Value>(&body) {
                    self.free.lock().unwrap().reconcile(unix_ms(), &error);
                }
                response = HttpResponse {
                    status: 429,
                    headers,
                    body: Box::pin(streams::once(async { Ok(body) })),
                };
            }
            if !(200..300).contains(&response.status) {
                return Ok(response);
            }
            let free = Arc::clone(&self.free);
            stream::adapt(response, ctx.streaming, move |value| {
                if let Some(tokens) = value["response"]["usage"]["total_tokens"].as_u64() {
                    free.lock().unwrap().record(unix_ms(), tokens);
                }
            })
            .await
        })
    }
}
