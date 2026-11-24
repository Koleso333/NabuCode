mod check;
mod models;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use nabu_protocol::{Account as AccountRow, AccountStatus, Event, Model, Provider, Quota};
use ncore::Endpoint;
use ncore_antigravity::{Account, Antigravity, oauth};
use ncore_tokio::{HyperTransport, TokioClock};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::callback::Callback;

// Ports allowed as redirect targets for the Antigravity OAuth client.
const PORTS: [u16; 6] = [51321, 51322, 51323, 51324, 51325, 51326];

struct Entry {
    account: Account,
    tier: Option<String>,
    status: AccountStatus,
    models: Vec<Model>,
    quota: Vec<Quota>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    login: Option<JoinHandle<()>>,
    /// Plugin over the working accounts, kept while they stay the same so tokens and cooldowns survive.
    plugin: Option<(Vec<String>, Arc<Antigravity>)>,
}

/// Google accounts serving Claude and Gemini through Antigravity.
pub struct Accounts {
    path: PathBuf,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
}

impl Accounts {
    /// Reads `path` in the ReProxy/NaProxy format `{"accounts": [...]}`.
    pub fn load(
        path: PathBuf,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|raw| Account::parse_file(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|account| Entry {
                account,
                tier: None,
                status: AccountStatus::Unchecked,
                models: Vec::new(),
                quota: Vec::new(),
            })
            .collect();
        let state = Mutex::new(State {
            entries,
            ..State::default()
        });
        Arc::new(Self {
            path,
            net,
            events,
            state,
        })
    }

    pub fn list(&self) -> Vec<AccountRow> {
        let state = self.state.lock().unwrap();
        state
            .entries
            .iter()
            .map(|e| AccountRow {
                email: e.account.email.clone(),
                id: None,
                max_credits: None,
                tier: e.tier.clone(),
                status: e.status.clone(),
            })
            .collect()
    }

    /// The provider, present once at least one account works; models are the union over working accounts.
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
        models.sort_by(crate::models::strongest_first);
        Some(Provider {
            id: "antigravity".into(),
            name: "Antigravity".into(),
            models,
            custom: false,
        })
    }

    /// Endpoint that spreads requests over the working accounts; `None` when none works.
    pub fn endpoint(&self) -> Option<Endpoint> {
        let mut state = self.state.lock().unwrap();
        let working: Vec<&Entry> = state
            .entries
            .iter()
            .filter(|e| e.status == AccountStatus::Work)
            .collect();
        if working.is_empty() {
            return None;
        }
        let emails: Vec<String> = working.iter().map(|e| e.account.email.clone()).collect();
        let plugin = match &state.plugin {
            Some((cached, plugin)) if *cached == emails => plugin.clone(),
            _ => {
                let plugin = Arc::new(Antigravity::new(
                    working.iter().map(|e| e.account.clone()).collect(),
                ));
                state.plugin = Some((emails, plugin.clone()));
                plugin
            }
        };
        Some(Antigravity::endpoint().with_plugin(plugin))
    }

    /// Quota over every account that reported one.
    /// Empty until some working account reports a quota.
    pub fn quota(&self) -> Vec<Quota> {
        let state = self.state.lock().unwrap();
        if !state
            .entries
            .iter()
            .any(|e| e.status == AccountStatus::Work && !e.quota.is_empty())
        {
            return Vec::new();
        }
        crate::quota::combine(state.entries.iter().flat_map(|e| &e.quota))
    }

    /// Starts a browser sign-in and returns its URL; the account arrives later as an event.
    pub async fn login(self: &Arc<Self>) -> Result<String, String> {
        if let Some(previous) = self.state.lock().unwrap().login.take() {
            previous.abort();
        }
        let callback = Callback::bind(&PORTS, "/oauth-callback")
            .await
            .map_err(|e| format!("Could not open the sign-in callback: {e}"))?;
        let start = oauth::start_login(&callback.redirect);
        let url = start.url.clone();
        let this = self.clone();
        let task = tokio::spawn(async move {
            let redirect = callback.redirect.clone();
            let result = match callback.code(start.state).await {
                Ok(code) => {
                    oauth::finish_login(&*this.net, &TokioClock, &code, &start.verifier, &redirect)
                        .await
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(e),
            };
            match result {
                Ok(account) => this.add(account).await,
                Err(message) => {
                    let _ = this.events.send(Event::AntigravityLoginFailed { message });
                }
            }
        });
        self.state.lock().unwrap().login = Some(task);
        Ok(url)
    }

    async fn add(self: &Arc<Self>, account: Account) {
        let email = account.email.clone();
        {
            let mut state = self.state.lock().unwrap();
            state.entries.retain(|e| e.account.email != email);
            state.entries.push(Entry {
                account,
                tier: None,
                status: AccountStatus::Unchecked,
                models: Vec::new(),
                quota: Vec::new(),
            });
        }
        self.save();
        self.check(&[email], true).await;
    }

    pub fn remove(&self, email: &str) {
        self.state
            .lock()
            .unwrap()
            .entries
            .retain(|e| e.account.email != email);
        self.save();
        self.changed(true);
    }

    pub fn remove_all(&self) {
        self.state.lock().unwrap().entries.clear();
        self.save();
        self.changed(true);
    }

    /// Re-checks every account; `probe` also sends one real request through each.
    pub async fn check_all(self: &Arc<Self>, probe: bool) {
        let emails: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.account.email.clone())
            .collect();
        self.check(&emails, probe).await;
    }

    /// Reads only the quota of every account; an account whose read fails keeps its last one.
    pub async fn refresh_quota(&self) {
        let accounts: Vec<Account> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.account.clone())
            .collect();
        let quotas = join_all(
            accounts
                .iter()
                .map(|account| check::quota(&self.net, account)),
        )
        .await;
        {
            let mut state = self.state.lock().unwrap();
            for (account, quota) in accounts.iter().zip(quotas) {
                if let Some(entry) = state
                    .entries
                    .iter_mut()
                    .find(|e| e.account.email == account.email)
                    && let Ok(quota) = quota
                {
                    entry.quota = quota;
                }
            }
        }
        self.changed(false);
    }

    async fn check(self: &Arc<Self>, emails: &[String], probe: bool) {
        let accounts: Vec<(Account, AccountStatus)> = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .filter(|e| emails.contains(&e.account.email))
                .map(|e| {
                    (
                        e.account.clone(),
                        std::mem::replace(&mut e.status, AccountStatus::Checking),
                    )
                })
                .collect()
        };
        self.changed(false);

        join_all(accounts.into_iter().map(|(account, before)| async move {
            let report = check::check(&self.net, &account, probe).await;
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state
                    .entries
                    .iter_mut()
                    .find(|e| e.account.email == account.email)
                {
                    entry.tier = report.tier;
                    entry.models = report.models;
                    entry.quota = report.quota;
                    // Without a probe a healthy account keeps the status its last probe gave it.
                    entry.status = match report.status {
                        AccountStatus::Unchecked => before,
                        status => status,
                    };
                }
            }
            self.changed(false);
        }))
        .await;
        self.changed(true);
    }

    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::AntigravityAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::AntigravityQuota {
            quota: self.quota(),
        });
        if providers {
            let _ = self.events.send(Event::ProvidersChanged);
        }
    }

    fn save(&self) {
        let accounts: Vec<Account> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.account.clone())
            .collect();
        let body =
            serde_json::to_string_pretty(&json!({ "accounts": accounts })).expect("serializable");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, body)?;
            std::fs::rename(tmp, &self.path)
        };
        if let Err(e) = write() {
            eprintln!("[antigravity] could not save accounts: {e}");
        }
    }
}
