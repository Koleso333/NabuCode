use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use nabu_protocol::{Account, AccountStatus, Event, Model, ModelRef, Provider, Quota};
use ncore::GenerateOptions;
use ncore::rt::{BoxFuture, boxed};
use ncore_grokacc::{Billing, CredentialStore, Credentials, FreeUsage, GrokAcc, oauth};
use ncore_tokio::HyperTransport;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::accounts_file::{AccountStore, AccountsFile};
use crate::callback::Callback;
use crate::models::Route;

const ID: &str = "grok-account";
const NAME: &str = "Grok Account";
const FREE_LIMIT_MESSAGE: &str = "Grok's free usage limit was reached";

#[derive(Clone, Serialize, Deserialize)]
struct SavedAccount {
    credentials: Credentials,
    #[serde(default)]
    usage: FreeUsage,
}

struct Entry {
    key: String,
    user_id: String,
    label: String,
    plugin: Arc<GrokAcc>,
    tier: Option<String>,
    billing: Option<Billing>,
    models: Vec<Model>,
    status: AccountStatus,
    checking: bool,
}

impl Entry {
    fn status(&self) -> AccountStatus {
        if self.checking {
            AccountStatus::Checking
        } else if self.status == AccountStatus::Work && self.plugin.free_usage_reported_exhausted()
        {
            AccountStatus::Limit {
                message: FREE_LIMIT_MESSAGE.into(),
            }
        } else {
            self.status.clone()
        }
    }

    fn supports(&self, model: &ModelRef) -> bool {
        self.status == AccountStatus::Work
            && self.models.iter().any(|known| {
                known.id == model.model_id
                    && model
                        .variant
                        .as_ref()
                        .is_none_or(|effort| known.efforts.contains(effort))
            })
    }

    fn quota(&self) -> Option<Quota> {
        if self
            .tier
            .as_deref()
            .is_some_and(|tier| tier.eq_ignore_ascii_case("free"))
        {
            return Some(Quota {
                family: "Grok Free · 24h".into(),
                remaining: self.plugin.free_quota().remaining_percent / 100.0,
                reset: None,
            });
        }
        let billing = self.billing.as_ref()?;
        Some(Quota {
            family: "Grok".into(),
            remaining: 1.0 - billing.used_percent()? / 100.0,
            reset: billing
                .resets_at()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|time| time.with_timezone(&chrono::Utc).to_rfc3339()),
        })
    }
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    login: Option<tokio::task::JoinHandle<()>>,
}

pub struct GrokAccounts {
    file: Arc<AccountsFile<SavedAccount>>,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    checking: tokio::sync::Mutex<()>,
}

impl GrokAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(dir.join("grok-accounts.json"), None);
        let entries = file
            .list()
            .into_iter()
            .map(|saved| entry(&file, saved.key, saved.email, saved.credentials))
            .collect();
        Arc::new(Self {
            file,
            net,
            events,
            state: Mutex::new(State {
                entries,
                ..Default::default()
            }),
            checking: Default::default(),
        })
    }

    pub fn list(&self) -> Vec<Account> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|entry| Account {
                id: None,
                max_credits: None,
                email: entry.label.clone(),
                tier: entry.tier.clone(),
                status: entry.status(),
            })
            .collect()
    }

    pub fn quota(&self) -> Vec<Quota> {
        let quotas: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter_map(Entry::quota)
            .collect();
        crate::quota::combine(quotas.iter())
    }

    pub fn provider(&self) -> Option<Provider> {
        let state = self.state.lock().unwrap();
        let mut models: Vec<Model> = Vec::new();
        for model in state
            .entries
            .iter()
            .filter(|entry| !matches!(entry.status, AccountStatus::Error { .. }))
            .flat_map(|entry| &entry.models)
        {
            if let Some(known) = models.iter_mut().find(|known| known.id == model.id) {
                for effort in &model.efforts {
                    if !known.efforts.contains(effort) {
                        known.efforts.push(effort.clone());
                    }
                }
            } else {
                models.push(model.clone());
            }
        }
        (!models.is_empty()).then(|| Provider {
            id: ID.into(),
            name: NAME.into(),
            models,
            custom: false,
        })
    }

    pub async fn login(self: &Arc<Self>) -> Result<String, String> {
        let callback = Callback::bind(&[0], "/callback")
            .await
            .map_err(|error| error.to_string())?;
        let start = oauth::start_login(callback.port).map_err(|error| error.to_string())?;
        let url = start.url.clone();
        let state = url
            .split('&')
            .find_map(|pair| pair.strip_prefix("state="))
            .ok_or("The Grok sign-in link has no state")?
            .to_owned();
        let this = self.clone();
        let mut accounts = self.state.lock().unwrap();
        if let Some(previous) = accounts.login.take() {
            previous.abort();
        }
        accounts.login = Some(tokio::spawn(async move {
            let result = async {
                let code = callback.code(state.clone()).await?;
                oauth::finish_login(&*this.net, &Default::default(), start, &code, &state)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            match result {
                Ok((credentials, profile)) => {
                    this.add(credentials, profile.email);
                    let _check = tokio::spawn(async move { this.check_all().await });
                }
                Err(message) => {
                    let _ = this.events.send(Event::GrokLoginFailed { message });
                }
            }
        }));
        Ok(url)
    }

    fn add(&self, credentials: Credentials, email: Option<String>) {
        let mut state = self.state.lock().unwrap();
        let previous = state
            .entries
            .iter()
            .position(|entry| entry.user_id == credentials.user_id);
        let usage = previous
            .map(|index| state.entries[index].plugin.free_usage())
            .unwrap_or_default();
        let account = SavedAccount { credentials, usage };
        let key = self.file.add(account.clone());
        let label = email.unwrap_or_else(|| format!("Grok · {key}"));
        self.file.set_email(&key, &label);
        let new_entry = entry(&self.file, key, Some(label), account);
        if let Some(index) = previous {
            self.file.remove(&state.entries[index].key);
            state.entries[index] = new_entry;
        } else {
            state.entries.push(new_entry);
        }
    }

    pub fn remove(&self, email: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        if let Some(login) = state.login.take() {
            login.abort();
        }
        state.entries.retain(|entry| {
            if email.is_some_and(|email| email != entry.label) {
                return true;
            }
            self.file.remove(&entry.key);
            false
        });
        drop(state);
        self.changed(true);
    }

    pub async fn check_all(&self) {
        let _checking = self.checking.lock().await;
        let plugins = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter_mut()
            .map(|entry| {
                entry.checking = true;
                (entry.key.clone(), entry.plugin.clone())
            })
            .collect::<Vec<_>>();
        if plugins.is_empty() {
            return;
        }
        self.changed(false);
        join_all(plugins.into_iter().map(|(key, plugin)| async move {
            let (profile, models, billing) = tokio::join!(
                plugin.profile(&*self.net),
                plugin.models(&*self.net),
                plugin.billing(&*self.net)
            );
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.key == key && Arc::ptr_eq(&entry.plugin, &plugin))
            else {
                return;
            };
            entry.checking = false;
            self.file
                .update(&key, |saved| saved.credentials.usage = plugin.free_usage());
            if let Ok(profile) = profile {
                entry.tier = profile.subscription_tier.or(entry.tier.take());
            }
            if let Ok(billing) = billing {
                entry.billing = Some(billing);
            }
            match models {
                Ok(models) => {
                    entry.models = models
                        .into_iter()
                        .filter(|model| model.context_window > 0)
                        .map(model_row)
                        .collect();
                    entry.status = if entry.models.is_empty() {
                        AccountStatus::Error {
                            message: "Grok did not return any available models".into(),
                        }
                    } else {
                        AccountStatus::Work
                    };
                }
                Err(error) => {
                    entry.status = AccountStatus::Error {
                        message: error.to_string(),
                    }
                }
            }
        }))
        .await;
        self.changed(true);
    }

    pub async fn refresh_quota(&self) {
        let _checking = self.checking.lock().await;
        let plugins = self.plugins();
        join_all(plugins.into_iter().map(|(key, plugin)| async move {
            let billing = plugin.billing(&*self.net).await;
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state
                .entries
                .iter_mut()
                .find(|entry| entry.key == key && Arc::ptr_eq(&entry.plugin, &plugin))
            {
                self.file
                    .update(&key, |saved| saved.credentials.usage = plugin.free_usage());
                if let Ok(billing) = billing {
                    entry.billing = Some(billing);
                }
            }
        }))
        .await;
        self.changed(false);
    }

    pub fn route(&self, session: &str, model: &ModelRef) -> Result<Route, String> {
        let state = self.state.lock().unwrap();
        let mut eligible = state
            .entries
            .iter()
            .filter(|entry| entry.supports(model))
            .peekable();
        if eligible.peek().is_none() {
            return Err("No working Grok account offers this model and reasoning level".into());
        }
        let entry = eligible
            .find(|entry| !entry.plugin.free_usage_reported_exhausted())
            .ok_or(FREE_LIMIT_MESSAGE)?;
        let key = entry.key.clone();
        let endpoint = GrokAcc::endpoint()
            .with_signature_namespace(format!("grokacc:{key}"))
            .with_plugin(entry.plugin.clone());
        let options = GenerateOptions {
            headers: vec![GrokAcc::cache_key(format!("{key}:{session}"))],
            ..Default::default()
        };
        Ok(Route {
            endpoint,
            options,
            account: Some(key),
        })
    }

    pub fn limited(&self, key: &str) -> bool {
        {
            let state = self.state.lock().unwrap();
            let Some(entry) = state.entries.iter().find(|entry| entry.key == key) else {
                return false;
            };
            if !entry.plugin.free_usage_reported_exhausted() {
                return false;
            }
            let usage = entry.plugin.free_usage();
            self.file
                .update(key, |saved| saved.credentials.usage = usage);
        }
        self.changed(false);
        true
    }

    fn plugins(&self) -> Vec<(String, Arc<GrokAcc>)> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.plugin.clone()))
            .collect()
    }

    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::GrokAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::GrokQuota {
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
                .map(|account| account.credentials)
                .ok_or_else(|| "Grok account was removed".into())
        })
    }

    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            self.file.update(&self.key, |saved| {
                saved.credentials.credentials = credentials.clone()
            });
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
        label: label.unwrap_or_else(|| format!("Grok · {key}")),
        key,
        user_id: saved.credentials.user_id,
        plugin: Arc::new(GrokAcc::with_state(store, Default::default(), saved.usage)),
        tier: None,
        billing: None,
        models: Vec::new(),
        status: AccountStatus::Unchecked,
        checking: false,
    }
}

fn model_row(model: ncore_grokacc::Model) -> Model {
    Model {
        id: model.id,
        name: model.name,
        family: "grok".into(),
        efforts: model
            .reasoning_efforts
            .into_iter()
            .map(|effort| effort.value)
            .collect(),
        context: model.context_window,
        output: 0,
        images: false,
        fast: false,
        older: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quota_failover_is_shared_by_all_chats() {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let dir = std::env::temp_dir().join(format!("nabu-grok-{:x}", u64::from_ne_bytes(random)));
        let accounts = GrokAccounts::load(
            &dir,
            Arc::new(HyperTransport::new()),
            broadcast::channel(8).0,
        );
        for user_id in ["primary", "backup"] {
            accounts.add(
                Credentials {
                    access_token: "test-token".into(),
                    refresh_token: None,
                    expires_at: u64::MAX,
                    user_id: user_id.into(),
                },
                Some(user_id.into()),
            );
        }
        let model = ModelRef {
            provider_id: ID.into(),
            model_id: "grok-test".into(),
            variant: Some("high".into()),
            fast: false,
        };
        let (primary, backup) = {
            let mut state = accounts.state.lock().unwrap();
            for entry in &mut state.entries {
                entry.models = vec![Model {
                    id: model.model_id.clone(),
                    name: "Grok Test".into(),
                    family: "grok".into(),
                    efforts: vec!["high".into()],
                    context: 1024,
                    output: 0,
                    images: false,
                    fast: false,
                    older: false,
                }];
                entry.status = AccountStatus::Work;
            }
            (state.entries[0].key.clone(), state.entries[1].key.clone())
        };
        assert_eq!(
            accounts
                .route("existing", &model)
                .unwrap()
                .account
                .as_deref(),
            Some(primary.as_str())
        );
        assert!(!accounts.limited(&primary));

        let exhausted = serde_json::from_value(json!({
            "entries": [],
            "reported": { "at": chrono::Utc::now().timestamp_millis(), "used": 100, "limit": 100, "local": 0 },
        }))
        .unwrap();
        {
            let mut state = accounts.state.lock().unwrap();
            let store = Arc::new(AccountStore {
                file: accounts.file.clone(),
                key: primary.clone(),
            });
            state.entries[0].plugin =
                Arc::new(GrokAcc::with_state(store, Default::default(), exhausted));
        }
        assert!(accounts.limited(&primary));
        assert!(matches!(
            accounts.list()[0].status,
            AccountStatus::Limit { .. }
        ));
        for chat in ["existing", "new"] {
            assert_eq!(
                accounts.route(chat, &model).unwrap().account.as_deref(),
                Some(backup.as_str())
            );
        }
        accounts.remove(Some("backup"));
        assert_eq!(
            accounts.route("existing", &model).err().as_deref(),
            Some(FREE_LIMIT_MESSAGE)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
