use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use nabu_protocol::{Account, AccountStatus, CoArenaQuota, Event, Model, ModelRef, Provider};
use ncore::core::ir::{self, ReasoningCfg};
use ncore::core::{ErrorKind, ProviderError};
use ncore::{
    Auth, CachePolicy, CancelToken, Core, Endpoint, Format, GenerateOptions, InboundRequest,
    PassthroughBody, PassthroughOptions,
};
use ncore_agentrouter_bp::AgentRouterBP;
use ncore_tokio::HyperTransport;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::accounts_file::AccountsFile;
use crate::models::Route;

const MAX_CONCURRENT_CHECKS: usize = 4;

#[derive(Clone, Copy)]
pub enum KeyProvider {
    AgentRouter,
    CoArena,
    Clauden,
    Gemini,
    NvidiaBuild,
}

impl KeyProvider {
    fn id(self) -> &'static str {
        match self {
            Self::AgentRouter => "agentrouter",
            Self::CoArena => "coarena",
            Self::Clauden => "clauden",
            Self::Gemini => "gemini",
            Self::NvidiaBuild => "nvidiabuild",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::AgentRouter => "Agent Router",
            Self::CoArena => "CoArena",
            Self::Clauden => "Clauden",
            Self::Gemini => "Gemini API",
            Self::NvidiaBuild => "Nvidia Build",
        }
    }

    fn endpoint(self, credentials: &str) -> Endpoint {
        match self {
            Self::AgentRouter => AgentRouterBP::endpoint(credentials),
            Self::Gemini => Endpoint::gemini(credentials).with_signature_namespace(self.id()),
            Self::NvidiaBuild => {
                Endpoint::new(Format::OpenAiChat, "https://integrate.api.nvidia.com")
                    .with_auth(Auth::Bearer(credentials.into()))
                    .with_signature_namespace(self.id())
            }
            Self::CoArena => Endpoint::new(Format::AnthropicMessages, "https://coarena.ai/api")
                .with_auth(Auth::Bearer(credentials.into()))
                .with_header("anthropic-version", "2023-06-01"),
            Self::Clauden => Endpoint::new(Format::AnthropicMessages, "https://clauden.best")
                .with_auth(Auth::Header {
                    name: "x-api-key".into(),
                    value: credentials.into(),
                })
                .with_header("anthropic-version", "2023-06-01")
                .with_signature_namespace(self.id()),
        }
    }

    fn generate_options(self) -> GenerateOptions {
        let mut options = GenerateOptions::default();
        match self {
            Self::AgentRouter | Self::Gemini => {}
            Self::NvidiaBuild => {
                options.encode.chat_max_tokens_field =
                    ncore::core::codec::MaxTokensField::MaxTokens;
            }
            Self::CoArena => options.cache = Some(CachePolicy::default()),
            Self::Clauden => {
                options.cache = Some(CachePolicy::none());
                options.encode.default_max_tokens = Some(32_000);
            }
        }
        options
    }

    fn catalog_model(self, info: &Value) -> Option<Model> {
        if matches!(self, Self::NvidiaBuild) {
            return nvidia_model(info);
        }
        let mut model = catalog_model(info)?;
        if matches!(self, Self::CoArena) {
            model.context = info["context_length"].as_u64().unwrap_or(model.context);
            model.output = info["max_completion_tokens"].as_u64().unwrap_or(0);
            model.images = info["capabilities"]["vision"]
                .as_bool()
                .unwrap_or(model.images);
            model.efforts = info["capabilities"]["efforts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|effort| effort.as_str().map(str::to_owned))
                .collect();
        }
        if matches!(self, Self::AgentRouter) && model.family == "gpt" {
            model.efforts = AGENTROUTER_EFFORTS.map(str::to_owned).to_vec();
        }
        if matches!(self, Self::Clauden) {
            model.efforts.clear();
            model.fast = false;
        }
        Some(model)
    }
}

const AGENTROUTER_EFFORTS: [&str; 3] = ["high", "xhigh", "max"];

struct Entry {
    key: String,
    label: String,
    status: AccountStatus,
    models: Vec<Model>,
    /// CoArena account the key belongs to; never changes, so it is fetched once.
    account: Option<String>,
    coarena: Option<CoArenaLimits>,
    gemini_blocks: HashMap<String, GeminiBlock>,
}

struct GeminiBlock {
    message: String,
    retry_at: Option<Instant>,
}

impl GeminiBlock {
    fn active(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|deadline| deadline > now)
    }
}

struct CoArenaLimits {
    total: f64,
    used: f64,
    remaining: f64,
    requests_left: Option<u64>,
    reset: DateTime<Utc>,
    checked_at: Instant,
}

impl CoArenaLimits {
    fn parse(body: &Value) -> Option<Self> {
        let data = &body["data"];
        let today = &data["today"];
        Some(Self {
            total: data["usd_per_day"].as_f64()?,
            used: today["used_usd"].as_f64()?,
            remaining: today["left_usd"].as_f64()?,
            requests_left: today["requests_left"].as_u64(),
            reset: DateTime::from_timestamp_millis(today["resets_at"].as_i64()?)?,
            checked_at: Instant::now(),
        })
    }

    fn status(&self) -> AccountStatus {
        if self.requests_left == Some(0) || (self.remaining <= 0.0 && self.used >= self.total) {
            AccountStatus::Limit {
                message: format!(
                    "Daily quota exhausted. Resets at {}.",
                    self.reset.to_rfc3339()
                ),
            }
        } else {
            AccountStatus::Work
        }
    }
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    sessions: HashMap<String, String>,
    next: usize,
    experimental_context: bool,
    direct_checks: bool,
}

pub struct ApiKeyAccounts {
    provider: KeyProvider,
    file: Arc<AccountsFile<String>>,
    context_path: PathBuf,
    direct_checks_path: PathBuf,
    core: Core,
    direct_core: Option<Core>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    checking: tokio::sync::Mutex<()>,
    stop_refresh: tokio::sync::Notify,
}

impl ApiKeyAccounts {
    pub fn load(
        provider: KeyProvider,
        dir: &Path,
        net: Arc<dyn ncore::Transport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file =
            AccountsFile::<String>::load(dir.join(format!("{}-keys.json", provider.id())), None);
        let context_path = dir.join("coarena-experimental-context.json");
        let direct_checks_path = dir.join("coarena-direct-checks.json");
        let coarena = matches!(provider, KeyProvider::CoArena);
        let experimental_context = coarena && read_switch(&context_path);
        let direct_checks = coarena && read_switch(&direct_checks_path);
        let entries = file
            .list()
            .into_iter()
            .map(|s| entry(s.key, &s.credentials))
            .collect();
        Arc::new(Self {
            provider,
            file,
            context_path,
            direct_checks_path,
            core: Core::builder(net).build(),
            direct_core: coarena.then(|| Core::builder(Arc::new(HyperTransport::new())).build()),
            events,
            state: Mutex::new(State {
                entries,
                experimental_context,
                direct_checks,
                ..State::default()
            }),
            checking: tokio::sync::Mutex::new(()),
            stop_refresh: tokio::sync::Notify::new(),
        })
    }

    pub fn list(&self) -> Vec<Account> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| Account {
                id: None,
                max_credits: None,
                email: e.label.clone(),
                tier: None,
                status: e.status.clone(),
            })
            .collect()
    }

    pub fn coarena_quota(&self) -> CoArenaQuota {
        let state = self.state.lock().unwrap();
        let now = Utc::now();
        let mut accounts = HashMap::<&str, &CoArenaLimits>::new();
        let mut partial = false;
        for entry in &state.entries {
            let Some(limits) = entry.coarena.as_ref().filter(|limits| limits.reset > now) else {
                partial = true;
                continue;
            };
            let Some(id) = entry.account.as_deref() else {
                partial = true;
                continue;
            };
            let latest = accounts.entry(id).or_insert(limits);
            if limits.checked_at > latest.checked_at {
                *latest = limits;
            }
        }
        CoArenaQuota {
            total_usd: accounts.values().map(|limits| limits.total).sum(),
            used_usd: accounts.values().map(|limits| limits.used).sum(),
            remaining_usd: accounts.values().map(|limits| limits.remaining).sum(),
            reset: accounts
                .values()
                .map(|limits| limits.reset)
                .min()
                .map(|reset| reset.to_rfc3339()),
            partial,
        }
    }

    pub fn add(&self, keys: Vec<String>) -> Result<Vec<String>, String> {
        let keys: Vec<_> = keys
            .into_iter()
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
            .collect();
        if keys.is_empty() {
            return Err("Add at least one API key".into());
        }
        let mut state = self.state.lock().unwrap();
        let mut added = Vec::new();
        for credentials in keys {
            if self
                .file
                .list()
                .iter()
                .any(|s| s.credentials == credentials)
            {
                continue;
            }
            let key = self.file.add(credentials.clone());
            added.push(key.clone());
            state.entries.push(entry(key, &credentials));
        }
        drop(state);
        self.changed();
        Ok(added)
    }

    pub fn remove(&self, label: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        let keys: Vec<_> = state
            .entries
            .iter()
            .filter(|e| label.is_none_or(|label| e.label == label))
            .map(|e| e.key.clone())
            .collect();
        for key in &keys {
            self.file.remove(key);
        }
        state.entries.retain(|e| !keys.contains(&e.key));
        state.sessions.retain(|_, key| !keys.contains(key));
        drop(state);
        self.changed();
        self.quota_changed();
    }

    pub fn experimental_context(&self) -> bool {
        self.state.lock().unwrap().experimental_context
    }

    pub fn set_experimental_context(&self, enabled: bool) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.experimental_context == enabled {
            return Ok(());
        }
        write_switch(&self.context_path, enabled)?;
        state.experimental_context = enabled;
        drop(state);
        let _ = self.events.send(Event::ProvidersChanged);
        Ok(())
    }

    pub fn direct_checks(&self) -> bool {
        self.state.lock().unwrap().direct_checks
    }

    pub fn set_direct_checks(&self, enabled: bool) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.direct_checks != enabled {
            write_switch(&self.direct_checks_path, enabled)?;
            state.direct_checks = enabled;
        }
        Ok(())
    }

    pub fn provider(&self) -> Option<Provider> {
        let state = self.state.lock().unwrap();
        let mut models = Vec::<Model>::new();
        for entry in &state.entries {
            for model in &entry.models {
                if !models.iter().any(|m| m.id == model.id) {
                    models.push(model.clone());
                }
            }
        }
        if state.experimental_context {
            for model in &mut models {
                if model.context == 128_000
                    && matches!(
                        model.id.as_str(),
                        "claude-opus-5-5"
                            | "claude-sonnet-5-5"
                            | "claude-haiku-5-5"
                            | "claude-fable-5-2"
                            | "gemini-4-carbon"
                    )
                {
                    model.context = 135_000;
                }
            }
        }
        models.sort_by(crate::models::strongest_first);
        (!models.is_empty()).then(|| Provider {
            id: self.provider.id().into(),
            name: self.provider.name().into(),
            models,
            custom: false,
        })
    }

    pub async fn check_all(&self) {
        self.check_keys(|_| true, true).await;
    }

    pub async fn check_limited(&self) {
        self.check_keys(
            |entry| matches!(entry.status, AccountStatus::Limit { .. }),
            true,
        )
        .await;
    }

    pub async fn refresh_coarena(&self) {
        self.check_keys(|_| true, false).await;
    }

    pub async fn check_added(&self, added: Vec<String>) {
        if !added.is_empty() {
            self.check_keys(|entry| added.contains(&entry.key), true)
                .await;
        }
    }

    async fn check_keys(&self, matches: impl Fn(&Entry) -> bool, full: bool) {
        let mut stop = std::pin::pin!(self.stop_refresh.notified());
        let _guard = if full {
            // A background refresh repeats part of this check, so it gives way instead of delaying it.
            self.stop_refresh.notify_waiters();
            self.checking.lock().await
        } else {
            stop.as_mut().enable();
            let Ok(guard) = self.checking.try_lock() else {
                return;
            };
            guard
        };
        let keys = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .filter(|entry| matches(entry))
                .map(|e| {
                    if full {
                        e.status = AccountStatus::Checking;
                    }
                    (e.key.clone(), full || e.models.is_empty())
                })
                .collect::<Vec<_>>()
        };
        if keys.is_empty() {
            return;
        }
        if full {
            self.changed();
        }
        let checks = stream::iter(keys)
            .for_each_concurrent(MAX_CONCURRENT_CHECKS, |(key, refresh_models)| {
                self.check_key(key, refresh_models)
            });
        tokio::select! {
            _ = checks => {}
            _ = stop, if !full => return,
        }
        self.quota_changed();
    }

    async fn check_key(&self, key: String, refresh_models: bool) {
        let Some(credentials) = self.file.credentials(&key) else {
            return;
        };
        let result = match self.provider {
            KeyProvider::AgentRouter => self.check_agentrouter(&credentials).await,
            KeyProvider::NvidiaBuild => self.check_nvidia(&credentials).await,
            KeyProvider::Clauden | KeyProvider::Gemini => self
                .catalog(&credentials)
                .await
                .map_err(|message| check_error(message, &credentials)),
            KeyProvider::CoArena => {
                return self.check_coarena(&key, &credentials, refresh_models).await;
            }
        };
        {
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                match result {
                    Ok(models) => {
                        entry.models = models;
                        entry.status = AccountStatus::Work;
                        // A catalog check allows retrying plan denials but cannot reset a timed quota.
                        let now = Instant::now();
                        entry
                            .gemini_blocks
                            .retain(|_, block| block.retry_at.is_some() && block.active(now));
                    }
                    Err(status) => {
                        entry.status = status;
                    }
                }
            }
        }
        self.changed();
    }

    async fn check_coarena(&self, key: &str, credentials: &str, refresh_models: bool) {
        let limits = self.fetch_coarena_limits(credentials).await;
        let has_account = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.key == key && e.account.is_some());
        let (account, models) = if limits.is_ok() {
            tokio::join!(
                async {
                    if has_account {
                        None
                    } else {
                        self.coarena_account(credentials).await
                    }
                },
                async {
                    if refresh_models {
                        self.catalog(credentials).await.ok()
                    } else {
                        None
                    }
                },
            )
        } else {
            (None, None)
        };
        let changed = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key) else {
                return;
            };
            let status = match &limits {
                Ok(limits) => limits.status(),
                Err(message) => check_error(message.clone(), credentials),
            };
            let changed = status != entry.status || models.is_some();
            entry.status = status;
            if let Some(models) = models {
                entry.models = models;
            }
            if account.is_some() {
                entry.account = account;
            }
            if limits.is_ok() || !matches!(entry.status, AccountStatus::Limit { .. }) {
                entry.coarena = limits.ok();
            }
            changed
        };
        if changed {
            self.changed();
        }
    }

    async fn fetch_coarena_limits(&self, credentials: &str) -> Result<CoArenaLimits, String> {
        let body = self.fetch(credentials, "/v1/limits", None).await?;
        CoArenaLimits::parse(&body).ok_or_else(|| "Invalid CoArena limits response".into())
    }

    async fn coarena_account(&self, credentials: &str) -> Option<String> {
        let body = self.fetch(credentials, "/v1/keys", None).await.ok()?;
        // Keys from the same account see the same key IDs.
        body["data"]
            .as_array()?
            .iter()
            .filter_map(|key| key["id"].as_str())
            .min()
            .map(str::to_owned)
    }

    async fn catalog(&self, credentials: &str) -> Result<Vec<Model>, String> {
        if matches!(self.provider, KeyProvider::Gemini) {
            return self.gemini_catalog(credentials).await;
        }
        let catalog = self.fetch(credentials, "/v1/models", None).await?;
        let mut models: Vec<Model> = catalog["data"]
            .as_array()
            .ok_or("Invalid model catalog")?
            .iter()
            .filter_map(|info| self.provider.catalog_model(info))
            .collect();
        for model in &mut models {
            model.older = false;
        }
        if models.is_empty() {
            return Err(format!("{} returned no models", self.provider.name()));
        }
        Ok(models)
    }

    async fn gemini_catalog(&self, credentials: &str) -> Result<Vec<Model>, String> {
        const FIRST_PAGE: &str = "/v1beta/models?pageSize=1000";
        let mut models = Vec::new();
        let mut path = FIRST_PAGE.to_owned();
        loop {
            let page = self.fetch(credentials, &path, None).await?;
            models.extend(
                page["models"]
                    .as_array()
                    .ok_or("Invalid Gemini model catalog")?
                    .iter()
                    .filter_map(gemini_model),
            );
            let Some(token) = page["nextPageToken"]
                .as_str()
                .filter(|token| !token.is_empty())
            else {
                break;
            };
            let token =
                percent_encoding::utf8_percent_encode(token, percent_encoding::NON_ALPHANUMERIC);
            path = format!("{FIRST_PAGE}&pageToken={token}");
        }
        if models.is_empty() {
            return Err("Gemini API returned no chat models".into());
        }
        Ok(models)
    }

    async fn check_nvidia(&self, credentials: &str) -> Result<Vec<Model>, AccountStatus> {
        let models = self
            .catalog(credentials)
            .await
            .map_err(|e| check_error(e, credentials))?;
        // The public catalog responds even to invalid keys. Prefer lightweight chat models
        // for the authenticated probe, but don't make the catalog depend on those IDs.
        let mut candidates: Vec<_> = models.iter().collect();
        candidates.sort_by_key(|model| match model.id.as_str() {
            "nvidia/nemotron-3.5-bolt-e4b" => 0,
            "google/gemma-4-31b-it" => 1,
            "meta/muse-glimmer-30b" => 2,
            _ => 3,
        });
        let mut failure = "Nvidia Build did not return a chat completion".to_owned();
        for model in candidates.into_iter().take(3) {
            let result = self
                .fetch_json(
                    credentials,
                    "/v1/chat/completions",
                    Some(json!({
                        "model": model.id,
                        "messages": [{"role": "user", "content": "Reply only OK."}],
                        "max_tokens": 16,
                        "stream": false,
                    })),
                )
                .await;
            match result {
                Ok((status, body))
                    if (200..300).contains(&status)
                        && body["choices"].as_array().is_some_and(|choices| {
                            choices.iter().any(|choice| choice["message"].is_object())
                        }) =>
                {
                    return Ok(models);
                }
                Ok((status @ (401 | 403), body)) => {
                    return Err(check_error(check_http_error(status, &body), credentials));
                }
                Ok((status @ 400..=599, body)) => failure = check_http_error(status, &body),
                Ok(_) => failure = "Nvidia Build did not return a chat completion".into(),
                Err(error) => failure = error,
            }
        }
        Err(check_error(failure, credentials))
    }

    async fn check_agentrouter(&self, credentials: &str) -> Result<Vec<Model>, AccountStatus> {
        let models = self
            .catalog(credentials)
            .await
            .map_err(|message| check_error(message, credentials))?;
        let mut failure = String::new();
        for model in &models {
            let result = self
                .fetch_json(
                    credentials,
                    "/v1/responses",
                    Some(json!({
                        "model": model.id, "input": "Reply only OK.", "max_output_tokens": 128,
                        "reasoning": {"effort": "low"}, "store": false, "stream": false
                    })),
                )
                .await;
            // A response from the upstream model confirms the key can reach it.
            match result {
                Ok((status, body)) => {
                    if status < 400 && agentrouter_check_complete(&body) {
                        return Ok(models);
                    }
                    let error = ProviderError::from_value(
                        Format::OpenAiResponses,
                        Some(status),
                        &body,
                        body.to_string().into(),
                    );
                    if agentrouter_quota_exhausted(&error) {
                        return Err(AccountStatus::Limit {
                            message: check_http_error(status, &body)
                                .replace(credentials, "[redacted]"),
                        });
                    }
                    if error.kind == ErrorKind::RateLimited
                        || agentrouter_upstream_busy(&error.message)
                    {
                        return Ok(models);
                    }
                    failure = if status >= 400 {
                        check_http_error(status, &body)
                    } else {
                        "Model did not complete the connection check".into()
                    };
                }
                Err(error) => failure = error,
            }
        }
        Err(check_error(failure, credentials))
    }

    async fn fetch(
        &self,
        credentials: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let (status, body) = self.fetch_json(credentials, path, body).await?;
        if status >= 400 {
            return Err(check_http_error(status, &body));
        }
        Ok(body)
    }

    async fn fetch_json(
        &self,
        credentials: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), String> {
        let request = InboundRequest {
            method: if body.is_some() { "POST" } else { "GET" }.into(),
            path_and_query: path.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.map_or_else(Default::default, |body| body.to_string().into()),
        };
        let endpoint = self.provider.endpoint(credentials);
        let cancel = CancelToken::new();
        let core = self
            .direct_core
            .as_ref()
            .filter(|_| self.direct_checks())
            .unwrap_or(&self.core);
        let response = tokio::time::timeout(
            Duration::from_secs(45),
            core.passthrough(&endpoint, request, PassthroughOptions::default(), &cancel),
        )
        .await
        .map_err(|_| format!("{} request timed out", self.provider.name()))?
        .map_err(|e| e.to_string())?;
        let PassthroughBody::Full { bytes, .. } = response.body else {
            return Err("Unexpected stream".into());
        };
        let body = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok((response.status, body))
    }

    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let mut blocked = None;
        let usable: Vec<_> = state
            .entries
            .iter()
            .filter(|entry| {
                if entry.status != AccountStatus::Work
                    || !entry.models.iter().any(|m| m.id == model.model_id)
                {
                    return false;
                }
                if let Some(block) = entry
                    .gemini_blocks
                    .get(&model.model_id)
                    .filter(|block| block.active(now))
                {
                    blocked = Some(&block.message);
                    return false;
                }
                true
            })
            .collect();
        let previous = state
            .sessions
            .get(session)
            .and_then(|key| usable.iter().find(|e| e.key == *key));
        let entry = previous
            .copied()
            .or_else(|| usable.get(state.next % usable.len().max(1)).copied())
            .ok_or_else(|| {
                blocked.cloned().unwrap_or_else(|| {
                    format!("No working {} key has this model", self.provider.name())
                })
            })?;
        let key = entry.key.clone();
        let known = entry
            .models
            .iter()
            .find(|m| m.id == model.model_id)
            .unwrap();
        if matches!(self.provider, KeyProvider::CoArena) && known.output > 0 {
            request.max_tokens.get_or_insert(known.output);
        }
        // Without a picked level Agent Router gets the top one.
        let fallback = matches!(self.provider, KeyProvider::AgentRouter)
            .then(|| known.efforts.last())
            .flatten();
        request.reasoning = model
            .variant
            .as_ref()
            .or(fallback)
            .filter(|effort| known.efforts.contains(effort))
            .map(|effort| ReasoningCfg {
                effort: Some(effort.clone()),
                budget_tokens: if matches!(self.provider, KeyProvider::Gemini)
                    && known.id.starts_with("gemini-2.5-")
                {
                    match effort.as_str() {
                        "low" => Some(512),
                        "medium" => Some(4_096),
                        "high" => Some(16_384),
                        _ => None,
                    }
                } else {
                    None
                },
                include: Some(true),
                ..Default::default()
            });
        let credentials = self
            .file
            .credentials(&key)
            .ok_or_else(|| format!("{} key removed", self.provider.name()))?;
        if !state.sessions.contains_key(session) {
            state.next = state.next.wrapping_add(1);
        }
        state.sessions.insert(session.into(), key.clone());
        Ok(Route {
            endpoint: self.provider.endpoint(&credentials),
            options: self.provider.generate_options(),
            account: Some(key),
        })
    }

    pub fn gemini_refused(&self, key: &str, model: &str, error: &mut ProviderError) -> bool {
        let Some((reason, delay)) = gemini_refusal(error) else {
            return false;
        };
        let credentials = self.file.credentials(key).unwrap_or_default();
        let details = if credentials.is_empty() {
            error.message.clone()
        } else {
            error.message.replace(&credentials, "[redacted]")
        };
        let status = error
            .status
            .map(|status| format!(" (HTTP {status})"))
            .unwrap_or_default();
        error.message = format!("Gemini API · {model} — {reason}{status}. {details}");

        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key) {
            let now = Instant::now();
            let retry_at = delay.map(|delay| {
                now.checked_add(delay)
                    .unwrap_or_else(|| now + Duration::from_secs(60))
            });
            entry.gemini_blocks.insert(
                model.into(),
                GeminiBlock {
                    message: error.message.clone(),
                    retry_at,
                },
            );
        }
        true
    }

    pub fn limited(&self, key: &str, error: &ProviderError) -> bool {
        if matches!(self.provider, KeyProvider::AgentRouter) && !agentrouter_quota_exhausted(error)
        {
            return false;
        }
        let message = &error.message;
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
            let credentials = self.file.credentials(key).unwrap_or_default();
            entry.status = AccountStatus::Limit {
                message: if credentials.is_empty() {
                    message.into()
                } else {
                    message.replace(&credentials, "[redacted]")
                },
            };
        }
        state.sessions.retain(|_, value| value != key);
        drop(state);
        self.changed();
        true
    }

    fn changed(&self) {
        let accounts = self.list();
        let event = match self.provider {
            KeyProvider::AgentRouter => Event::AgentRouterAccounts { accounts },
            KeyProvider::CoArena => Event::CoArenaAccounts { accounts },
            KeyProvider::Clauden => Event::ClaudenAccounts { accounts },
            KeyProvider::Gemini => Event::GeminiAccounts { accounts },
            KeyProvider::NvidiaBuild => Event::NvidiaBuildAccounts { accounts },
        };
        let _ = self.events.send(event);
        let _ = self.events.send(Event::ProvidersChanged);
    }

    fn quota_changed(&self) {
        if matches!(self.provider, KeyProvider::CoArena) {
            let _ = self.events.send(Event::CoArenaQuota {
                quota: self.coarena_quota(),
            });
        }
    }
}

fn gemini_refusal(error: &ProviderError) -> Option<(&'static str, Option<Duration>)> {
    if error.kind == ErrorKind::RateLimited || error.status == Some(429) {
        let body: Value = serde_json::from_slice(&error.raw).unwrap_or(Value::Null);
        let details = body["error"]["details"].as_array().into_iter().flatten();
        let zero_quota = details
            .clone()
            .filter(|detail| detail["@type"] == "type.googleapis.com/google.rpc.QuotaFailure")
            .flat_map(|detail| detail["violations"].as_array().into_iter().flatten())
            .any(|violation| {
                violation["quotaValue"].as_u64() == Some(0)
                    || violation["quotaValue"].as_str() == Some("0")
            })
            || error.message.lines().any(|line| {
                line.contains("Quota exceeded for metric:") && line.contains(", limit: 0,")
            });
        if zero_quota {
            return Some(("Model unavailable on the current plan (quota is 0)", None));
        }
        let retry_delay = details
            .filter(|detail| detail["@type"] == "type.googleapis.com/google.rpc.RetryInfo")
            .filter_map(|detail| detail["retryDelay"].as_str())
            .filter_map(|delay| delay.strip_suffix('s')?.parse::<f64>().ok())
            .find_map(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .filter(|delay| !delay.is_zero())
            .unwrap_or(Duration::from_secs(60));
        return Some(("Model quota exhausted", Some(retry_delay)));
    }

    match (error.status, error.kind) {
        (Some(402), _) | (_, ErrorKind::Billing) => {
            Some(("Google requires billing or credits", None))
        }
        _ if error.code.as_deref() == Some("FAILED_PRECONDITION")
            && error.message.to_ascii_lowercase().contains("billing") =>
        {
            Some(("Google requires billing or credits", None))
        }
        (_, ErrorKind::PermissionDenied | ErrorKind::NotFound) => {
            Some(("Model unavailable to this Google project", None))
        }
        _ => None,
    }
}

pub(crate) fn read_switch(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub(crate) fn write_switch(path: &Path, enabled: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, enabled.to_string())?;
    std::fs::rename(tmp, path)
}

pub(crate) fn agentrouter_upstream_busy(message: &str) -> bool {
    message.contains("Your requests to ") && message.contains(" have exceeded token rate limit")
}

fn agentrouter_quota_exhausted(error: &ProviderError) -> bool {
    let quota_code = |code: &str| matches!(code, "insufficient_quota" | "quota_exhausted");
    error.code.as_deref().is_some_and(quota_code)
        || serde_json::from_slice::<Value>(&error.raw).is_ok_and(|body| {
            ["/error/code", "/error/type", "/code"]
                .into_iter()
                .filter_map(|path| body.pointer(path).and_then(Value::as_str))
                .any(quota_code)
        })
}

fn check_http_error(status: u16, body: &Value) -> String {
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .or_else(|| body["error"].as_str())
        .unwrap_or("Provider rejected the request");
    format!("HTTP {status}: {message}")
}

fn check_error(message: String, credentials: &str) -> AccountStatus {
    let message = message.replace(credentials, "[redacted]");
    if message.starts_with("HTTP 429:") {
        AccountStatus::Limit { message }
    } else {
        AccountStatus::Error { message }
    }
}

fn agentrouter_check_complete(body: &Value) -> bool {
    body["status"] == "completed"
        && body["output"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["text"]
                            .as_str()
                            .is_some_and(|text| text.trim() == "OK")
                    })
                })
            })
        })
}

fn entry(key: String, credentials: &str) -> Entry {
    let suffix: String = credentials
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Entry {
        label: format!("••••{suffix} · {key}"),
        key,
        status: AccountStatus::Unchecked,
        models: Vec::new(),
        account: None,
        coarena: None,
        gemini_blocks: HashMap::new(),
    }
}

fn nvidia_model(info: &Value) -> Option<Model> {
    let id = info["id"].as_str().filter(|id| !id.is_empty())?;
    let normalized = id.to_ascii_lowercase();
    // Most catalog items have no capability metadata. Exclude known non-chat
    // families and endpoints confirmed unavailable, not untested or slow models.
    if matches!(
        normalized.as_str(),
        "deepseek-ai/deepseek-v4.1-flash"
            | "google/gemma-3-12b-it"
            | "nvidia/cosmos-reason2-8b"
            | "bigcode/starcoder2-15b"
            | "nvidia/nvclip"
            | "fastino/gliner2-pii"
    ) || normalized.split(['/', '-', '_', '.']).any(|part| {
        matches!(
            part,
            "embed"
                | "embedqa"
                | "nemoretriever"
                | "embedding"
                | "detector"
                | "rerank"
                | "reranker"
                | "safety"
                | "nemoguard"
                | "guard"
                | "translate"
                | "calibration"
        )
    }) || matches!(
        info["model_type"].as_str(),
        Some("embedding" | "reranker" | "classification")
    ) || info["output_modalities"]
        .as_array()
        .is_some_and(|m| !m.iter().any(|v| v == "text"))
    {
        return None;
    }
    let mut model = catalog_model(info)?;
    model.id = id.into();
    let efforts: &[&str] = match id {
        "nvidia/nemotron-3-ultra-550b-a55b"
        | "nvidia/nemotron-3.5-super-vl-120b-a12b"
        | "nvidia/nemotron-3.5-lightning-30b-a3b"
        | "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning" => &["none", "low", "high", "max"],
        // Step's `none` still yielded reasoning; `max` timed out in the probe.
        "stepfun-ai/step-5-preview" => &["minimal", "low", "medium", "high", "xhigh"],
        "mistralai/mistral-large-4-1t-a52b" => &["low"],
        _ => &[],
    };
    if model.efforts.is_empty() {
        model.efforts = efforts.iter().map(|effort| (*effort).into()).collect();
    } else {
        model.efforts.retain(|effort| {
            matches!(
                effort.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        });
    }
    model.images = info["input_modalities"].as_array().map_or_else(
        || info["model_type"] == "vlm" || id == "nvidia/nemotron-3.5-super-vl-120b-a12b",
        |m| m.iter().any(|v| v == "image"),
    );
    model.output = info["max_completion_tokens"].as_u64().unwrap_or(0);
    model.fast = false; // No separate priority tier, regardless of model latency.
    Some(model)
}

fn gemini_model(info: &Value) -> Option<Model> {
    let id = info["name"].as_str()?.strip_prefix("models/")?;
    let supports_generation = info["supportedGenerationMethods"]
        .as_array()?
        .iter()
        .any(|method| method == "generateContent");
    // The catalog also lists specialized models that use generateContent for other modalities.
    let specialized = id.split('-').any(|part| {
        matches!(
            part,
            "image"
                | "tts"
                | "audio"
                | "live"
                | "robotics"
                | "transcribe"
                | "omni"
                | "banana"
                | "computer"
        )
    });
    if !supports_generation || !id.starts_with("gemini-") || specialized {
        return None;
    }
    Some(Model {
        id: id.into(),
        name: info["displayName"].as_str().unwrap_or(id).into(),
        family: "gemini".into(),
        efforts: gemini_efforts(id, info["thinking"].as_bool())
            .iter()
            .map(|effort| (*effort).into())
            .collect(),
        context: info["inputTokenLimit"].as_u64().unwrap_or(0),
        output: info["outputTokenLimit"].as_u64().unwrap_or(0),
        images: true,
        fast: false,
        older: false,
    })
}

fn gemini_efforts(id: &str, thinking: Option<bool>) -> &'static [&'static str] {
    if thinking == Some(false) {
        return &[];
    }
    match id {
        "gemini-4-neon" | "gemini-4-hydrogen" => &["low", "medium", "high", "xhigh"],
        "gemini-4-carbon"
        | "gemini-4-argon"
        | "gemini-3.8-flash"
        | "gemini-3.7-flash"
        | "gemini-3.1-pro-preview"
        | "gemini-3.1-pro-preview-customtools"
        | "gemini-2.5-pro"
        | "gemini-2.5-flash"
        | "gemini-2.5-flash-lite" => &["low", "medium", "high"],
        "gemini-3.6-flash"
        | "gemini-3.5-flash"
        | "gemini-3.5-flash-lite"
        | "gemini-3.1-flash-lite"
        | "gemini-3.1-flash-lite-preview"
        | "gemini-3-flash-preview" => &["minimal", "low", "medium", "high"],
        "gemini-3-pro-preview"
        | "gemini-pro-latest"
        | "gemini-flash-latest"
        | "gemini-flash-lite-latest" => &["low", "high"],
        _ => &[],
    }
}

fn catalog_model(info: &Value) -> Option<Model> {
    let mut model = crate::models::openai_model(info)?;
    let id = model.id.to_lowercase();
    model.family = id.rsplit('/').next()?.split('-').next()?.to_owned();
    Some(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use ncore::rt::{BoxFuture, header};
    use ncore::{HttpRequest, HttpResponse, Transport, TransportError};

    #[test]
    fn catalog_models_follow_family_strength_and_version() {
        let mut models: Vec<_> = [
            "gpt-5.6-sol",
            "gemini-3-flash",
            "claude-sonnet-4-6",
            "gpt-6.1-sol",
            "claude-opus-4-6",
            "gpt-6-astra",
            "claude-opus-4-8",
            "gemini-3-pro",
            "deepseek-v3",
            "qwen-3",
            "gpt-6.1-terra",
        ]
        .into_iter()
        .map(|id| catalog_model(&json!({"id":id})).unwrap())
        .collect();
        models.sort_by(crate::models::strongest_first);
        assert_eq!(
            models
                .iter()
                .map(|m| (m.id.as_str(), m.older))
                .collect::<Vec<_>>(),
            [
                ("claude-opus-4-8", false),
                ("claude-opus-4-6", false),
                ("claude-sonnet-4-6", false),
                ("gpt-6-astra", false),
                ("gpt-6.1-sol", false),
                ("gpt-6.1-terra", false),
                ("gpt-5.6-sol", false),
                ("gemini-3-pro", false),
                ("gemini-3-flash", false),
                ("deepseek-v3", false),
                ("qwen-3", false),
            ]
        );
    }

    #[derive(Default)]
    struct Mock {
        seen: Mutex<Vec<HttpRequest>>,
        fail: bool,
        router_busy: Option<&'static str>,
        catalog_permits: Option<tokio::sync::Semaphore>,
        nvidia_status: u16,
    }

    const ROUTER_BUSY: &str =
        "Your requests to gpt-6.1-sol for gpt-6.1-sol in eastus2 have exceeded token rate limit.";

    impl Transport for Mock {
        fn send(
            &self,
            request: HttpRequest,
        ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            let catalog = request.url.ends_with("/v1/models");
            let nvidia = request.url.starts_with("https://integrate.api.nvidia.com/");
            let messages = request.url.ends_with("/v1/messages");
            let limits = request.url.ends_with("/v1/limits");
            let keys = request.url.ends_with("/v1/keys");
            self.seen.lock().unwrap().push(request);
            Box::pin(async move {
                if catalog && let Some(permits) = &self.catalog_permits {
                    permits.acquire().await.unwrap().forget();
                }
                let body = if nvidia && catalog {
                    json!({"data":[{"id":"nvidia/nemotron-3.5-bolt-e4b"},
                        {"id":"nvidia/nemotron-3-ultra-550b-a55b"}, {"id":"moonshotai/kimi-k3.1-pro"},
                        {"id":"nvidia/nemotron-3-embed-1b"}]})
                } else if nvidia && self.nvidia_status == 403 {
                    json!({"message":"Authorization failed"})
                } else if nvidia {
                    json!({"choices":[{"message":{"role":"assistant","content":"OK","reasoning_content":"Thinking."},"finish_reason":"stop"}]})
                } else if self.fail {
                    json!({"error":{"message":"Invalid key"}})
                } else if catalog {
                    json!({"data":[{"id":"gpt-5.6-sol"},{"id":"gpt-6-astra"},{"id":"gpt-6.1-sol","max_completion_tokens":16384}]})
                } else if let Some(message) = self.router_busy {
                    json!({"error":{"message":message}})
                } else if limits {
                    json!({"data":{"usd_per_day":5,"today":{"used_usd":2,"left_usd":3,"requests_left":null,
                        "resets_at":(Utc::now() + chrono::Duration::hours(1)).timestamp_millis()}}})
                } else if keys {
                    json!({"data":[{"id":"shared-account-key-id"}]})
                } else if messages {
                    json!({"type":"message","role":"assistant","content":[{"type":"text","text":"OK"}],
                        "stop_reason":"end_turn","usage":{"input_tokens":18,"cache_read_input_tokens":1382,"output_tokens":4}})
                } else {
                    json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}]})
                };
                Ok(HttpResponse {
                    status: if nvidia && !catalog && self.nvidia_status != 0 {
                        self.nvidia_status
                    } else if self.fail {
                        401
                    } else if self.router_busy.is_some() && !catalog {
                        429
                    } else {
                        200
                    },
                    headers: Vec::new(),
                    body: Box::pin(stream::iter([Ok(body.to_string().into())])),
                })
            })
        }
    }

    fn accounts(net: Arc<Mock>) -> (Arc<ApiKeyAccounts>, std::path::PathBuf) {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("nabu-agentrouter-{:x}", u64::from_ne_bytes(random)));
        let (events, _) = broadcast::channel(32);
        (
            ApiKeyAccounts::load(KeyProvider::AgentRouter, &path, net, events),
            path,
        )
    }

    #[tokio::test]
    async fn check_all_starts_another_key_when_one_finishes() {
        let net = Arc::new(Mock {
            catalog_permits: Some(tokio::sync::Semaphore::new(0)),
            ..Default::default()
        });
        let permits = net.catalog_permits.as_ref().unwrap();
        let (accounts, path) = accounts(net.clone());
        accounts
            .add((0..5).map(|i| format!("key-{i}")).collect())
            .unwrap();

        let mut checks = std::pin::pin!(accounts.check_all());
        assert!(futures_util::poll!(&mut checks).is_pending());
        assert_eq!(net.seen.lock().unwrap().len(), 4);

        permits.add_permits(1);
        assert!(futures_util::poll!(&mut checks).is_pending());
        assert_eq!(net.seen.lock().unwrap().len(), 6);
        assert_eq!(
            accounts
                .list()
                .iter()
                .filter(|a| a.status == AccountStatus::Work)
                .count(),
            1
        );

        permits.add_permits(4);
        checks.await;
        assert!(
            accounts
                .list()
                .iter()
                .all(|a| a.status == AccountStatus::Work)
        );
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn adding_checks_only_new_keys() {
        let net = Arc::new(Mock::default());
        let (accounts, path) = accounts(net.clone());
        accounts.add(vec!["old-key".into()]).unwrap();

        let added = accounts
            .add(vec!["old-key".into(), "new-key".into()])
            .unwrap();
        accounts.check_added(added).await;
        let duplicate = accounts.add(vec!["new-key".into()]).unwrap();
        accounts.check_added(duplicate).await;

        let entries = accounts.list();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, AccountStatus::Unchecked);
        assert_eq!(entries[1].status, AccountStatus::Work);
        {
            let seen = net.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert!(seen.iter().all(|request| {
                header(&request.headers, "authorization") == Some("Bearer new-key")
            }));
        }
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn keys_are_checked_routed_and_never_returned_to_ui() {
        let net = Arc::new(Mock::default());
        let (accounts, path) = accounts(net.clone());
        accounts
            .add(vec![
                "secret-key-one".into(),
                "secret-key-two".into(),
                "secret-key-one".into(),
            ])
            .unwrap();
        assert_eq!(accounts.list().len(), 2);
        accounts.check_all().await;
        assert!(
            accounts
                .list()
                .iter()
                .all(|a| a.status == AccountStatus::Work)
        );
        assert!(
            !serde_json::to_string(&accounts.list())
                .unwrap()
                .contains("secret-key")
        );
        let provider = accounts.provider().unwrap();
        assert_eq!(provider.models.len(), 3);
        assert_eq!(
            provider
                .models
                .iter()
                .map(|m| (m.id.as_str(), m.older))
                .collect::<Vec<_>>(),
            [
                ("gpt-6-astra", false),
                ("gpt-6.1-sol", false),
                ("gpt-5.6-sol", false),
            ]
        );
        assert!(provider.models.iter().any(|m| m.name == "GPT-6.1-Sol"));
        {
            let seen = net.seen.lock().unwrap();
            assert_eq!(seen.len(), 4);
            assert!(
                seen.iter()
                    .all(|r| header(&r.headers, "originator") == Some("codex_cli_rs")
                        && header(&r.headers, "user-agent").is_some())
            );
        }
        let model = ModelRef {
            provider_id: "agentrouter".into(),
            model_id: "gpt-6.1-sol".into(),
            variant: None,
            fast: false,
        };
        let mut request = ir::Request::default();
        let first = accounts.route("a", &model, &mut request).unwrap();
        let sticky = accounts.route("a", &model, &mut request).unwrap();
        let second = accounts.route("b", &model, &mut request).unwrap();
        assert_eq!(first.account, sticky.account);
        assert_ne!(first.account, second.account);
        for message in [ROUTER_BUSY, "Too many requests"] {
            let error = ProviderError::parse(
                Format::OpenAiResponses,
                Some(429),
                json!({"error":{"message":message}}).to_string().into(),
            );
            assert!(!accounts.limited(first.account.as_deref().unwrap(), &error));
        }
        assert_eq!(
            accounts.route("a", &model, &mut request).unwrap().account,
            first.account
        );
        let error = ProviderError::parse(
            Format::OpenAiResponses,
            Some(429),
            json!({"error":{"type":"rate_limit_error","code":"insufficient_quota","message":"Quota exhausted"}})
                .to_string()
                .into(),
        );
        assert!(accounts.limited(first.account.as_deref().unwrap(), &error));
        assert_eq!(
            accounts.route("a", &model, &mut request).unwrap().account,
            second.account
        );
        accounts.check_limited().await;
        assert!(
            accounts
                .list()
                .iter()
                .all(|a| a.status == AccountStatus::Work)
        );
        let label = accounts.list()[0].email.clone();
        accounts.remove(Some(&label));
        assert_eq!(accounts.list().len(), 1);
        accounts.remove(None);
        assert!(accounts.route("a", &model, &mut request).is_err());
        assert!(accounts.provider().is_none());
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn agentrouter_key_stays_usable_when_the_upstream_is_busy_during_check() {
        let (accounts, path) = accounts(Arc::new(Mock {
            router_busy: Some("Too many requests"),
            ..Default::default()
        }));
        accounts.add(vec!["router-key".into()]).unwrap();
        accounts.check_all().await;
        assert_eq!(accounts.list()[0].status, AccountStatus::Work);
        assert!(
            accounts
                .provider()
                .unwrap()
                .models
                .iter()
                .any(|m| m.id == "gpt-6.1-sol")
        );
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn rejected_key_is_not_routable() {
        let (accounts, path) = accounts(Arc::new(Mock {
            fail: true,
            ..Default::default()
        }));
        accounts.add(vec!["invalid-key".into()]).unwrap();
        accounts.check_all().await;
        assert!(matches!(
            accounts.list()[0].status,
            AccountStatus::Error { .. }
        ));
        assert!(accounts.provider().is_none());
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn nvidia_checks_chat_even_when_catalog_accepts_invalid_keys() {
        for status in [200, 403] {
            let net = Arc::new(Mock {
                nvidia_status: status,
                ..Default::default()
            });
            let (_, path) = accounts(net.clone());
            let (events, mut receiver) = broadcast::channel(32);
            let nvidia = ApiKeyAccounts::load(KeyProvider::NvidiaBuild, &path, net.clone(), events);
            nvidia.add(vec!["nvidia-test-key".into()]).unwrap();
            assert!(matches!(
                receiver.try_recv(),
                Ok(Event::NvidiaBuildAccounts { .. })
            ));
            nvidia.check_all().await;
            if status == 403 {
                assert!(matches!(
                    nvidia.list()[0].status,
                    AccountStatus::Error { .. }
                ));
                assert!(nvidia.provider().is_none());
            } else {
                assert_eq!(nvidia.list()[0].status, AccountStatus::Work);
                let provider = nvidia.provider().unwrap();
                assert_eq!(provider.id, "nvidiabuild");
                assert_eq!(provider.models.len(), 3);
                assert!(
                    provider
                        .models
                        .iter()
                        .any(|m| m.id == "moonshotai/kimi-k3.1-pro")
                );
                let mut request = ir::Request {
                    model: "nvidia/nemotron-3-ultra-550b-a55b".into(),
                    max_tokens: Some(32),
                    messages: vec![ir::Message::new(
                        ir::Role::User,
                        vec![ir::Block::text("Hello")],
                    )],
                    ..Default::default()
                };
                let route = nvidia
                    .route(
                        "session",
                        &ModelRef {
                            provider_id: provider.id,
                            model_id: request.model.clone(),
                            variant: Some("none".into()),
                            fast: false,
                        },
                        &mut request,
                    )
                    .unwrap();
                let mut generation = nvidia
                    .core
                    .generate(
                        &route.endpoint,
                        &request,
                        &route.options,
                        &CancelToken::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    ir::plain_text(&generation.take_response().unwrap().content),
                    "OK"
                );
            }
            {
                let seen = net.seen.lock().unwrap();
                assert_eq!(seen[0].url, "https://integrate.api.nvidia.com/v1/models");
                assert_eq!(
                    seen[1].url,
                    "https://integrate.api.nvidia.com/v1/chat/completions"
                );
                assert!(
                    seen.iter()
                        .all(|r| header(&r.headers, "authorization")
                            == Some("Bearer nvidia-test-key"))
                );
                let probe: Value = serde_json::from_slice(&seen[1].body).unwrap();
                assert_eq!(probe["model"], "nvidia/nemotron-3.5-bolt-e4b");
                if status == 200 {
                    let body: Value = serde_json::from_slice(&seen[2].body).unwrap();
                    assert_eq!(body["reasoning_effort"], "none");
                    assert_eq!(body["max_tokens"], 32);
                }
            }
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn nvidia_catalog_uses_model_specific_capabilities() {
        let vision = nvidia_model(&json!({"id":"nvidia/nemotron-3.5-super-vl-120b-a12b"})).unwrap();
        assert!(vision.images);
        assert_eq!(vision.efforts, ["none", "low", "high", "max"]);
        let slow = nvidia_model(&json!({"id":"z-ai/glm-5.3-flash"})).unwrap();
        assert!(!slow.images);
        assert!(!slow.older);
        assert!(slow.efforts.is_empty());
        assert!(nvidia_model(&json!({"id":"deepseek-ai/deepseek-v4.1-flash"})).is_none());
        assert!(nvidia_model(&json!({"id":"other/m1", "model_type":"embedding"})).is_none());
    }

    #[test]
    fn coarena_catalog_keeps_provider_capabilities() {
        let model = KeyProvider::CoArena
            .catalog_model(&json!({
                "id": "claude-fable-5-2", "display_name": "Claude Fable 5.2",
                "context_length": 128000, "max_completion_tokens": 16384,
                "capabilities": {"vision": false, "efforts": ["low", "high", "max"]}
            }))
            .unwrap();
        assert_eq!(model.name, "Claude Fable 5.2");
        assert_eq!(model.family, "claude");
        assert_eq!((model.context, model.output), (128000, 16384));
        assert!(!model.images);
        assert_eq!(model.efforts, ["low", "high", "max"]);
    }

    #[test]
    fn coarena_experimental_context_is_saved_and_limited_to_verified_models() {
        let net = Arc::new(Mock::default());
        let (_, path) = accounts(net.clone());
        let (events, mut receiver) = broadcast::channel(8);
        let load =
            || ApiKeyAccounts::load(KeyProvider::CoArena, &path, net.clone(), events.clone());
        let coarena = load();
        let mut account = entry("key-id".into(), "coarena-key");
        for (id, context) in [
            ("claude-opus-5-5", 128_000),
            ("gemini-4-carbon", 128_000),
            ("grok-4-7", 128_000),
            ("kimi-k3-1-pro", 128_000),
            ("claude-sonnet-5-5", 200_000),
        ] {
            account.models.push(
                KeyProvider::CoArena
                    .catalog_model(&json!({
                        "id": id, "context_length": context, "max_completion_tokens": 16384
                    }))
                    .unwrap(),
            );
        }
        coarena.state.lock().unwrap().entries.push(account);
        let context = |id| {
            coarena
                .provider()
                .unwrap()
                .models
                .into_iter()
                .find(|model| model.id == id)
                .unwrap()
                .context
        };

        assert!(!coarena.experimental_context());
        assert_eq!(context("claude-opus-5-5"), 128_000);
        coarena.set_experimental_context(true).unwrap();
        assert!(matches!(receiver.try_recv(), Ok(Event::ProvidersChanged)));
        assert!(load().experimental_context());
        assert_eq!(context("claude-opus-5-5"), 135_000);
        assert_eq!(context("gemini-4-carbon"), 135_000);
        assert_eq!(context("grok-4-7"), 128_000);
        assert_eq!(context("kimi-k3-1-pro"), 128_000);
        assert_eq!(context("claude-sonnet-5-5"), 200_000);
        coarena.set_experimental_context(false).unwrap();
        assert!(!load().experimental_context());
        assert_eq!(context("claude-opus-5-5"), 128_000);

        std::fs::remove_file(&coarena.context_path).unwrap();
        std::fs::remove_dir(path).unwrap();
    }

    #[tokio::test]
    async fn coarena_uses_messages_with_cache_and_an_independent_key_store() {
        let net = Arc::new(Mock::default());
        let (agentrouter, path) = accounts(net.clone());
        let (events, mut receiver) = broadcast::channel(32);
        let coarena = ApiKeyAccounts::load(KeyProvider::CoArena, &path, net.clone(), events);
        agentrouter.add(vec!["router-key".into()]).unwrap();
        coarena
            .add(vec!["coarena-key".into(), "coarena-other-key".into()])
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::CoArenaAccounts { .. })
        ));
        coarena.check_all().await;
        assert!(
            coarena
                .list()
                .iter()
                .all(|account| account.status == AccountStatus::Work)
        );
        let quota = coarena.coarena_quota();
        assert_eq!(
            (quota.total_usd, quota.used_usd, quota.partial),
            (5.0, 2.0, false)
        );
        assert_eq!(quota.remaining_usd, 3.0);
        assert!(quota.reset.is_some());
        assert_eq!(coarena.provider().unwrap().id, "coarena");
        assert_eq!(agentrouter.list()[0].status, AccountStatus::Unchecked);
        let model = ModelRef {
            provider_id: "coarena".into(),
            model_id: "gpt-6.1-sol".into(),
            variant: None,
            fast: false,
        };
        let mut request = ir::Request::default();
        let route = coarena.route("chat", &model, &mut request).unwrap();
        request.model = model.model_id.clone();
        request.messages.push(ir::Message::new(
            ir::Role::User,
            vec![ir::Block::text("Reply only OK.")],
        ));
        let mut generation = coarena
            .core
            .generate(
                &route.endpoint,
                &request,
                &route.options,
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let response = generation.take_response().unwrap();
        assert_eq!(ir::plain_text(&response.content), "OK");
        assert_eq!(response.usage.cache_read, Some(1382));
        {
            let seen = net.seen.lock().unwrap();
            assert_eq!(seen.len(), 7);
            for path in ["limits", "models", "keys"] {
                let url = format!("https://coarena.ai/api/v1/{path}");
                assert_eq!(
                    seen[..6]
                        .iter()
                        .filter(|request| request.url == url && request.method == "GET")
                        .count(),
                    2
                );
            }
            assert_eq!(seen[6].url, "https://coarena.ai/api/v1/messages");
            assert_eq!(
                header(&seen[6].headers, "anthropic-version"),
                Some("2023-06-01")
            );
            assert!(seen.iter().all(|request| matches!(
                header(&request.headers, "authorization"),
                Some("Bearer coarena-key" | "Bearer coarena-other-key")
            )
                && header(&request.headers, "originator").is_none()));
            let body: Value = serde_json::from_slice(&seen[6].body).unwrap();
            assert_eq!(body["messages"][0]["content"][0]["text"], "Reply only OK.");
            assert_eq!(
                body["messages"][0]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );
            assert_eq!(body["max_tokens"], 16384);
            assert!(body.get("input").is_none());
        }
        request.max_tokens = Some(1024);
        coarena.route("chat", &model, &mut request).unwrap();
        assert_eq!(request.max_tokens, Some(1024));

        coarena.remove(None);
        assert_eq!(coarena.coarena_quota().total_usd, 0.0);
        assert!(coarena.provider().is_none());
        assert!(coarena.route("chat", &model, &mut request).is_err());
        assert_eq!(agentrouter.list().len(), 1);
        agentrouter.remove(None);
        let _ = std::fs::remove_file(path.join("coarena-keys.json"));
        let _ = std::fs::remove_file(path.join("agentrouter-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn coarena_checks_use_the_saved_route_and_keep_quota_when_rate_limited() {
        let direct = Arc::new(Mock::default());
        let proxy = Arc::new(Mock {
            router_busy: Some(ROUTER_BUSY),
            ..Mock::default()
        });
        let (_, path) = accounts(proxy.clone());
        let load = || {
            ApiKeyAccounts::load(
                KeyProvider::CoArena,
                &path,
                proxy.clone(),
                broadcast::channel(32).0,
            )
        };
        assert!(!load().direct_checks());
        load().set_direct_checks(true).unwrap();
        let mut coarena = load();
        assert!(coarena.direct_checks());
        Arc::get_mut(&mut coarena).unwrap().direct_core =
            Some(Core::builder(direct.clone()).build());
        coarena.add(vec!["coarena-key".into()]).unwrap();
        coarena.check_all().await;

        let requests = direct.seen.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.method == "GET"));
        drop(requests);
        assert!(proxy.seen.lock().unwrap().is_empty());
        assert_eq!(coarena.coarena_quota().total_usd, 5.0);

        coarena.set_direct_checks(false).unwrap();
        assert!(!load().direct_checks());
        coarena.check_all().await;
        assert_eq!(proxy.seen.lock().unwrap().len(), 1);
        assert!(matches!(
            coarena.list()[0].status,
            AccountStatus::Limit { .. }
        ));
        assert_eq!(coarena.coarena_quota().total_usd, 5.0);

        Arc::get_mut(&mut coarena).unwrap().core = Core::builder(Arc::new(Mock {
            fail: true,
            ..Mock::default()
        }))
        .build();
        coarena.check_all().await;
        assert!(matches!(
            coarena.list()[0].status,
            AccountStatus::Error { .. }
        ));
        assert_eq!(coarena.coarena_quota().total_usd, 0.0);
        let _ = std::fs::remove_file(path.join("coarena-keys.json"));
        let _ = std::fs::remove_file(path.join("coarena-direct-checks.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    async fn coarena_check_stops_background_refresh_and_keeps_the_account() {
        let net = Arc::new(Mock {
            catalog_permits: Some(tokio::sync::Semaphore::new(0)),
            ..Default::default()
        });
        let (_, path) = accounts(net.clone());
        let coarena = ApiKeyAccounts::load(
            KeyProvider::CoArena,
            &path,
            net.clone(),
            broadcast::channel(32).0,
        );
        coarena.add(vec!["coarena-key".into()]).unwrap();
        let key_requests = || {
            net.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.url.ends_with("/v1/keys"))
                .count()
        };

        let mut refresh = std::pin::pin!(coarena.refresh_coarena());
        assert!(futures_util::poll!(&mut refresh).is_pending());
        let mut check = std::pin::pin!(coarena.check_all());
        assert!(futures_util::poll!(&mut check).is_pending());
        assert!(futures_util::poll!(&mut refresh).is_ready());
        assert!(futures_util::poll!(&mut check).is_pending());
        assert_eq!(coarena.list()[0].status, AccountStatus::Checking);
        net.catalog_permits.as_ref().unwrap().add_permits(2);
        check.await;
        assert_eq!(coarena.list()[0].status, AccountStatus::Work);
        assert!(!coarena.coarena_quota().partial);

        let fetched = key_requests();
        coarena.check_all().await;
        assert_eq!(key_requests(), fetched);
        coarena.remove(None);
        let _ = std::fs::remove_file(path.join("coarena-keys.json"));
        let _ = std::fs::remove_dir(path);
    }

    #[tokio::test]
    #[ignore = "requires NABU_NVIDIA_BUILD_KEY and generates model tokens"]
    async fn nvidia_live_streaming() {
        let key = std::env::var("NABU_NVIDIA_BUILD_KEY").expect("set NABU_NVIDIA_BUILD_KEY");
        let provider = KeyProvider::NvidiaBuild;
        let accounts = ApiKeyAccounts::load(
            provider,
            &std::env::temp_dir().join("nabu-nvidia-live"),
            Arc::new(HyperTransport::with_proxy(crate::proxy_from_env())),
            broadcast::channel(8).0,
        );
        let models = accounts
            .check_nvidia(&key)
            .await
            .expect("Nvidia key check failed");
        let model = "nvidia/nemotron-3.5-bolt-e4b";
        assert!(models.iter().any(|m| m.id == model));
        let request = ir::Request {
            model: model.into(),
            stream: true,
            max_tokens: Some(32),
            messages: vec![ir::Message::new(
                ir::Role::User,
                vec![ir::Block::text("Reply only OK.")],
            )],
            ..Default::default()
        };
        tokio::time::timeout(Duration::from_secs(45), async {
            let mut generation = accounts
                .core
                .generate(
                    &provider.endpoint(&key),
                    &request,
                    &provider.generate_options(),
                    &CancelToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(generation.status, 200);
            let mut acc = ir::Accumulator::new();
            while let Some(event) = generation.next().await {
                acc.push(&event.unwrap());
            }
            let result = generation.take_response().unwrap_or_else(|| acc.finish());
            assert!(!ir::plain_text(&result.content).trim().is_empty());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires NABU_COARENA_KEY and generates model tokens"]
    async fn coarena_live_generation() {
        use futures_util::StreamExt;
        let key = std::env::var("NABU_COARENA_KEY").expect("set NABU_COARENA_KEY");
        let model = std::env::var("NABU_COARENA_MODEL").expect("set NABU_COARENA_MODEL");
        let core = Core::builder(Arc::new(ncore_tokio::HyperTransport::with_proxy(
            crate::proxy_from_env(),
        )))
        .build();
        let provider = KeyProvider::CoArena;
        let catalog = ApiKeyAccounts::load(
            provider,
            &std::env::temp_dir().join("nabu-coarena-live"),
            Arc::new(ncore_tokio::HyperTransport::with_proxy(
                crate::proxy_from_env(),
            )),
            broadcast::channel(32).0,
        );
        catalog
            .fetch_coarena_limits(&key)
            .await
            .expect("key check failed");
        let models = catalog.catalog(&key).await.expect("model catalog failed");
        assert!(models.iter().any(|item| item.id == model));
        for stream in [false, true] {
            let request = ir::Request {
                model: model.clone(),
                stream,
                max_tokens: Some(128),
                messages: vec![ir::Message::new(
                    ir::Role::User,
                    vec![ir::Block::text("Reply only OK.")],
                )],
                ..Default::default()
            };
            let mut generation = tokio::time::timeout(
                Duration::from_secs(45),
                core.generate(
                    &provider.endpoint(&key),
                    &request,
                    &provider.generate_options(),
                    &CancelToken::new(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(generation.status, 200);
            let mut acc = ir::Accumulator::new();
            tokio::time::timeout(Duration::from_secs(45), async {
                while let Some(event) = generation.next().await {
                    acc.push(&event.unwrap());
                }
            })
            .await
            .unwrap();
            let response = generation.take_response().unwrap_or_else(|| acc.finish());
            assert_eq!(ir::plain_text(&response.content).trim(), "OK");
            println!("CoArena {model} stream={stream}: OK");
        }
    }
}
