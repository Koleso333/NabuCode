mod check;
mod store;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use nabu_protocol::{
    Account as AccountRow, AccountStatus, Event, Model, ModelRef, Provider, Quota,
};
use ncore::core::ir::{self, ReasoningCfg, ReasoningMode};
use ncore::{Core, Endpoint, Format, GenerateOptions};
use ncore_claudesub::{ClaudeSub, oauth};
use ncore_tokio::HyperTransport;
use serde_json::json;
use tokio::sync::broadcast;

use crate::accounts_file::{AccountStore, AccountsFile};
use crate::models::Route;
use ncore_claudesub::Credentials;

struct Entry {
    key: String,
    email: Option<String>,
    plugin: Arc<ClaudeSub>,
    plan: Option<String>,
    status: AccountStatus,
    models: Vec<Model>,
    quota: Vec<Quota>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    login: Option<oauth::LoginStart>,
    /// Account each session talks through, so its prompt cache keeps working.
    sessions: HashMap<String, String>,
    /// Fast mode support per model id; it belongs to the model, so one answer serves every account.
    fast: HashMap<String, bool>,
    /// Models that take adaptive thinking; the rest think only on a budget, which NabuCode does not set.
    adaptive: HashSet<String>,
}

/// Claude subscription accounts; requests go to one of the working ones.
pub struct ClaudeAccounts {
    file: Arc<AccountsFile<Credentials>>,
    net: Arc<HyperTransport>,
    core: Core,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
}

impl ClaudeAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(
            dir.join("claude-accounts.json"),
            Some(dir.join("claude-account.json")),
        );
        let entries = file
            .list()
            .into_iter()
            .map(|s| entry(&file, s.key, s.email))
            .collect();
        let core = Core::builder(net.clone()).build();
        Arc::new(Self {
            file,
            net,
            core,
            events,
            state: Mutex::new(State {
                entries,
                ..State::default()
            }),
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
        crate::models::sort_and_mark_older(&mut models);
        Some(Provider {
            id: "claude-account".into(),
            name: "Claude".into(),
            models,
            custom: false,
        })
    }

    /// Starts a sign-in; the browser page then shows a code to paste into `finish_login`.
    pub fn login(&self) -> Result<String, String> {
        let start = oauth::start_login().map_err(|e| e.to_string())?;
        let url = start.url.clone();
        self.state.lock().unwrap().login = Some(start);
        Ok(url)
    }

    /// Takes the `code#state` text from Claude's page.
    pub async fn finish_login(self: &Arc<Self>, pasted: &str) -> Result<(), String> {
        let (code, state) = pasted
            .trim()
            .split_once('#')
            .ok_or("Paste the whole code, including the part after #")?;
        let start = self
            .state
            .lock()
            .unwrap()
            .login
            .take()
            .ok_or("Start the sign-in again")?;
        let credentials = oauth::finish_login(&*self.net, start, code, state)
            .await
            .map_err(|e| e.to_string())?;
        let key = self.file.add(credentials);
        self.state
            .lock()
            .unwrap()
            .entries
            .push(entry(&self.file, key.clone(), None));
        self.check(&[key]).await;
        Ok(())
    }

    pub fn remove(&self, email: &str) {
        let keys: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .entries
                .iter()
                .filter(|e| e.email.as_deref() == Some(email))
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

    /// Accounts at their limit, checked again so they return once the window resets.
    pub async fn check_limited(&self) {
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

    /// Reads only the quota of every account; an account whose read fails keeps its last one.
    pub async fn refresh_quota(&self) {
        let plugins: Vec<(String, Arc<ClaudeSub>)> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.plugin.clone()))
            .collect();
        let quotas = join_all(
            plugins
                .iter()
                .map(|(_, plugin)| check::quota(plugin, &self.net)),
        )
        .await;
        {
            let mut state = self.state.lock().unwrap();
            for ((key, _), quota) in plugins.into_iter().zip(quotas) {
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key)
                    && !quota.is_empty()
                {
                    entry.quota = quota;
                }
            }
        }
        self.changed(false);
    }

    /// Reads email, plan, models and quota; an account works once its models come back.
    async fn check(&self, keys: &[String]) {
        let plugins: Vec<(String, Option<String>, Arc<ClaudeSub>)> = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .filter(|e| keys.contains(&e.key))
                .map(|e| {
                    e.status = AccountStatus::Checking;
                    (e.key.clone(), e.email.clone(), e.plugin.clone())
                })
                .collect()
        };
        self.changed(false);

        join_all(plugins.into_iter().map(|(key, email, plugin)| async move {
            let mut models =
                check::models(&self.core, &endpoint(&plugin))
                    .await
                    .map(|(models, adaptive)| {
                        self.state.lock().unwrap().adaptive.extend(adaptive);
                        models
                    });
            if let Ok(list) = models.as_mut() {
                self.mark_fast(&plugin, list).await;
            }
            let plan = plugin
                .profile(&*self.net)
                .await
                .ok()
                .and_then(|p| p.value.plan().map(str::to_owned));
            let quota = check::quota(&plugin, &self.net).await;
            let email = match email {
                Some(email) => Some(email),
                None => self.find_email(&key).await,
            };
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                    entry.email = email;
                    entry.plan = plan;
                    entry.status = status(&models, &quota);
                    entry.models = models.unwrap_or_default();
                    entry.quota = quota;
                }
            }
            self.changed(false);
        }))
        .await;
        self.drop_duplicates();
        self.changed(true);
    }

    async fn mark_fast(&self, plugin: &Arc<ClaudeSub>, models: &mut [Model]) {
        let unknown: Vec<String> = {
            let state = self.state.lock().unwrap();
            models
                .iter()
                .filter(|m| !state.fast.contains_key(&m.id))
                .map(|m| m.id.clone())
                .collect()
        };
        let endpoint = endpoint(plugin);
        let answers = join_all(
            unknown
                .iter()
                .map(|id| check::fast(&self.core, &endpoint, id)),
        )
        .await;
        let mut state = self.state.lock().unwrap();
        for (id, answer) in unknown.into_iter().zip(answers) {
            if let Some(fast) = answer {
                state.fast.insert(id, fast);
            }
        }
        for model in models {
            model.fast = state.fast.get(&model.id) == Some(&true);
        }
    }

    /// The token is fresh right after the checks above, so it can be used as is.
    async fn find_email(&self, key: &str) -> Option<String> {
        let saved = self.file.list().into_iter().find(|s| s.key == key)?;
        let email = check::email(&self.net, &saved.credentials.access_token).await?;
        self.file.set_email(key, &email);
        Some(email)
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
    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        let mut state = self.state.lock().unwrap();
        let usable = |e: &&Entry| {
            e.status == AccountStatus::Work && e.models.iter().any(|m| m.id == model.model_id)
        };
        let previous = state
            .sessions
            .get(session)
            .and_then(|key| state.entries.iter().filter(usable).find(|e| e.key == *key));
        let entry = previous
            .or_else(|| {
                state
                    .entries
                    .iter()
                    .filter(usable)
                    .max_by(|a, b| headroom(a).total_cmp(&headroom(b)))
            })
            .ok_or_else(|| no_account(&state.entries))?;
        let known = entry.models.iter().find(|m| m.id == model.model_id);

        let mut options = GenerateOptions::default();
        options.encode.default_max_tokens =
            known.map(|m| m.output).filter(|n| *n > 0).or(Some(32_000));
        options
            .headers
            .push((ncore_claudesub::SESSION_HEADER.into(), session.to_owned()));
        let effort = model
            .variant
            .clone()
            .filter(|e| known.is_some_and(|m| m.efforts.contains(e)));
        request.reasoning = if state.adaptive.contains(&model.model_id) {
            Some(ReasoningCfg {
                mode: Some(ReasoningMode::Adaptive),
                effort,
                include: Some(true),
                ..ReasoningCfg::default()
            })
        } else {
            effort.map(|effort| ReasoningCfg {
                effort: Some(effort),
                ..ReasoningCfg::default()
            })
        };
        if model.fast && known.is_some_and(|m| m.fast) {
            request.ext.format = Some(Format::AnthropicMessages);
            request.ext.fields.insert("speed".into(), json!("fast"));
            options
                .headers
                .push(("anthropic-beta".into(), "fast-mode-2026-02-01".into()));
        }
        let route = Route {
            endpoint: endpoint(&entry.plugin),
            options,
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
        let _ = self.events.send(Event::ClaudeAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::ClaudeQuota {
            quota: self.quota(),
        });
        if providers {
            let _ = self.events.send(Event::ProvidersChanged);
        }
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
        plugin: Arc::new(ClaudeSub::new(store)),
        plan: None,
        status: AccountStatus::Unchecked,
        models: Vec::new(),
        quota: Vec::new(),
    }
}

fn endpoint(plugin: &Arc<ClaudeSub>) -> Endpoint {
    ClaudeSub::endpoint().with_plugin(plugin.clone())
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
        "Sign in to a Claude account first".into()
    } else if entries
        .iter()
        .any(|e| matches!(e.status, AccountStatus::Limit { .. }))
    {
        "Every Claude account with this model is at its limit".into()
    } else {
        "No working Claude account has this model".into()
    }
}
