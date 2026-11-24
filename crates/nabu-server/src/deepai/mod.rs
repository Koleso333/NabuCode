mod check;
mod lane_store;

use crate::{
    accounts_file::{AccountStore, AccountsFile},
    models::Route,
};
use futures_util::future::join_all;
use nabu_protocol::{Account, AccountStatus, Credits, Event, Model, ModelRef, Provider};
use ncore::{
    GenerateOptions,
    core::{
        ErrorKind, ProviderError,
        ir::{self, ReasoningCfg},
    },
    rt::{BoxFuture, boxed},
};
use ncore_deepaiacc::{
    ClientInfo, CredentialStore, Credentials, DeepAIAcc, Lanes, ModelApi, Session, oauth,
};
use ncore_tokio::HyperTransport;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Clone, Serialize, Deserialize)]
struct SavedAccount {
    credentials: Credentials,
    client: ClientInfo,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    is_pro: bool,
}

struct Entry {
    key: String,
    label: String,
    username: Option<String>,
    account_id: String,
    plugin: Arc<DeepAIAcc>,
    report: check::Report,
    blocked: HashSet<String>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    sessions: HashMap<String, String>,
    login: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
}

pub struct DeepAIAccounts {
    file: Arc<AccountsFile<SavedAccount>>,
    lanes: Arc<Lanes>,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    checking: tokio::sync::Mutex<()>,
}

impl DeepAIAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(dir.join("deepai-accounts.json"), None);
        let lanes = Arc::new(Lanes::new(Arc::new(lane_store::FileLaneStore::new(
            dir.join("deepai-lane.json"),
        ))));
        let entries = file
            .list()
            .into_iter()
            .filter_map(|saved| entry(&file, lanes.clone(), saved.key, saved.credentials).ok())
            .collect();
        Arc::new(Self {
            file,
            lanes,
            net,
            events,
            state: Mutex::new(State {
                entries,
                ..Default::default()
            }),
            checking: tokio::sync::Mutex::new(()),
        })
    }

    pub fn list(&self) -> Vec<Account> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|entry| Account {
                email: entry.label.clone(),
                id: Some(entry.key.clone()),
                max_credits: entry.report.max_credits,
                tier: entry.report.tier.clone(),
                status: entry.report.status.clone(),
            })
            .collect()
    }

    pub fn credits(&self) -> Option<Credits> {
        crate::quota::combine_credits(
            self.state
                .lock()
                .unwrap()
                .entries
                .iter()
                .filter_map(|entry| entry.report.credits.as_ref()),
        )
    }

    pub fn provider(&self) -> Option<Provider> {
        let state = self.state.lock().unwrap();
        let mut models: Vec<Model> = vec![];
        for entry in &state.entries {
            if matches!(entry.report.status, AccountStatus::Error { .. }) {
                continue;
            }
            for model in &entry.report.models {
                if !models.iter().any(|known| known.id == model.id) {
                    models.push(model.clone());
                }
            }
        }
        models.sort_by(crate::models::strongest_first);
        (!models.is_empty()).then(|| Provider {
            id: "deepai-account".into(),
            name: "DeepAI Account".into(),
            models,
            custom: false,
        })
    }

    pub async fn login(self: &Arc<Self>) -> Result<String, String> {
        let generation = {
            let mut state = self.state.lock().unwrap();
            cancel_login(&mut state);
            state.generation
        };
        let callback = crate::callback::Callback::bind(&[0], "/auth-callback")
            .await
            .map_err(|_| "Couldn't open the local callback for the DeepAI sign-in")?;
        let login =
            oauth::start_login(callback.port).map_err(|_| "Couldn't start signing in to DeepAI")?;
        let url = login.url().to_owned();
        let token = reqwest::Url::parse(&url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(name, _)| name == "state")
                    .map(|(_, value)| value.into_owned())
            })
            .filter(|value| !value.is_empty())
            .ok_or("Couldn't start signing in to DeepAI")?;
        let this = self.clone();
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err("DeepAI sign-in cancelled".into());
        }
        state.login = Some(tokio::spawn(async move {
            let code = match callback.code(token.clone()).await {
                Ok(code) => code,
                Err(_) => {
                    this.login_failed(
                        generation,
                        "Couldn't finish signing in to DeepAI. Sign in again.",
                    );
                    return;
                }
            };
            let account =
                match oauth::finish_login_with_profile(&*this.net, login, &token, &code).await {
                    Ok(account) => account,
                    Err(error) => {
                        this.login_failed(generation, &error.to_string());
                        return;
                    }
                };
            match this.accept_login(generation, account) {
                Ok(()) => this.check(false).await,
                Err(message) => this.login_failed(generation, &message),
            }
        }));
        Ok(url)
    }

    fn accept_login(&self, generation: u64, account: oauth::LoginAccount) -> Result<(), String> {
        account.credentials.validate()?;
        let account_id = account.credentials.account_id();
        let username = profile_name(account.username.as_deref());
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return Err("DeepAI sign-in cancelled".into());
            }
            let existing = state
                .entries
                .iter()
                .position(|entry| entry.account_id == account_id)
                .or_else(|| {
                    let username = username?;
                    state.entries.iter().position(|entry| {
                        entry
                            .username
                            .as_deref()
                            .is_some_and(|saved| saved.eq_ignore_ascii_case(username))
                    })
                });
            if let Some(index) = existing {
                let entry = &mut state.entries[index];
                self.file
                    .try_update(&entry.key, |saved| {
                        if let Some(username) = username {
                            saved.credentials.username = Some(username.to_owned());
                        }
                        saved.credentials.is_pro = account.is_pro;
                        saved.email =
                            Some(label(saved.credentials.username.as_deref(), &saved.key));
                    })
                    .map_err(|_| "Couldn't save the DeepAI account")?;
                let saved = self
                    .file
                    .credentials(&entry.key)
                    .ok_or("The DeepAI account was removed")?;
                entry.label = label(saved.username.as_deref(), &entry.key);
                entry.username = profile_name(saved.username.as_deref()).map(str::to_owned);
                entry.report.tier = Some(tier(saved.is_pro).into());
            } else {
                let saved = SavedAccount {
                    credentials: account.credentials,
                    client: ClientInfo::default(),
                    username: username.map(str::to_owned),
                    is_pro: account.is_pro,
                };
                let key = self
                    .file
                    .try_add(saved.clone())
                    .map_err(|_| "Couldn't save the DeepAI account")?;
                state
                    .entries
                    .push(entry(&self.file, self.lanes.clone(), key, saved)?);
            }
        }
        self.changed(true);
        Ok(())
    }

    fn login_failed(&self, generation: u64, message: &str) {
        if self.state.lock().unwrap().generation == generation {
            let _ = self.events.send(Event::DeepAiLoginFailed {
                message: message.into(),
            });
        }
    }

    pub fn remove(&self, label: Option<&str>, id: Option<&str>) -> Result<(), String> {
        {
            let mut state = self.state.lock().unwrap();
            cancel_login(&mut state);
            let keys: Vec<_> = match (id, label) {
                (None, None) => state
                    .entries
                    .iter()
                    .map(|entry| entry.key.clone())
                    .collect(),
                (id, label) => state
                    .entries
                    .iter()
                    .find(|entry| {
                        id.map_or_else(|| label == Some(entry.label.as_str()), |id| entry.key == id)
                    })
                    .map(|entry| entry.key.clone())
                    .into_iter()
                    .collect(),
            };
            if !keys.is_empty() {
                self.file
                    .try_remove_many(&keys)
                    .map_err(|_| "Couldn't remove the DeepAI account")?;
                state.entries.retain(|entry| !keys.contains(&entry.key));
                state.sessions.retain(|_, account| !keys.contains(account));
            }
        }
        self.changed(true);
        Ok(())
    }

    pub async fn check_all(&self) {
        self.check(true).await;
    }

    pub async fn check_limited(&self) {
        self.check(false).await;
    }

    async fn check(&self, reset_blocks: bool) {
        let _checking = self.checking.lock().await;
        let entries = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .map(|entry| {
                    entry.report.status = AccountStatus::Checking;
                    (entry.key.clone(), entry.plugin.clone())
                })
                .collect::<Vec<_>>()
        };
        if entries.is_empty() {
            self.changed(true);
            return;
        }
        self.changed(false);
        join_all(entries.into_iter().map(|(key, plugin)| async move {
            let report = plugin
                .snapshot(&*self.net)
                .await
                .map_err(check::error)
                .and_then(check::report);
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key) {
                    checked(entry, report, reset_blocks);
                }
            }
            self.changed(false);
        }))
        .await;
        self.changed(true);
    }

    pub async fn refresh_credits(&self) {
        let _checking = self.checking.lock().await;
        let entries = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.plugin.clone()))
            .collect::<Vec<_>>();
        join_all(entries.into_iter().map(|(key, plugin)| async move {
            let allowance = plugin.credits(&*self.net).await;
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key) {
                match allowance {
                    Ok(allowance) => match check::refresh(&mut entry.report, &allowance) {
                        Ok(()) => mark_blocked(entry),
                        Err(message) => entry.report.status = AccountStatus::Error { message },
                    },
                    Err(error) => {
                        entry.report.status = AccountStatus::Error {
                            message: check::error(error),
                        }
                    }
                }
            }
        }))
        .await;
        self.changed(true);
    }

    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        let mut state = self.state.lock().unwrap();
        let usable = |entry: &&Entry| {
            entry.report.enabled
                && matches!(
                    entry.report.status,
                    AccountStatus::Work | AccountStatus::Checking
                )
                && !entry.blocked.contains(&model.model_id)
                && entry
                    .report
                    .models
                    .iter()
                    .any(|known| known.id == model.model_id)
                && (entry.report.free.contains(&model.model_id) || entry.report.paid)
        };
        let previous = state.sessions.get(session).and_then(|key| {
            state
                .entries
                .iter()
                .filter(usable)
                .find(|entry| &entry.key == key)
        });
        let entry = previous
            .or_else(|| {
                state
                    .entries
                    .iter()
                    .filter(usable)
                    .max_by(|a, b| remaining(a).total_cmp(&remaining(b)))
            })
            .ok_or("No DeepAI account has access to this model and credits left")?;
        let known = entry
            .report
            .models
            .iter()
            .find(|known| known.id == model.model_id)
            .unwrap();
        let effort = model.variant.as_deref().or_else(|| {
            request
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort.as_deref())
        });
        if let Some(effort) = effort {
            let ui_effort = if effort == "none" { "off" } else { effort };
            if !known.efforts.iter().any(|known| known == ui_effort) {
                return Err("The chosen thinking level isn't available for this model".into());
            }
            let effort = if ui_effort == "off" {
                "none"
            } else {
                ui_effort
            }
            .to_owned();
            let reasoning = request.reasoning.get_or_insert_with(ReasoningCfg::default);
            reasoning.effort = Some(effort);
            reasoning.include = Some(true);
        }
        if request.max_tokens == Some(0) {
            return Err("The output token limit must be positive".into());
        }
        if known.output > 0 {
            request.max_tokens = Some(request.max_tokens.unwrap_or(known.output).min(known.output));
        }
        let endpoint = match entry.report.apis.get(&model.model_id) {
            Some(ModelApi::Responses) => DeepAIAcc::responses_endpoint(),
            Some(ModelApi::Chat) => DeepAIAcc::endpoint(),
            None => return Err("DeepAI didn't return a native API for the model".into()),
        };
        let endpoint = session_ids(session, &entry.key, &model.model_id)?
            .attach(endpoint.with_plugin(entry.plugin.clone()))?;
        let key = entry.key.clone();
        let route = Route {
            endpoint,
            options: GenerateOptions {
                encode: DeepAIAcc::encode_options(),
                ..Default::default()
            },
            account: Some(key.clone()),
        };
        state.sessions.insert(session.into(), key);
        Ok(route)
    }

    pub fn limited(&self, key: &str, model: &str, error: &ProviderError) -> bool {
        if !error
            .status
            .is_some_and(|status| (400..500).contains(&status))
            || !matches!(
                (error.kind, error.code.as_deref()),
                (
                    ErrorKind::RateLimited,
                    Some("rate_limit_error" | "rate_limit_exceeded" | "too_many_requests")
                ) | (
                    ErrorKind::Billing,
                    Some(
                        "billing_error"
                            | "theos_insufficient"
                            | "theos_exhausted"
                            | "network_bound"
                    )
                )
            )
        {
            return false;
        }
        {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.iter_mut().find(|entry| entry.key == key) else {
                return false;
            };
            if !entry.report.models.iter().any(|known| known.id == model) {
                return false;
            }
            entry.blocked.insert(model.into());
            mark_blocked(entry);
            state.sessions.retain(|_, account| account != key);
        }
        self.changed(true);
        true
    }

    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::DeepAiAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::DeepAiCredits {
            credits: self.credits(),
        });
        if providers {
            let _ = self.events.send(Event::ProvidersChanged);
        }
    }
}

impl CredentialStore for AccountStore<SavedAccount> {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async {
            self.file
                .credentials(&self.key)
                .map(|saved| saved.credentials)
                .ok_or_else(|| "The DeepAI account was removed".into())
        })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            credentials.validate()?;
            let saved = self
                .file
                .credentials(&self.key)
                .ok_or("The DeepAI account was removed")?;
            if credentials.account_id() != saved.credentials.account_id()
                || credentials.machine_uuid != saved.credentials.machine_uuid
                || credentials.instance != saved.credentials.instance
            {
                return Err("The DeepAI account id changed".into());
            }
            self.file.try_update(&self.key, |saved| {
                saved.credentials.credentials = credentials.clone();
                saved.credentials.client.version = ClientInfo::default().version;
            })
        })
    }
}

fn entry(
    file: &Arc<AccountsFile<SavedAccount>>,
    lanes: Arc<Lanes>,
    key: String,
    saved: SavedAccount,
) -> Result<Entry, String> {
    saved.credentials.validate()?;
    let account_id = saved.credentials.account_id();
    let store = Arc::new(AccountStore {
        file: file.clone(),
        key: key.clone(),
    });
    let mut client = saved.client;
    client.version = ClientInfo::default().version;
    let plugin = Arc::new(
        DeepAIAcc::new(store, client)
            .map_err(|_| "Couldn't connect the DeepAI account")?
            .with_lanes(lanes),
    );
    Ok(Entry {
        label: label(saved.username.as_deref(), &key),
        username: profile_name(saved.username.as_deref()).map(str::to_owned),
        key,
        account_id,
        plugin,
        report: check::Report {
            tier: Some(tier(saved.is_pro).into()),
            ..Default::default()
        },
        blocked: HashSet::new(),
    })
}

fn label(username: Option<&str>, key: &str) -> String {
    profile_name(username)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("DeepAI · {key}"))
}

fn profile_name(username: Option<&str>) -> Option<&str> {
    username
        .filter(|name| {
            !name.trim().is_empty()
                && name.chars().count() <= 128
                && !name.chars().any(char::is_control)
        })
        .map(str::trim)
}

fn tier(is_pro: bool) -> &'static str {
    if is_pro { "Pro" } else { "Free" }
}

fn remaining(entry: &Entry) -> f64 {
    entry
        .report
        .credits
        .as_ref()
        .map(|credits| credits.remaining)
        .unwrap_or(0.0)
}

fn checked(entry: &mut Entry, report: Result<check::Report, String>, reset_blocks: bool) {
    match report {
        Ok(mut report) => {
            report.tier = entry.report.tier.clone();
            entry.report = report;
            if reset_blocks {
                entry.blocked.clear();
            }
            mark_blocked(entry);
        }
        Err(message) => entry.report.status = AccountStatus::Error { message },
    }
}

fn mark_blocked(entry: &mut Entry) {
    if entry.report.enabled
        && !entry.report.models.is_empty()
        && entry.report.models.iter().all(|model| {
            entry.blocked.contains(&model.id)
                || (!entry.report.paid && !entry.report.free.contains(&model.id))
        })
    {
        entry.report.status = AccountStatus::Limit {
            message: "The DeepAI limit for the available models is used up".into(),
        };
    }
}

fn cancel_login(state: &mut State) {
    state.generation += 1;
    if let Some(task) = state.login.take() {
        task.abort();
    }
}

fn session_ids(session: &str, account: &str, model: &str) -> Result<Session, String> {
    let mut hash = Sha256::new();
    for part in [session, account, model] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let id = format!("nabu-{digest}");
    Session::from_ids(format!("{id}-affinity"), format!("{id}-cache"))
}

#[cfg(test)]
mod tests;
