mod check;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use nabu_protocol::{Account, AccountStatus, Event, Model, ModelRef, Provider, Quota};
use ncore::GenerateOptions;
use ncore::core::ir::{self, ReasoningCfg};
use ncore::rt::{BoxFuture, boxed};
use ncore_tokio::HyperTransport;
use ncore_zcodeacc::{ClientInfo, CredentialStore, Credentials, ZCodeAcc, oauth};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::accounts_file::{AccountStore, AccountsFile};
use crate::models::Route;

#[derive(Clone, Serialize, Deserialize)]
struct SavedAccount {
    credentials: Credentials,
    client: ClientInfo,
    user_id: String,
}

struct Entry {
    key: String,
    label: String,
    user_id: String,
    plugin: Arc<ZCodeAcc>,
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

pub struct ZCodeAccounts {
    file: Arc<AccountsFile<SavedAccount>>,
    client: ClientInfo,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    checking: tokio::sync::Mutex<()>,
}

impl ZCodeAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(dir.join("zcode-accounts.json"), None);
        let client = client_info(dir);
        let entries = file
            .list()
            .into_iter()
            .map(|saved| entry(&file, saved.key, saved.email, saved.credentials))
            .collect();
        Arc::new(Self {
            file,
            client,
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
            .map(|e| Account {
                id: None,
                max_credits: None,
                email: e.label.clone(),
                tier: e.report.tier.clone(),
                status: e.report.status.clone(),
            })
            .collect()
    }

    pub fn quota(&self) -> Vec<Quota> {
        crate::quota::combine(
            self.state
                .lock()
                .unwrap()
                .entries
                .iter()
                .flat_map(|e| &e.report.quota),
        )
    }

    pub fn provider(&self) -> Option<Provider> {
        let mut models: Vec<Model> = Vec::new();
        for entry in &self.state.lock().unwrap().entries {
            if matches!(entry.report.status, AccountStatus::Error { .. }) {
                continue;
            }
            for model in &entry.report.models {
                if !models.iter().any(|m| m.id == model.id) {
                    models.push(model.clone());
                }
            }
        }
        models.sort_by_key(|m| m.id.contains("flash"));
        (!models.is_empty()).then(|| Provider {
            id: "zcode-account".into(),
            name: "ZCode Account".into(),
            models,
            custom: false,
        })
    }

    pub async fn login(
        self: &Arc<Self>,
        language: String,
        timezone: String,
    ) -> Result<String, String> {
        let generation = {
            let mut state = self.state.lock().unwrap();
            if let Some(task) = state.login.take() {
                task.abort();
            }
            state.generation += 1;
            state.generation
        };
        let mut client = self.client.clone();
        client.language = language;
        client.timezone = timezone;
        let mut login = oauth::start_login(&*self.net, &client)
            .await
            .map_err(|e| e.to_string())?;
        let url = login.url.clone();
        let this = self.clone();
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err("ZCode sign-in cancelled".into());
        }
        state.login = Some(tokio::spawn(async move {
            loop {
                match oauth::poll_login(&*this.net, &client, &mut login).await {
                    Ok(oauth::LoginPoll::Pending) => tokio::time::sleep(login.poll_interval).await,
                    Ok(oauth::LoginPoll::Ready(credentials)) => {
                        let user = login.user().cloned();
                        let user_id = user.as_ref().map(|u| u.id.clone()).unwrap_or_default();
                        let label = user.and_then(|u| u.email.or(u.name)).unwrap_or_else(|| {
                            format!(
                                "Z.ai · {}",
                                if user_id.is_empty() {
                                    "Account"
                                } else {
                                    &user_id
                                }
                            )
                        });
                        let saved = SavedAccount {
                            credentials,
                            client,
                            user_id: user_id.clone(),
                        };
                        {
                            let mut state = this.state.lock().unwrap();
                            if state.generation != generation {
                                return;
                            }
                            let previous = state
                                .entries
                                .iter()
                                .filter(|e| !user_id.is_empty() && e.user_id == user_id)
                                .map(|e| e.key.clone())
                                .collect::<Vec<_>>();
                            for key in &previous {
                                this.file.remove(key);
                            }
                            state.entries.retain(|e| !previous.contains(&e.key));
                            state.sessions.retain(|_, key| !previous.contains(key));
                            let key = this.file.add(saved.clone());
                            this.file.set_email(&key, &label);
                            state
                                .entries
                                .push(entry(&this.file, key, Some(label), saved));
                        }
                        this.check_all().await;
                        return;
                    }
                    Err(error) => {
                        if this.state.lock().unwrap().generation == generation {
                            let _ = this.events.send(Event::ZCodeLoginFailed {
                                message: error.to_string(),
                            });
                        }
                        return;
                    }
                }
            }
        }));
        Ok(url)
    }

    pub fn remove(&self, label: Option<&str>) {
        {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            if let Some(task) = state.login.take() {
                task.abort();
            }
            let keys = state
                .entries
                .iter()
                .filter(|e| label.is_none_or(|label| e.label == label))
                .map(|e| e.key.clone())
                .collect::<Vec<_>>();
            for key in &keys {
                self.file.remove(key);
            }
            state.entries.retain(|e| !keys.contains(&e.key));
            state.sessions.retain(|_, key| !keys.contains(key));
        }
        self.changed(true);
    }

    pub async fn check_all(&self) {
        self.check(false).await;
    }

    pub async fn check_limited(&self) {
        self.check(true).await;
    }

    async fn check(&self, limited_only: bool) {
        let _checking = self.checking.lock().await;
        let entries = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .filter(|e| {
                    !limited_only
                        || !e.blocked.is_empty()
                        || matches!(e.report.status, AccountStatus::Limit { .. })
                })
                .map(|e| {
                    e.report.status = AccountStatus::Checking;
                    (e.key.clone(), e.plugin.clone())
                })
                .collect::<Vec<_>>()
        };
        if entries.is_empty() {
            return;
        }
        self.changed(false);
        join_all(entries.into_iter().map(|(key, plugin)| async move {
            plugin.invalidate_usage().await;
            let result = plugin.usage(&*self.net).await;
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                    match result {
                        Ok(snapshot) => {
                            entry.report = check::report(&snapshot);
                            entry.blocked.clear();
                        }
                        Err(error) => {
                            entry.report.status = AccountStatus::Error {
                                message: error.to_string(),
                            }
                        }
                    }
                }
            }
            self.changed(false);
        }))
        .await;
        self.changed(true);
    }

    /// Reads only the balances of every account; an account whose read fails keeps its last ones.
    pub async fn refresh_quota(&self) {
        let plugins = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.plugin.clone()))
            .collect::<Vec<_>>();
        let snapshots = join_all(plugins.iter().map(|(_, plugin)| async move {
            plugin.invalidate_usage().await;
            plugin.usage(&*self.net).await
        }))
        .await;
        {
            let mut state = self.state.lock().unwrap();
            for ((key, _), snapshot) in plugins.into_iter().zip(snapshots) {
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key)
                    && let Ok(snapshot) = snapshot
                {
                    let report = check::report(&snapshot);
                    entry.report.quota = report.quota;
                    entry.report.remaining = report.remaining;
                }
            }
        }
        self.changed(false);
    }

    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        let mut state = self.state.lock().unwrap();
        let usable = |e: &&Entry| {
            matches!(e.report.status, AccountStatus::Work)
                && !e.blocked.contains(&model.model_id)
                && e.report.models.iter().any(|m| m.id == model.model_id)
                && e.report
                    .remaining
                    .get(&model.model_id)
                    .is_none_or(|n| *n > 0.0)
        };
        let previous = state
            .sessions
            .get(session)
            .and_then(|key| state.entries.iter().filter(usable).find(|e| &e.key == key));
        let entry = previous
            .or_else(|| {
                state.entries.iter().filter(usable).max_by(|a, b| {
                    a.report
                        .remaining
                        .get(&model.model_id)
                        .unwrap_or(&1.0)
                        .total_cmp(b.report.remaining.get(&model.model_id).unwrap_or(&1.0))
                })
            })
            .ok_or("No ZCode account has quota left for this model")?;
        let known = entry
            .report
            .models
            .iter()
            .find(|m| m.id == model.model_id)
            .unwrap();
        if let Some(effort) = &model.variant {
            if !known.efforts.contains(effort) {
                return Err("The chosen thinking level isn't available for this model".into());
            }
            request.reasoning = Some(ReasoningCfg {
                effort: Some(effort.clone()),
                include: Some(true),
                ..Default::default()
            });
        }
        request.max_tokens.get_or_insert(known.output);
        let route = Route {
            endpoint: ZCodeAcc::endpoint().with_plugin(entry.plugin.clone()),
            options: GenerateOptions::default(),
            account: Some(entry.key.clone()),
        };
        let key = entry.key.clone();
        state.sessions.insert(session.into(), key);
        Ok(route)
    }

    pub fn limited(&self, key: &str, model: &str, message: &str) -> bool {
        if message.to_ascii_lowercase().contains("concurrency") {
            return false;
        }
        {
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                entry.blocked.insert(model.into());
                if entry.report.models.iter().all(|m| {
                    entry.blocked.contains(&m.id)
                        || entry.report.remaining.get(&m.id).is_some_and(|n| *n <= 0.0)
                }) {
                    entry.report.status = AccountStatus::Limit {
                        message: message.into(),
                    };
                }
            }
            state.sessions.retain(|_, value| value != key);
        }
        self.changed(true);
        true
    }

    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::ZCodeAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::ZCodeQuota {
            quota: self.quota(),
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
                .map(|s| s.credentials)
                .ok_or_else(|| "The account was removed".into())
        })
    }
    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            let mut account = self.file.credentials(&self.key).ok_or("The account was removed")?;
            account.credentials = credentials.clone();
            self.file.replace(&self.key, &account);
            Ok(())
        })
    }
}

fn entry(
    file: &Arc<AccountsFile<SavedAccount>>,
    key: String,
    label: Option<String>,
    saved: SavedAccount,
) -> Entry {
    let store = Arc::new(AccountStore {
        file: file.clone(),
        key: key.clone(),
    });
    Entry {
        label: label.unwrap_or_else(|| format!("Z.ai · {key}")),
        key,
        user_id: saved.user_id,
        plugin: Arc::new(ZCodeAcc::new(store, saved.client)),
        blocked: HashSet::new(),
        report: check::Report {
            tier: None,
            models: vec![],
            quota: vec![],
            remaining: HashMap::new(),
            status: AccountStatus::Unchecked,
        },
    }
}

fn client_info(dir: &Path) -> ClientInfo {
    let path = dir.join("zcode-client.json");
    if let Some(client) = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
    {
        return client;
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("os random");
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let client = ClientInfo::new("3.17.2", id);
    if let Err(error) = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(path, serde_json::to_vec_pretty(&client).unwrap()))
    {
        eprintln!("[zcode] could not save client identity: {error}");
    }
    client
}
