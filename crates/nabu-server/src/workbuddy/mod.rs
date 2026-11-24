mod check;

use crate::{
    accounts_file::{AccountStore, AccountsFile},
    models::Route,
};
use futures_util::future::join_all;
use nabu_protocol::{Account, AccountStatus, Credits, Event, Model, ModelRef, Provider};
use ncore::{
    GenerateOptions,
    core::ir::{self, ReasoningCfg},
    rt::{BoxFuture, boxed},
};
use ncore_tokio::HyperTransport;
use ncore_workbuddyacc::{ClientInfo, CredentialStore, Credentials, WorkBuddyAcc, oauth};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Clone, Serialize, Deserialize)]
struct SavedAccount {
    credentials: Credentials,
    client: ClientInfo,
}
struct Entry {
    key: String,
    label: String,
    uid: String,
    plugin: Arc<WorkBuddyAcc>,
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
pub struct WorkBuddyAccounts {
    file: Arc<AccountsFile<SavedAccount>>,
    net: Arc<HyperTransport>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
    checking: tokio::sync::Mutex<()>,
}

impl WorkBuddyAccounts {
    pub fn load(
        dir: &Path,
        net: Arc<HyperTransport>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let file = AccountsFile::load(dir.join("workbuddy-accounts.json"), None);
        let entries = file
            .list()
            .into_iter()
            .filter_map(|saved| entry(&file, saved.key, saved.email, saved.credentials).ok())
            .collect();
        Arc::new(Self {
            file,
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
    pub fn credits(&self) -> Option<Credits> {
        check::combine(
            self.state
                .lock()
                .unwrap()
                .entries
                .iter()
                .filter_map(|e| e.report.credits.as_ref()),
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
                if !models.iter().any(|m| m.id == model.id) {
                    models.push(model.clone());
                }
            }
        }
        models.sort_by(crate::models::strongest_first);
        (!models.is_empty()).then(|| Provider {
            id: "workbuddy-account".into(),
            name: "WorkBuddy Account".into(),
            models,
            custom: false,
        })
    }
    pub async fn login(self: &Arc<Self>) -> Result<String, String> {
        let generation = {
            let mut state = self.state.lock().unwrap();
            if let Some(task) = state.login.take() {
                task.abort();
            }
            state.generation += 1;
            state.generation
        };
        let client = ClientInfo::new("5.9.4");
        let mut login = oauth::start_login(&*self.net, &client)
            .await
            .map_err(|e| e.to_string())?;
        let url = login.url.clone();
        let this = self.clone();
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return Err("WorkBuddy sign-in cancelled".into());
        }
        state.login = Some(tokio::spawn(async move {
            loop {
                match oauth::poll_login(&*this.net, &client, &mut login).await {
                    Ok(oauth::LoginPoll::Pending) => tokio::time::sleep(login.poll_interval).await,
                    Ok(oauth::LoginPoll::Ready(credentials)) => {
                        let uid = credentials.uid.clone();
                        let saved = SavedAccount {
                            credentials,
                            client,
                        };
                        {
                            let mut state = this.state.lock().unwrap();
                            if state.generation != generation {
                                return;
                            }
                            let previous = state
                                .entries
                                .iter()
                                .filter(|e| e.uid == uid)
                                .map(|e| e.key.clone())
                                .collect::<Vec<_>>();
                            for key in &previous {
                                this.file.remove(key);
                            }
                            state.entries.retain(|e| !previous.contains(&e.key));
                            state.sessions.retain(|_, key| !previous.contains(key));
                            let key = this.file.add(saved.clone());
                            match entry(&this.file, key, None, saved) {
                                Ok(entry) => state.entries.push(entry),
                                Err(message) => {
                                    let _ =
                                        this.events.send(Event::WorkBuddyLoginFailed { message });
                                    return;
                                }
                            }
                        }
                        this.check_all().await;
                        return;
                    }
                    Err(error) => {
                        if this.state.lock().unwrap().generation == generation {
                            let _ = this.events.send(Event::WorkBuddyLoginFailed {
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
        let _checking = self.checking.lock().await;
        let entries = {
            let mut state = self.state.lock().unwrap();
            state
                .entries
                .iter_mut()
                .map(|e| {
                    e.report.status = AccountStatus::Checking;
                    (e.key.clone(), e.plugin.clone(), e.uid.clone())
                })
                .collect::<Vec<_>>()
        };
        if entries.is_empty() {
            return;
        }
        self.changed(false);
        join_all(entries.into_iter().map(|(key, plugin, uid)| async move {
            let report = plugin
                .snapshot(&*self.net)
                .await
                .map_err(|e| e.to_string())
                .and_then(check::report);
            let profile = plugin.profile(&*self.net).await.ok();
            let label = profile
                .as_ref()
                .and_then(|p| {
                    p["email"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            p["nickname"]
                                .as_str()
                                .filter(|s| !s.trim().is_empty())
                                .map(|name| format!("{name} · {uid}"))
                        })
                })
                .unwrap_or_else(|| format!("WorkBuddy · {uid}"));
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                    if profile.is_some() {
                        entry.label = label.clone();
                        self.file.set_email(&key, &label);
                    }
                    match report {
                        Ok(report) => {
                            entry.report = report;
                            entry.blocked.clear();
                        }
                        Err(message) => entry.report.status = AccountStatus::Error { message },
                    }
                }
            }
            self.changed(false);
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
        let usable = |e: &&Entry| {
            matches!(
                e.report.status,
                AccountStatus::Work | AccountStatus::Checking
            ) && !e.blocked.contains(&model.model_id)
                && e.report.models.iter().any(|m| m.id == model.model_id)
                && (e.report.free.contains(&model.model_id)
                    || e.report.credits.as_ref().is_some_and(|c| c.remaining > 0.0))
        };
        let previous = state
            .sessions
            .get(session)
            .and_then(|key| state.entries.iter().filter(usable).find(|e| &e.key == key));
        let entry = previous
            .or_else(|| {
                state.entries.iter().filter(usable).max_by(|a, b| {
                    a.report
                        .credits
                        .as_ref()
                        .map(|c| c.remaining)
                        .unwrap_or(0.0)
                        .total_cmp(
                            &b.report
                                .credits
                                .as_ref()
                                .map(|c| c.remaining)
                                .unwrap_or(0.0),
                        )
                })
            })
            .ok_or("No WorkBuddy account has access to this model and credits left")?;
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
        if known.output > 0 {
            request.max_tokens.get_or_insert(known.output);
        }
        let key = entry.key.clone();
        let route = Route {
            endpoint: WorkBuddyAcc::endpoint().with_plugin(entry.plugin.clone()),
            options: GenerateOptions {
                encode: WorkBuddyAcc::encode_options(),
                ..Default::default()
            },
            account: Some(key.clone()),
        };
        state.sessions.insert(session.into(), key);
        Ok(route)
    }
    pub async fn refresh_credits(&self) {
        let _checking = self.checking.lock().await;
        let entries = self
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.plugin.clone()))
            .collect::<Vec<_>>();
        join_all(entries.into_iter().map(|(key, plugin)| async move {
            let result = plugin
                .credits(&*self.net)
                .await
                .map_err(|e| e.to_string())
                .and_then(|c| check::balance(&c));
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                match result {
                    Ok(credits) => {
                        if credits.remaining <= 0.0 && entry.report.free.is_empty() {
                            entry.report.status = AccountStatus::Limit {
                                message: "WorkBuddy credits are used up".into(),
                            };
                        }
                        entry.report.credits = Some(credits);
                    }
                    Err(message) => entry.report.status = AccountStatus::Error { message },
                }
            }
        }))
        .await;
        self.changed(true);
    }
    pub fn limited(&self, key: &str, model: &str, message: &str) -> bool {
        if message.to_ascii_lowercase().contains("concurrency") {
            return false;
        }
        {
            let mut state = self.state.lock().unwrap();
            if let Some(entry) = state.entries.iter_mut().find(|e| e.key == key) {
                entry.blocked.insert(model.into());
                if entry
                    .report
                    .models
                    .iter()
                    .all(|m| entry.blocked.contains(&m.id))
                {
                    entry.report.status = AccountStatus::Limit {
                        message: message.into(),
                    };
                }
            }
            state.sessions.retain(|_, k| k != key);
        }
        self.changed(true);
        true
    }
    fn changed(&self, providers: bool) {
        let _ = self.events.send(Event::WorkBuddyAccounts {
            accounts: self.list(),
        });
        let _ = self.events.send(Event::WorkBuddyCredits {
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
                .map(|s| s.credentials)
                .ok_or_else(|| "The account was removed".into())
        })
    }
    fn save<'a>(&'a self, credentials: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            let mut saved = self.file.credentials(&self.key).ok_or("The account was removed")?;
            saved.credentials = credentials.clone();
            self.file.replace(&self.key, &saved);
            Ok(())
        })
    }
}
fn entry(
    file: &Arc<AccountsFile<SavedAccount>>,
    key: String,
    label: Option<String>,
    saved: SavedAccount,
) -> Result<Entry, String> {
    let uid = saved.credentials.uid.clone();
    let store = Arc::new(AccountStore {
        file: file.clone(),
        key: key.clone(),
    });
    let plugin = Arc::new(WorkBuddyAcc::new(store, saved.client).map_err(|e| e.to_string())?);
    Ok(Entry {
        key,
        label: label.unwrap_or_else(|| format!("WorkBuddy · {uid}")),
        uid,
        plugin,
        report: check::Report::default(),
        blocked: HashSet::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accounts() -> Arc<WorkBuddyAccounts> {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("nabu-workbuddy-{:x}", u64::from_ne_bytes(random)));
        WorkBuddyAccounts::load(
            &path,
            Arc::new(HyperTransport::new()),
            broadcast::channel(32).0,
        )
    }
    fn add(accounts: &WorkBuddyAccounts, uid: &str, remaining: f64, total: f64, free: bool) {
        let saved = SavedAccount {
            credentials: Credentials {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: u64::MAX,
                refresh_expires_at: None,
                uid: uid.into(),
                domain: "www.workbuddy.ai".into(),
            },
            client: ClientInfo::new("5.9.4"),
        };
        let key = accounts.file.add(saved.clone());
        let mut entry = entry(&accounts.file, key, Some(uid.into()), saved).unwrap();
        entry.report = check::Report {
            tier: Some("Free".into()),
            models: vec![Model {
                id: "hy4free".into(),
                name: "Hy4 (Free)".into(),
                family: "hunyuan".into(),
                efforts: vec!["high".into()],
                context: 200000,
                output: 64000,
                images: true,
                fast: false,
                older: false,
            }],
            free: if free { vec!["hy4free".into()] } else { vec![] },
            credits: Some(Credits {
                remaining,
                total,
                reset: None,
            }),
            status: AccountStatus::Work,
        };
        accounts.state.lock().unwrap().entries.push(entry);
    }
    fn reference() -> ModelRef {
        ModelRef {
            provider_id: "workbuddy-account".into(),
            model_id: "hy4free".into(),
            variant: Some("high".into()),
            fast: false,
        }
    }

    #[test]
    fn combined_credits_and_routing_follow_the_accounts_that_remain_connected() {
        let accounts = accounts();
        add(&accounts, "a", 120.0, 120.0, true);
        add(&accounts, "b", 100.0, 100.0, true);
        let balance = accounts.credits().unwrap();
        assert_eq!(balance.remaining, 220.0);
        assert_eq!(balance.total, 220.0);
        assert_eq!(accounts.provider().unwrap().models.len(), 1);
        let model = reference();
        let mut request = ir::Request::default();
        let first = accounts.route("s", &model, &mut request).unwrap();
        let selected = accounts
            .state
            .lock()
            .unwrap()
            .entries
            .iter()
            .find(|e| Some(&e.key) == first.account.as_ref())
            .unwrap()
            .label
            .clone();
        assert_eq!(selected, "a");
        assert_eq!(
            first.options.encode.chat_reasoning_field.as_deref(),
            Some("reasoning")
        );
        accounts.limited(
            first.account.as_ref().unwrap(),
            &model.model_id,
            "daily allowance exhausted",
        );
        let next = accounts.route("s", &model, &mut request).unwrap();
        assert_ne!(first.account, next.account);
        accounts.remove(Some("a"));
        let balance = accounts.credits().unwrap();
        assert_eq!(balance.remaining, 100.0);
        assert_eq!(balance.total, 100.0);
        accounts.remove(None);
        assert!(accounts.credits().is_none());
        assert!(accounts.provider().is_none());
    }

    #[test]
    fn zero_credit_free_model_can_run_while_paid_models_and_unknown_efforts_cannot() {
        let accounts = accounts();
        add(&accounts, "free", 0.0, 130.0, true);
        accounts.state.lock().unwrap().entries[0].report.status = AccountStatus::Checking;
        let mut request = ir::Request::default();
        assert!(accounts.route("s", &reference(), &mut request).is_ok());
        accounts.state.lock().unwrap().entries[0]
            .report
            .free
            .clear();
        assert!(accounts.route("s", &reference(), &mut request).is_err());
        accounts.state.lock().unwrap().entries[0]
            .report
            .free
            .push("hy4free".into());
        let mut unsupported = reference();
        unsupported.variant = Some("low".into());
        assert!(accounts.route("s", &unsupported, &mut request).is_err());
        let mut unavailable = reference();
        unavailable.model_id = "gpt-pro-only".into();
        assert!(accounts.route("s", &unavailable, &mut request).is_err());
    }

    #[tokio::test]
    #[ignore = "reads explicit WorkBuddy credentials, checks the host adapter and generates a short reply"]
    async fn live_workbuddy_host_adapter() {
        use futures_util::StreamExt;
        use ncore::{
            CancelToken, Core,
            core::ir::{Accumulator, Block, Message, Role},
        };
        let path = std::env::var("NABUCODE_WORKBUDDY_TEST_HOME").expect("set a private test home");
        let credentials: Credentials = serde_json::from_slice(
            &std::fs::read(
                std::env::var("NABU_WORKBUDDY_CREDENTIALS").expect("set credentials path"),
            )
            .unwrap(),
        )
        .unwrap();
        let net = Arc::new(HyperTransport::with_proxy(crate::proxy_from_env()));
        let manager =
            WorkBuddyAccounts::load(Path::new(&path), net.clone(), broadcast::channel(32).0);
        if manager.file.list().is_empty() {
            manager.file.add(SavedAccount {
                credentials,
                client: ClientInfo::new("5.9.4"),
            });
        }
        let manager =
            WorkBuddyAccounts::load(Path::new(&path), net.clone(), broadcast::channel(32).0);
        manager.check_all().await;
        assert!(matches!(manager.list()[0].status, AccountStatus::Work));
        let provider = manager.provider().expect("no WorkBuddy provider");
        assert_eq!(provider.id, "workbuddy-account");
        assert!(
            provider
                .models
                .iter()
                .any(|m| m.id == "hy4free" && m.name == "Hy4 (Free)")
        );
        assert!(provider.models.iter().all(|m| m.id != "gpt-6-astra"));
        let credits = manager.credits().unwrap();
        assert!(credits.total >= credits.remaining);
        let mut request = ir::Request {
            model: "hy4free".into(),
            system: vec![Block::text("Reply only NABU_ACCOUNT_OK.")],
            messages: vec![Message::new(
                Role::User,
                vec![Block::text("Reply with the requested marker.")],
            )],
            max_tokens: Some(1024),
            stream: true,
            ..Default::default()
        };
        let route = manager
            .route("host-test", &reference(), &mut request)
            .unwrap();
        let core = Core::builder(net).build();
        let mut generation = core
            .generate(
                &route.endpoint,
                &request,
                &route.options,
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let mut acc = Accumulator::new();
        while let Some(event) = generation.next().await {
            acc.push(&event.unwrap());
        }
        assert!(acc.error().is_none());
        let result = acc.finish();
        let text = result
            .content
            .iter()
            .filter_map(|b| {
                if let Block::Text { text, .. } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        assert_eq!(text.trim(), "NABU_ACCOUNT_OK");
        manager.refresh_credits().await;
        println!(
            "WorkBuddy host: {} eligible models, credits {} / {}, generated NABU_ACCOUNT_OK",
            provider.models.len(),
            credits.remaining,
            credits.total
        );
    }
}
