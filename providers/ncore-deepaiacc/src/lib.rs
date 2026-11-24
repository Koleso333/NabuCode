mod api;
pub mod catalog;
pub mod credentials;
mod lane;
pub mod oauth;
pub mod quota;
mod request;
mod stream;
mod usage;

pub use api::ClientInfo;
pub use catalog::{Catalog, Model, ModelApi, Reasoning};
pub use credentials::{CheckIn, CredentialStore, Credentials, MemoryStore};
pub use lane::{DEFAULT_LANE, LaneStore, Lanes, MAX_LANE, MemoryLaneStore};
pub use quota::{Allowance, Amount};
pub use request::Session;
pub use usage::RequestUsage;

use api::{Sender, fail};
use bytes::Bytes;
use futures_util::{StreamExt, lock::Mutex};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, EventAction, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware,
    Next, RequestCtx, Transport,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(PartialEq)]
struct Binding {
    account: String,
    machine: String,
    instance: String,
}

impl Binding {
    fn new(credentials: &Credentials) -> Self {
        Self {
            account: credentials.account_id(),
            machine: credentials.machine_digest(),
            instance: credentials.instance.clone(),
        }
    }
}

#[derive(Default)]
struct State {
    binding: Option<Binding>,
    pending: Option<Credentials>,
    allowance: Option<Allowance>,
    checked_at: Option<Instant>,
    catalog: Option<Catalog>,
    catalog_at: Option<Instant>,
}

pub struct DeepAIAcc {
    store: Arc<dyn CredentialStore>,
    client: ClientInfo,
    started: Instant,
    lanes: Arc<Lanes>,
    state: Mutex<State>,
    slot: Arc<Mutex<()>>,
    usage: std::sync::Mutex<Option<RequestUsage>>,
}

#[derive(Clone, Debug)]
pub struct AccountSnapshot {
    pub catalog: Catalog,
    pub allowance: Allowance,
}

impl DeepAIAcc {
    pub fn new(store: Arc<dyn CredentialStore>, client: ClientInfo) -> Result<Self, Error> {
        client.validate().map_err(fail)?;
        Ok(Self {
            store,
            client,
            started: Instant::now(),
            lanes: Arc::new(Lanes::default()),
            state: Mutex::default(),
            slot: Arc::new(Mutex::new(())),
            usage: std::sync::Mutex::default(),
        })
    }

    pub fn with_lanes(mut self, lanes: Arc<Lanes>) -> Self {
        self.lanes = lanes;
        self
    }

    pub async fn lane(&self) -> Result<u32, Error> {
        self.lanes.current().await
    }

    pub fn endpoint() -> Endpoint {
        Endpoint::new(Format::OpenAiChat, api::BASE)
            .with_signature_namespace("deepaiacc")
            .without_replays()
    }

    pub fn responses_endpoint() -> Endpoint {
        Endpoint::new(Format::OpenAiResponses, api::BASE)
            .with_signature_namespace("deepaiacc")
            .without_replays()
    }

    pub fn encode_options() -> ncore::core::codec::EncodeOptions {
        ncore::core::codec::EncodeOptions {
            chat_reasoning_field: Some("reasoning_content".into()),
            ..Default::default()
        }
    }

    pub fn last_usage(&self) -> Option<RequestUsage> {
        self.usage.lock().unwrap().clone()
    }

    async fn load(&self, state: &mut State) -> Result<Credentials, Error> {
        let current = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load DeepAI credentials"))?;
        current.validate().map_err(fail)?;
        let binding = Binding::new(&current);
        if state
            .binding
            .as_ref()
            .is_some_and(|saved| *saved != binding)
        {
            return Err(fail(
                "DeepAI account identity changed; create a new plugin instance",
            ));
        }
        state.binding = Some(binding);
        if let Some(pending) = state.pending.as_ref() {
            self.store.save(pending).await.map_err(|_| {
                fail("could not persist DeepAI check-in; no model request was sent")
            })?;
            let pending = state.pending.take().unwrap();
            return Ok(pending);
        }
        Ok(current)
    }

    async fn persist(&self, state: &mut State, credentials: &Credentials) -> Result<(), Error> {
        state.pending = Some(credentials.clone());
        self.store
            .save(credentials)
            .await
            .map_err(|_| fail("could not persist DeepAI check-in; no model request was sent"))?;
        state.pending = None;
        Ok(())
    }

    fn due(state: &State, credentials: &Credentials) -> bool {
        let Some(mark) = &credentials.check_in else {
            return true;
        };
        let Some(checked_at) = state.checked_at else {
            return true;
        };
        let margin = (mark.seconds * 0.1).min(30.0);
        state.allowance.is_none() || checked_at.elapsed().as_secs_f64() >= mark.seconds - margin
    }

    fn live_mark(state: &State, credentials: &Credentials) -> bool {
        let Some(mark) = &credentials.check_in else {
            return false;
        };
        if let Some(checked_at) = state.checked_at {
            return checked_at.elapsed().as_secs_f64() < mark.seconds;
        }
        credentials::unix_ms()
            .checked_sub(mark.checked_at)
            .is_some_and(|elapsed| elapsed as f64 / 1000.0 < mark.seconds)
    }

    async fn renew(
        &self,
        sender: Sender<'_>,
        state: &mut State,
        credentials: &mut Credentials,
    ) -> Result<Allowance, Error> {
        state.allowance = None;
        let mut req = api::request(credentials, "/theos", credentials.lane)?;
        req.headers.push((
            "x-deepai-code-beacon".into(),
            self.client.beacon(self.started.elapsed().as_secs()),
        ));
        if let Some(mark) = &credentials.check_in {
            req.headers
                .push(("x-deepai-code-check-in".into(), mark.nonce.clone()));
        }
        let (status, value) = api::json_response(sender.send(req).await?).await?;
        let mark = quota::parse_check_in(&value, credentials::unix_ms()).map_err(fail)?;
        let success = (200..300).contains(&status)
            && !value.get("error").is_some_and(|error| !error.is_null());
        let allowance = if success {
            Some(quota::parse_allowance(&value).map_err(fail)?)
        } else {
            None
        };
        if allowance
            .as_ref()
            .and_then(|a| a.lane_version)
            .is_some_and(|lane| lane != credentials.lane)
        {
            return Err(fail("DeepAI returned a different lane version"));
        }
        if allowance.as_ref().is_some_and(|a| a.enabled)
            && mark.is_none()
            && !Self::live_mark(state, credentials)
        {
            return Err(fail("DeepAI did not supply live check-in state"));
        }
        if let Some(mark) = mark {
            credentials.check_in = Some(mark);
            state.checked_at = Some(Instant::now());
            self.persist(state, credentials).await?;
        }
        if !success {
            return Err(Error::Provider(api::provider_error(Some(status), &value)));
        }
        let allowance = allowance.unwrap();
        state.allowance = Some(allowance.clone());
        Ok(allowance)
    }

    fn align_lane(state: &mut State, credentials: &mut Credentials, lane: u32) {
        if credentials.lane != lane {
            credentials.lane = lane;
            credentials.check_in = None;
            state.allowance = None;
            state.checked_at = None;
            state.catalog = None;
            state.catalog_at = None;
        }
    }

    async fn recover_lane(
        &self,
        sender: Sender<'_>,
        credentials: &Credentials,
    ) -> Result<u32, Error> {
        self.lanes
            .recover(
                sender,
                credentials,
                &self.client,
                self.started.elapsed().as_secs(),
                credentials.lane,
            )
            .await
    }

    async fn ready(
        &self,
        sender: Sender<'_>,
        state: &mut State,
        force: bool,
    ) -> Result<Credentials, Error> {
        let mut credentials = self.load(state).await?;
        Self::align_lane(state, &mut credentials, self.lanes.current().await?);
        if (force || Self::due(state, &credentials))
            && let Err(error) = self.renew(sender, state, &mut credentials).await
        {
            if !lane::unsupported(&error) {
                return Err(api::account_error("/theos", error));
            }
            let lane = self.recover_lane(sender, &credentials).await?;
            Self::align_lane(state, &mut credentials, lane);
            self.renew(sender, state, &mut credentials)
                .await
                .map_err(|error| api::account_error("/theos", error))?;
        }
        Ok(credentials)
    }

    async fn catalog(
        &self,
        sender: Sender<'_>,
        state: &mut State,
        credentials: &mut Credentials,
        force: bool,
    ) -> Result<Catalog, Error> {
        match self.fetch_catalog(sender, state, credentials, force).await {
            Ok(catalog) => Ok(catalog),
            Err(error) if lane::unsupported(&error) => {
                let lane = self.recover_lane(sender, credentials).await?;
                Self::align_lane(state, credentials, lane);
                self.renew(sender, state, credentials)
                    .await
                    .map_err(|error| api::account_error("/theos", error))?;
                self.fetch_catalog(sender, state, credentials, true)
                    .await
                    .map_err(|error| api::account_error("/models", error))
            }
            Err(error) => Err(api::account_error("/models", error)),
        }
    }

    async fn fetch_catalog(
        &self,
        sender: Sender<'_>,
        state: &mut State,
        credentials: &Credentials,
        force: bool,
    ) -> Result<Catalog, Error> {
        if !force
            && state
                .catalog_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
            && let Some(catalog) = &state.catalog
        {
            return Ok(catalog.clone());
        }
        let req = api::request(credentials, "/models", credentials.lane)?;
        let (status, value) = api::json_response(sender.send(req).await?).await?;
        if !(200..300).contains(&status) || value.get("error").is_some_and(|error| !error.is_null())
        {
            return Err(Error::Provider(api::provider_error(Some(status), &value)));
        }
        let catalog = Catalog::from_json(value, credentials::unix_ms()).map_err(fail)?;
        state.catalog = Some(catalog.clone());
        state.catalog_at = Some(Instant::now());
        Ok(catalog)
    }

    pub async fn check_in(&self, net: &dyn Transport) -> Result<Allowance, Error> {
        let mut state = self.state.lock().await;
        self.ready(Sender::Transport(net), &mut state, true).await?;
        Ok(state.allowance.clone().unwrap())
    }

    pub async fn credits(&self, net: &dyn Transport) -> Result<Allowance, Error> {
        self.check_in(net).await
    }

    pub async fn models(&self, net: &dyn Transport) -> Result<Catalog, Error> {
        let mut state = self.state.lock().await;
        let sender = Sender::Transport(net);
        let mut credentials = self.ready(sender, &mut state, false).await?;
        self.catalog(sender, &mut state, &mut credentials, true)
            .await
    }

    pub async fn snapshot(&self, net: &dyn Transport) -> Result<AccountSnapshot, Error> {
        let mut state = self.state.lock().await;
        let sender = Sender::Transport(net);
        let mut credentials = self.ready(sender, &mut state, true).await?;
        let catalog = self
            .catalog(sender, &mut state, &mut credentials, true)
            .await?;
        Ok(AccountSnapshot {
            catalog,
            allowance: state.allowance.clone().unwrap(),
        })
    }

    fn observe_usage(&self, ctx: &RequestCtx, bytes: &[u8]) {
        if let Some(sample) = usage::sample(bytes) {
            let mut usage = self.usage.lock().unwrap();
            usage::update(&mut usage, ctx.id, sample);
        }
    }
}

impl Hook for DeepAIAcc {
    fn name(&self) -> &str {
        "deepaiacc"
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
            let path = match ctx.format {
                Format::OpenAiChat => "/chat/completions",
                Format::OpenAiResponses => "/responses",
                _ => return Err("DeepAIAcc requires OpenAI Chat or Responses".into()),
            };
            let url = format!("{}{path}", api::BASE);
            if req.url == format!("{}/v1{path}", api::BASE) {
                req.url = url.clone();
            }
            if req.method != "POST" || req.url != url {
                return Err("unsupported DeepAI endpoint".into());
            }
            let mut body = request::prepare(&req.body, ctx.format)?;
            let affinity = request::session(&mut body, &req.headers)?;
            req.headers = vec![("x-session-affinity".into(), affinity)];
            req.body = Bytes::from(body.to_string());
            Ok(())
        })
    }

    fn on_body(&self, ctx: &RequestCtx, status: u16, body: &mut Bytes) -> Result<(), String> {
        api::normalize_error(body, Some(status));
        self.observe_usage(ctx, body);
        Ok(())
    }

    fn on_stream_event(
        &self,
        ctx: &RequestCtx,
        event: &mut ncore::core::SseEvent,
    ) -> Result<EventAction, String> {
        let mut bytes = Bytes::copy_from_slice(event.data.as_bytes());
        api::normalize_error(&mut bytes, None);
        self.observe_usage(ctx, &bytes);
        if bytes.as_ref() != event.data.as_bytes() {
            event.set_data(
                std::str::from_utf8(&bytes).map_err(|_| "invalid DeepAI event encoding")?,
            );
        }
        Ok(EventAction::Keep)
    }
}

fn model_lane_error(error: Error) -> Error {
    let reason = match error {
        Error::Provider(error) => format!(
            "{} (HTTP {}, {})",
            error.message,
            error.status.unwrap_or(0),
            error.code.as_deref().unwrap_or("request_failed")
        ),
        Error::Plugin { message, .. } => message,
        _ => "metadata request failed".into(),
    };
    fail(format!(
        "Lane update failed after a model response; no model request was replayed: {reason}"
    ))
}

impl Middleware for DeepAIAcc {
    fn name(&self) -> &str {
        "deepaiacc"
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        mut req: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            if ctx.attempt > 1 {
                return Err(fail(
                    "automatic DeepAI model replay refused; billing may already have occurred",
                ));
            }
            let expected = match ctx.format {
                Format::OpenAiChat => api::CHAT_URL,
                Format::OpenAiResponses => api::RESPONSES_URL,
                _ => return Err(fail("unsupported DeepAI format")),
            };
            if req.method != "POST" || req.url != expected {
                return Err(fail("unsupported DeepAI endpoint"));
            }
            let slot = self.slot.clone().lock_owned().await;
            let sender = Sender::Next(next);
            let mut body: Value = serde_json::from_slice(&req.body)
                .map_err(|_| fail("invalid DeepAI request JSON"))?;
            let credentials = {
                let mut state = self.state.lock().await;
                let mut credentials = self.ready(sender, &mut state, false).await?;
                if !state.allowance.as_ref().is_some_and(|a| a.enabled) {
                    return Err(fail("DeepAI Theos allowance is disabled"));
                }
                let catalog = self
                    .catalog(sender, &mut state, &mut credentials, false)
                    .await?;
                let id = body["model"]
                    .as_str()
                    .ok_or_else(|| fail("missing DeepAI model"))?;
                let model = catalog
                    .model(id)
                    .ok_or_else(|| fail("DeepAI model is not in the current catalog"))?;
                request::validate_model(&mut body, model, ctx.format).map_err(fail)?;
                credentials
            };
            let affinity = ncore::rt::header(&req.headers, "x-session-affinity")
                .map(str::to_owned)
                .ok_or_else(|| fail("missing DeepAI session identity"))?;
            req.headers = api::headers(&credentials, credentials.lane)?;
            req.headers.push(("x-session-affinity".into(), affinity));
            req.body = Bytes::from(body.to_string());
            let mut response = sender.send(req).await?;
            if response.status == 426 {
                let head = response.headers.clone();
                let (status, value) = api::json_response(response).await?;
                let mut error = api::provider_error(Some(status), &value);
                if lane::unsupported(&Error::Provider(error.clone())) {
                    let lane = self
                        .recover_lane(sender, &credentials)
                        .await
                        .map_err(model_lane_error)?;
                    let mut state = self.state.lock().await;
                    let mut current = self.load(&mut state).await.map_err(model_lane_error)?;
                    Self::align_lane(&mut state, &mut current, lane);
                    self.renew(sender, &mut state, &mut current)
                        .await
                        .map_err(|error| model_lane_error(api::account_error("/theos", error)))?;
                    error.code = Some("lane_updated".into());
                    error.message = "DeepAI lane was updated; retry this turn explicitly. No model request was replayed.".into();
                    error.raw = Bytes::from(serde_json::json!({"error":{"code":error.code,"type":"invalid_request_error","message":error.message}}).to_string());
                }
                let body = error.raw;
                return Ok(HttpResponse {
                    status,
                    headers: head
                        .into_iter()
                        .filter(|(name, _)| {
                            !matches!(
                                name.to_ascii_lowercase().as_str(),
                                "content-length" | "content-encoding" | "content-type"
                            )
                        })
                        .chain([("content-type".into(), "application/json".into())])
                        .collect(),
                    body: Box::pin(futures_util::stream::once(async move { Ok(body) })),
                });
            }
            if !(200..300).contains(&response.status) && response.status < 400 {
                return Err(fail(
                    "DeepAI returned an unexpected HTTP status; redirects are not followed",
                ));
            }
            if (200..300).contains(&response.status) {
                if !response
                    .header("content-type")
                    .is_some_and(|s| s.contains("text/event-stream"))
                {
                    return Err(fail("DeepAI did not return the requested event stream"));
                }
                if !ctx.streaming {
                    return stream::collect(ctx.format, response).await;
                }
            }
            response.body = Box::pin(futures_util::stream::unfold(
                Some((response.body, slot)),
                |state| async move {
                    let (mut body, slot) = state?;
                    match body.next().await {
                        Some(Ok(chunk)) => Some((Ok(chunk), Some((body, slot)))),
                        Some(Err(error)) => Some((Err(error), None)),
                        None => None,
                    }
                },
            ));
            Ok(response)
        })
    }
}
