mod check;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use nabu_protocol::{
    Account as AccountRow, AccountStatus, Event, Model, ModelRef, Provider, Quota,
};
use ncore::core::ir::{self, ReasoningCfg};
use ncore::rt::{BoxFuture, boxed};
use ncore::{Endpoint, Format, GenerateOptions};
use ncore_chatgptsub::{ChatGptSub, CredentialStore, Credentials, MemoryStore, oauth};
use ncore_tokio::HyperTransport;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast;

use crate::accounts_file::{AccountStore, AccountsFile};
use crate::api_keys::{read_switch, write_switch};
use crate::callback::Callback;
use crate::models::Route;

struct Entry {
    key: String,
    email: Option<String>,
    plugin: Arc<ChatGptSub>,
    plan: Option<String>,
    status: AccountStatus,
    models: Vec<Model>,
    quota: Vec<Quota>,
}

impl Entry {
    fn is_free(&self) -> bool {
        self.plan
            .as_deref()
            .is_some_and(|plan| plan.eq_ignore_ascii_case("free"))
    }

    fn apply_check(
        &mut self,
        models: Result<Vec<Model>, String>,
        usage: check::Usage,
        email: Option<String>,
    ) {
        self.email = email.or(self.email.take());
        self.plan = usage.plan;
        self.status = status(&models, &usage.quota);
        self.models = models.unwrap_or_default();
        self.quota = usage.quota;
    }
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    login: Option<tokio::task::JoinHandle<()>>,
    /// Account each session talks through, so its prompt cache keeps working.
    sessions: HashMap<String, String>,
    prefer_free: bool,
}

/// ChatGPT subscription accounts (Plus, Pro and others); requests go to one of the working ones.
pub struct ChatGptAccounts {
    file: Arc<AccountsFile<Credentials>>,
    prefer_free_path: PathBuf,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    version: tokio::sync::Mutex<check::CodexVersion>,
}

impl ChatGptAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(dir.join("chatgpt-accounts.json"), None);
        let entries = file
            .list()
            .into_iter()
            .map(|s| entry(&file, s.key, s.email))
            .collect();
        let prefer_free_path = dir.join("chatgpt-prefer-free.json");
        let prefer_free = read_switch(&prefer_free_path);
        Arc::new(Self {
            file,
            prefer_free_path,
            net,
            events,
            state: Mutex::new(State {
                entries,
                prefer_free,
                ..State::default()
            }),
            version: Default::default(),
        })
    }

    pub fn list(&self) -> Vec<AccountRow> {
        let state = self.state.lock().unwrap();
        state
            .entries
            .iter()
            .map(|e| AccountRow {
                email: e.email.clone().unwrap_or_default(),
                id: None,
                max_credits: None,
                tier: e.plan.clone(),
                status: e.status.clone(),
            })
            .collect()
    }

    pub fn prefer_free(&self) -> bool {
        self.state.lock().unwrap().prefer_free
    }

    pub fn set_prefer_free(&self, enabled: bool) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.prefer_free != enabled {
            write_switch(&self.prefer_free_path, enabled)?;
            state.prefer_free = enabled;
        }
        Ok(())
    }

    pub fn quota(&self) -> Vec<Quota> {
        let state = self.state.lock().unwrap();
        crate::quota::combine(state.entries.iter().flat_map(|e| &e.quota))
    }

    /// The provider, present while some account is signed in (a used-up one included); models are the union over them.
    pub fn provider(&self) -> Option<Provider> {
        let state = self.state.lock().unwrap();
        let mut models: Vec<Model> = Vec::new();
        for entry in state
            .entries
            .iter()
            .filter(|e| !matches!(e.status, AccountStatus::Error { .. }))
        {
            for model in &entry.models {
                if !models.iter().any(|m| m.id == model.id) {
                    models.push(model.clone());
                }
            }
        }
        if models.is_empty() {
            return None;
        }
        Some(Provider {
            id: "chatgpt-account".into(),
            name: "ChatGPT".into(),
            models,
            custom: false,
        })
    }

    /// Starts a sign-in: the browser comes back to a local page, and the account is added from there.
    pub async fn login(self: &Arc<Self>) -> Result<String, String> {
        if let Some(previous) = self.state.lock().unwrap().login.take() {
            previous.abort();
        }
        let callback = Callback::bind(&[1455], "/auth/callback")
            .await
            .map_err(|e| format!("Could not open the sign-in page on port 1455 (is Codex signing in right now?): {e}"))?;
        let start = oauth::start_login().map_err(|e| e.to_string())?;
        let url = start.url.clone();
        let state = query_value(&url, "state").ok_or("The sign-in link has no state")?;
        let this = self.clone();
        let task = tokio::spawn(async move {
            let result = match callback.code(state.clone()).await {
                Ok(code) => oauth::finish_login(&*this.net, start, &code, &state)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            match result {
                Ok(credentials) => this.add(credentials).await,
                Err(message) => {
                    let _ = this.events.send(Event::ChatGptLoginFailed { message });
                }
            }
        });
        self.state.lock().unwrap().login = Some(task);
        Ok(url)
    }

    async fn add(&self, credentials: Credentials) {
        let key = self.file.add(credentials);
        self.state
            .lock()
            .unwrap()
            .entries
            .push(entry(&self.file, key.clone(), None));
        self.check(&[key]).await;
    }

    pub async fn import_auth(&self, path: &Path) -> Result<(), String> {
        const MAX_AUTH_FILE_SIZE: u64 = 256 * 1024;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| "Could not open the selected auth.json")?;
        let mut bytes = Vec::new();
        file.take(MAX_AUTH_FILE_SIZE + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Could not read the selected auth.json")?;
        if bytes.len() as u64 > MAX_AUTH_FILE_SIZE {
            return Err("The selected auth.json is too large".into());
        }
        let credentials = Credentials::from_codex_json(&bytes)?;

        // Verify the copied access token before saving without rotating the original refresh token.
        let probe = ChatGptSub::new(Arc::new(MemoryStore::new(Credentials {
            refresh_token: None,
            ..credentials.clone()
        })));
        let mut version = self.version.lock().await;
        version.refresh_if_due().await;
        let models = check::models(&probe, &self.net, version.current()).await?;
        if models.is_empty() {
            return Err("ChatGPT returned no available models for the imported account".into());
        }
        let usage = check::usage(&probe, &self.net).await;
        let email = usage
            .email
            .clone()
            .or_else(|| check::email(&credentials.access_token));
        {
            let mut state = self.state.lock().unwrap();
            let key = self.file.try_add(credentials)?;
            if let Some(email) = &email {
                self.file.set_email(&key, email);
            }
            let mut imported = entry(&self.file, key, None);
            imported.apply_check(Ok(models), usage, email);
            state.entries.push(imported);
        }
        self.drop_duplicates();
        self.changed(true);
        Ok(())
    }

    pub fn remove(&self, email: &str) {
        let keys: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|e| e.email.as_deref().unwrap_or("") == email)
                .map(|e| e.key.clone())
                .collect()
        };
        self.drop_keys(&keys);
    }

    pub fn remove_all(&self) {
        let keys: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.key.clone())
            .collect();
        self.drop_keys(&keys);
    }

    fn drop_keys(&self, keys: &[String]) {
        for key in keys {
            self.file.remove(key);
        }
        {
            let mut state = self.state.lock().unwrap();
            state.entries.retain(|e| !keys.contains(&e.key));
            state.sessions.retain(|_, key| !keys.contains(key));
        }
        self.changed(true);
    }

    pub async fn check_all(&self) {
        let keys: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.key.clone())
            .collect();
        self.check(&keys).await;
    }

    pub async fn check_periodic(&self) {
        self.refresh_catalog().await;
        let keys: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|e| matches!(e.status, AccountStatus::Limit { .. }))
                .map(|e| e.key.clone())
                .collect()
        };
        if !keys.is_empty() {
            self.check(&keys).await;
        }
    }

    async fn refresh_catalog(&self) {
        let mut version = self.version.lock().await;
        if self.state.lock().unwrap().entries.is_empty() || !version.refresh_if_due().await {
            return;
        }

        let plugins: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|entry| entry.status == AccountStatus::Work)
            .map(|entry| (entry.key.clone(), entry.plugin.clone()))
            .collect();
        let version = version.current();
        let updates = join_all(plugins.into_iter().map(|(key, plugin)| async move {
            (key, check::models(&plugin, &self.net, version).await)
        }))
        .await;

        let mut changed = false;
        {
            let mut state = self.state.lock().unwrap();
            for (key, models) in updates {
                if let Ok(models) = models
                    && !models.is_empty()
                    && let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key)
                {
                    entry.models = models;
                    changed = true;
                }
            }
        }
        if changed {
            let _ = self.events.send(Event::ProvidersChanged);
        }
    }

    /// Reads only the quota of every account; an account whose read fails keeps its last one.
    pub async fn refresh_quota(&self) {
        let plugins: Vec<(String, Arc<ChatGptSub>)> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.plugin.clone()))
            .collect();
        let usages = join_all(
            plugins
                .iter()
                .map(|(_, plugin)| check::usage(plugin, &self.net)),
        )
        .await;
        {
            let mut state = self.state.lock().unwrap();
            for ((key, _), usage) in plugins.into_iter().zip(usages) {
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key)
                    && !usage.quota.is_empty()
                {
                    entry.quota = usage.quota;
                }
            }
        }
        self.changed(false);
    }

    /// Reads models, plan, email and quota; an account works once its models come back.
    async fn check(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut version = self.version.lock().await;
        version.refresh_if_due().await;
        let version = version.current();
        let plugins: Vec<(String, Arc<ChatGptSub>)> = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .filter(|e| keys.contains(&e.key))
                .map(|e| {
                    e.status = AccountStatus::Checking;
                    (e.key.clone(), e.plugin.clone())
                })
                .collect()
        };
        self.changed(false);

        join_all(plugins.into_iter().map(|(key, plugin)| async move {
            let models = check::models(&plugin, &self.net, version).await;
            let usage = check::usage(&plugin, &self.net).await;
            let saved = self.file.credentials(&key);
            let email = usage
                .email
                .clone()
                .or_else(|| saved.as_ref().and_then(|c| check::email(&c.access_token)));
            if let Some(email) = &email {
                self.file.set_email(&key, email);
            }
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                    entry.apply_check(models, usage, email);
                }
            }
            self.changed(false);
        }))
        .await;
        self.drop_duplicates();
        self.changed(true);
    }

    /// Signing in to an account again replaces its earlier entry.
    fn drop_duplicates(&self) {
        let stale: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| {
                    e.email.is_some()
                        && state.entries[i + 1..]
                            .iter()
                            .any(|later| later.email == e.email)
                })
                .map(|(_, e)| e.key.clone())
                .collect()
        };
        if !stale.is_empty() {
            self.drop_keys(&stale);
        }
    }

    /// Picks the account for `session`: the one it used before if it still works, else the one with most quota left.
    /// With Free preferred, paid accounts are skipped while a working Free one has the model.
    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        let mut state = self.state.lock().unwrap();
        let mut pool: Vec<&Entry> = state
            .entries
            .iter()
            .filter(|e| {
                e.status == AccountStatus::Work && e.models.iter().any(|m| m.id == model.model_id)
            })
            .collect();
        if state.prefer_free && pool.iter().any(|e| e.is_free()) {
            pool.retain(|e| e.is_free());
        }
        let previous = state
            .sessions
            .get(session)
            .and_then(|key| pool.iter().copied().find(|e| e.key == *key));
        let entry = previous
            .or_else(|| {
                pool.iter()
                    .copied()
                    .max_by(|a, b| headroom(a).total_cmp(&headroom(b)))
            })
            .ok_or_else(|| no_account(&state.entries))?;
        let known = entry.models.iter().find(|m| m.id == model.model_id);

        let effort = model
            .variant
            .clone()
            .filter(|e| known.is_some_and(|m| m.efforts.contains(e)));
        request.reasoning = Some(ReasoningCfg {
            effort,
            include: Some(true),
            ..ReasoningCfg::default()
        });
        request.ext.format = Some(Format::OpenAiResponses);
        // The backend keys its prompt cache by this; one key per session keeps the cache warm between steps.
        request
            .ext
            .fields
            .insert("prompt_cache_key".into(), json!(session));
        if model.fast && known.is_some_and(|m| m.fast) {
            request
                .ext
                .fields
                .insert("service_tier".into(), json!("priority"));
        }
        let route = Route {
            endpoint: endpoint(&entry.plugin),
            options: GenerateOptions::default(),
            account: Some(entry.key.clone()),
        };
        let key = entry.key.clone();
        state.sessions.insert(session.to_owned(), key);
        Ok(route)
    }

    /// The account hit its limit: requests go elsewhere until a later check finds quota again.
    pub fn limited(&self, key: &str, message: &str) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                entry.status = AccountStatus::Limit {
                    message: message.to_owned(),
                };
            }
            state.sessions.retain(|_, k| k != key);
        }
        self.changed(true);
    }

    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::ChatGptAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::ChatGptQuota {
            quota: self.quota(),
        });
        if providers {
            let _ = self.events.send(Event::ProvidersChanged);
        }
    }
}

impl CredentialStore for AccountStore<Credentials> {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async move {
            self.file
                .credentials(&self.key)
                .ok_or_else(|| "account removed".to_owned())
        })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move { self.file.try_replace(&self.key, credentials) })
    }
}

fn entry(file: &Arc<AccountsFile<Credentials>>, key: String, email: Option<String>) -> Entry {
    let store = Arc::new(AccountStore {
        file: file.clone(),
        key: key.clone(),
    });
    Entry {
        key,
        email,
        plugin: Arc::new(ChatGptSub::new(store)),
        plan: None,
        status: AccountStatus::Unchecked,
        models: Vec::new(),
        quota: Vec::new(),
    }
}

fn endpoint(plugin: &Arc<ChatGptSub>) -> Endpoint {
    ChatGptSub::endpoint().with_plugin(plugin.clone())
}

/// Share left in the tightest window.
fn headroom(entry: &Entry) -> f64 {
    entry.quota.iter().map(|q| q.remaining).fold(1.0, f64::min)
}

/// An account with a window used up is at its limit even before a request fails.
fn status(models: &Result<Vec<Model>, String>, quota: &[Quota]) -> AccountStatus {
    match models {
        Err(message) => AccountStatus::Error {
            message: message.clone(),
        },
        Ok(_) => match quota.iter().find(|q| q.remaining <= 0.0) {
            Some(q) => AccountStatus::Limit {
                message: format!(
                    "The {} window is used up until {}",
                    q.family,
                    q.reset.as_deref().unwrap_or("its reset")
                ),
            },
            None => AccountStatus::Work,
        },
    }
}

fn no_account(entries: &[Entry]) -> String {
    if entries.is_empty() {
        "Sign in to a ChatGPT account first".into()
    } else if entries
        .iter()
        .any(|e| matches!(e.status, AccountStatus::Limit { .. }))
    {
        "Every ChatGPT account with this model is at its limit".into()
    } else {
        "No working ChatGPT account has this model".into()
    }
}

/// A query value; the sign-in state is URL-safe base64, so it needs no decoding.
fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_owned())
    })
}
