mod api;
pub mod catalog;
pub mod credentials;
pub mod oauth;
mod request;
mod usage;
pub use usage::RequestUsage;

pub use api::ClientInfo;
use api::{Sender, fail};
use bytes::Bytes;
pub use catalog::{Access, Catalog, Credits, Model, Reasoning};
pub use credentials::{CredentialStore, Credentials, MemoryStore};
use futures_util::{StreamExt, lock::Mutex};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, EventAction, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware,
    Next, RequestCtx, Transport,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct RefreshState {
    pending: Option<Credentials>,
    failure: Option<(String, Instant)>,
}

struct Configuration {
    uid: String,
    fetched: Instant,
    value: Value,
}

pub struct WorkBuddyAcc {
    store: Arc<dyn CredentialStore>,
    client: ClientInfo,
    refresh: Mutex<RefreshState>,
    configuration: Mutex<Option<Configuration>>,
    usage: std::sync::Mutex<Option<RequestUsage>>,
    salt: [u8; 16],
}

pub struct AccountSnapshot {
    pub catalog: Catalog,
    pub credits: Credits,
}
impl WorkBuddyAcc {
    pub fn new(store: Arc<dyn CredentialStore>, client: ClientInfo) -> Result<Self, Error> {
        client.headers()?;
        let mut salt = [0; 16];
        getrandom::fill(&mut salt).map_err(|_| fail("random source unavailable"))?;
        Ok(Self {
            store,
            client,
            refresh: Mutex::default(),
            configuration: Mutex::default(),
            usage: std::sync::Mutex::default(),
            salt,
        })
    }
    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::OpenAiChat, api::ORIGIN).with_signature_namespace("workbuddyacc")
    }
    pub fn encode_options() -> ncore::core::codec::EncodeOptions {
        ncore::core::codec::EncodeOptions {
            chat_reasoning_field: Some("reasoning".into()),
            chat_max_tokens_field: ncore::core::codec::MaxTokensField::MaxTokens,
            ..Default::default()
        }
    }
    pub fn last_usage(&self) -> Option<RequestUsage> {
        self.usage.lock().unwrap().clone()
    }
    fn observe_usage(&self, ctx: &RequestCtx, body: &[u8]) {
        if let Ok(value) = serde_json::from_slice::<Value>(body)
            && value["usage"].is_object()
        {
            *self.usage.lock().unwrap() = Some(RequestUsage {
                request_id: ctx.id,
                model: value["model"].as_str().map(str::to_owned),
                raw: value["usage"].clone(),
            });
        }
    }
    async fn configuration(&self, sender: Sender<'_>, force: bool) -> Result<Value, Error> {
        let credentials = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load WorkBuddy credentials"))?;
        credentials.validate().map_err(fail)?;
        let mut cache = self.configuration.lock().await;
        if !force
            && let Some(cache) = cache.as_ref()
            && cache.uid == credentials.uid
            && cache.fetched.elapsed() < Duration::from_secs(60)
        {
            return Ok(cache.value.clone());
        }
        let req = api::request(&self.client, "GET", "/v3/config", None)?;
        let value = api::data(api::envelope(self.authorized(sender, req, false).await?).await?)?;
        if !value["models"].is_array() {
            return Err(fail("missing WorkBuddy models"));
        }
        *cache = Some(Configuration {
            uid: credentials.uid,
            fetched: Instant::now(),
            value: value.clone(),
        });
        Ok(value)
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
            .map_err(|_| fail("could not load WorkBuddy credentials"))?;
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
                "WorkBuddy refresh is cooling down; reconnect if the session was revoked",
            ));
        }
        let fresh = match oauth::refresh(sender, &self.client, &current).await {
            Ok(fresh) => fresh,
            Err(e) => {
                state.failure = Some((
                    current.access_token,
                    Instant::now() + Duration::from_secs(10),
                ));
                return Err(e);
            }
        };
        state.failure = None;
        state.pending = Some(fresh.clone());
        self.store
            .save(&fresh)
            .await
            .map_err(|_| fail("could not persist refreshed credentials"))?;
        state.pending = None;
        Ok(fresh)
    }
    fn authenticate(
        &self,
        req: &mut HttpRequest,
        c: &Credentials,
        chat: bool,
    ) -> Result<(), Error> {
        req.headers = self.client.headers()?;
        req.headers.extend([
            ("authorization".into(), format!("Bearer {}", c.access_token)),
            ("x-user-id".into(), c.uid.clone()),
        ]);
        if chat {
            req.headers.extend([
                ("x-agent-intent".into(), "craft".into()),
                ("x-agent-purpose".into(), "conversation".into()),
                ("x-agent-type".into(), "main".into()),
            ]);
            req.headers
                .extend(request::correlation(&req.body, &self.salt)?);
        }
        Ok(())
    }
    async fn authorized(
        &self,
        sender: Sender<'_>,
        mut req: HttpRequest,
        chat: bool,
    ) -> Result<HttpResponse, Error> {
        let c = self.credentials(sender, None).await?;
        self.authenticate(&mut req, &c, chat)?;
        let response = sender.send(req.clone()).await?;
        if response.status != 401 {
            return Ok(response);
        }
        drop(response);
        let c = self.credentials(sender, Some(&c.access_token)).await?;
        self.authenticate(&mut req, &c, chat)?;
        sender.send(req).await
    }
    async fn account_data(
        &self,
        net: &dyn Transport,
        method: &str,
        path: &str,
    ) -> Result<Value, Error> {
        let req = api::request(
            &self.client,
            method,
            path,
            (method == "POST").then(|| json!({})),
        )?;
        api::data(api::envelope(self.authorized(Sender::Transport(net), req, false).await?).await?)
    }
    pub async fn credits(&self, net: &dyn Transport) -> Result<Credits, Error> {
        let raw = self
            .account_data(net, "POST", "/billing/meter/get-user-resource-summary")
            .await?;
        if !raw["Packages"].is_array() || raw["TotalCredits"].as_str().is_none() {
            return Err(fail("invalid credit summary"));
        }
        Ok(Credits {
            raw,
            fetched_at: credentials::unix_ms(),
        })
    }
    pub async fn models(&self, net: &dyn Transport) -> Result<Catalog, Error> {
        Ok(self.snapshot(net).await?.catalog)
    }
    pub async fn snapshot(&self, net: &dyn Transport) -> Result<AccountSnapshot, Error> {
        let config = self.configuration(Sender::Transport(net), true).await?;
        let credits = self.credits(net).await?;
        let catalog = Catalog::from_config(config, Some(&credits), credentials::unix_ms()).map_err(fail)?;
        Ok(AccountSnapshot {catalog,credits})
    }
    pub async fn profile(&self, net: &dyn Transport) -> Result<Value, Error> {
        let current = self.store.load().await.map_err(|_|fail("could not load WorkBuddy credentials"))?;
        current.validate().map_err(fail)?;
        let value = self.account_data(net,"GET","/v2/plugin/accounts").await?;
        value["accounts"].as_array().and_then(|accounts|accounts.iter().find(|a|a["uid"]==current.uid)).cloned().ok_or_else(||fail("WorkBuddy account profile not found"))
    }
}

impl Hook for WorkBuddyAcc {
    fn name(&self) -> &str {
        "workbuddyacc"
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
        ctx: &'a RequestCtx,
        req: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if ctx.format != Format::OpenAiChat {
                return Err("WorkBuddyAcc requires OpenAI Chat".into());
            }
            if req.url == format!("{}/v1/chat/completions", api::ORIGIN) {
                req.url = api::CHAT_URL.into();
            }
            if req.method != "POST" || req.url != api::CHAT_URL {
                return Err("WorkBuddyAcc supports only the native chat endpoint".into());
            }
            req.body = request::prepare(&req.body)?;
            Ok(())
        })
    }
    fn on_body(&self, ctx: &RequestCtx, _: u16, body: &mut Bytes) -> Result<(), String> {
        request::normalize_event(body);
        self.observe_usage(ctx, body);
        Ok(())
    }
    fn on_stream_event(
        &self,
        ctx: &RequestCtx,
        event: &mut ncore::core::SseEvent,
    ) -> Result<EventAction, String> {
        let mut bytes = Bytes::copy_from_slice(event.data.as_bytes());
        request::normalize_event(&mut bytes);
        self.observe_usage(ctx, &bytes);
        if bytes.as_ref() != event.data.as_bytes() {
            event.set_data(std::str::from_utf8(&bytes).map_err(|_| "invalid error encoding")?);
        }
        Ok(EventAction::Keep)
    }
}
impl Middleware for WorkBuddyAcc {
    fn name(&self) -> &str {
        "workbuddyacc"
    }
    fn handle<'a>(
        &'a self,
        _: &'a RequestCtx,
        mut req: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            if req.method != "POST" || req.url != api::CHAT_URL {
                return Err(fail("unsupported WorkBuddy endpoint"));
            }
            let mut body: Value =
                serde_json::from_slice(&req.body).map_err(|_| fail("invalid chat JSON"))?;
            let streaming = body["stream"] == true;
            if request::has_reasoning_options(&body) {
                let config = self.configuration(Sender::Next(next), false).await?;
                request::prepare_reasoning(&mut body, &config).map_err(fail)?;
            }
            body["stream"] = true.into();
            req.body = Bytes::from(body.to_string());
            let response = self.authorized(Sender::Next(next), req, true).await?;
            if streaming || response.status >= 400 {
                return Ok(response);
            }
            collect(response).await
        })
    }
}

async fn collect(mut response: HttpResponse) -> Result<HttpResponse, Error> {
    use ncore::core::{SseParser, codec, ir::Accumulator};
    if !response
        .header("content-type")
        .is_some_and(|s| s.contains("text/event-stream"))
    {
        return Ok(response);
    }
    let mut parser = SseParser::new();
    let mut decoder = codec::stream_decoder(Format::OpenAiChat);
    let mut acc = Accumulator::new();
    let mut size = 0usize;
    let mut native_usage = Value::Null;
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        size = size.saturating_add(chunk.len());
        if size > 32 * 1024 * 1024 {
            return Err(fail("WorkBuddy stream exceeds aggregation limit"));
        }
        for mut event in parser.feed(&chunk) {
            let mut bytes = Bytes::copy_from_slice(event.data.as_bytes());
            request::normalize_event(&mut bytes);
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if native_usage.is_null() && value["usage"].is_object() {
                    native_usage = json!({});
                }
                usage::merge(&mut native_usage, &value["usage"]);
            }
            if bytes.as_ref() != event.data.as_bytes() {
                event.set_data(std::str::from_utf8(&bytes).map_err(|_| fail("invalid SSE"))?);
            }
            for item in decoder.feed(&event).map_err(|e| fail(e.0))? {
                acc.push(&item);
            }
        }
    }
    if acc.error().is_some() {
        return Err(fail("WorkBuddy stream reported an error"));
    }
    if !acc.is_done() {
        return Err(fail("WorkBuddy stream ended without completion"));
    }
    let (body, _) = codec::encode_response(Format::OpenAiChat, &acc.finish());
    let mut value: Value =
        serde_json::from_slice(&body).map_err(|_| fail("invalid collected response"))?;
    if native_usage.is_object() {
        value["usage"] = native_usage;
    } else {
        value.as_object_mut().unwrap().remove("usage");
    }
    let body = Bytes::from(value.to_string());
    response.headers.retain(|(name, _)| {
        !matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "content-length" | "content-encoding"
        )
    });
    response
        .headers
        .push(("content-type".into(), "application/json".into()));
    response.body = Box::pin(futures_util::stream::once(async move { Ok(body) }));
    Ok(response)
}
