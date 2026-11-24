//! The same requests for every built-in provider, whatever way it signs in, and the switch that turns a provider off.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use nabu_protocol::{
    Account, Credits, Done, Event, ModelKey, ProtocolError, ProviderAuth, ProviderEntry, Quota, Reply, UsageBar, UsageUnit,
};
use serde::{Deserialize, Serialize};

use crate::{Server, api_keys::ApiKeyAccounts, failed};

/// Built-in providers in the order they are offered.
const BUILTIN: [(&str, &str, ProviderAuth); 12] = [
    ("claude-account", "Claude", ProviderAuth::Account),
    ("chatgpt-account", "ChatGPT", ProviderAuth::Account),
    ("zcode-account", "ZCode Account", ProviderAuth::Account),
    ("grok-account", "Grok Account", ProviderAuth::Account),
    ("workbuddy-account", "WorkBuddy Account", ProviderAuth::Account),
    ("deepai-account", "DeepAI Account", ProviderAuth::Account),
    ("antigravity", "Antigravity", ProviderAuth::Account),
    ("agentrouter", "Agent Router", ProviderAuth::Key),
    ("coarena", "CoArena", ProviderAuth::Key),
    ("clauden", "Clauden", ProviderAuth::Key),
    ("gemini", "Gemini API", ProviderAuth::Key),
    ("nvidiabuild", "Nvidia Build", ProviderAuth::Key),
];

#[derive(Clone)]
pub(crate) enum Handle {
    Antigravity(Arc<crate::antigravity::Accounts>),
    Claude(Arc<crate::claude::ClaudeAccounts>),
    ChatGpt(Arc<crate::chatgpt::ChatGptAccounts>),
    ZCode(Arc<crate::zcode::ZCodeAccounts>),
    Grok(Arc<crate::grok::GrokAccounts>),
    WorkBuddy(Arc<crate::workbuddy::WorkBuddyAccounts>),
    DeepAi(Arc<crate::deepai::DeepAIAccounts>),
    CoArena(Arc<ApiKeyAccounts>),
    Keys(Arc<ApiKeyAccounts>),
}

impl Handle {
    fn accounts(&self) -> Vec<Account> {
        match self {
            Self::Antigravity(p) => p.list(),
            Self::Claude(p) => p.list(),
            Self::ChatGpt(p) => p.list(),
            Self::ZCode(p) => p.list(),
            Self::Grok(p) => p.list(),
            Self::WorkBuddy(p) => p.list(),
            Self::DeepAi(p) => p.list(),
            Self::CoArena(p) | Self::Keys(p) => p.list(),
        }
    }

    fn usage(&self) -> Vec<UsageBar> {
        match self {
            Self::Antigravity(p) => shares(p.quota(), false),
            Self::Claude(p) => shares(p.quota(), false),
            Self::ChatGpt(p) => shares(p.quota(), false),
            // Their windows reset on a fixed schedule, used or not
            Self::ZCode(p) => shares(p.quota(), true),
            Self::Grok(p) => shares(p.quota(), true),
            Self::WorkBuddy(p) => credits(p.credits()),
            Self::DeepAi(p) => credits(p.credits()),
            Self::CoArena(p) => {
                let quota = p.coarena_quota();
                let keys = p.clone();
                tokio::spawn(async move { keys.refresh_coarena().await });
                let bar = UsageBar {
                    label: None,
                    unit: UsageUnit::Usd,
                    used: quota.used_usd,
                    total: quota.total_usd,
                    reset: quota.reset.clone(),
                    idle: false,
                    partial: quota.partial,
                };
                quota.reset.map(|_| bar).into_iter().collect()
            }
            Self::Keys(_) => Vec::new(),
        }
    }

    async fn login(&self, language: Option<String>, timezone: Option<String>) -> Result<String, String> {
        match self {
            Self::Antigravity(p) => p.login().await,
            Self::Claude(p) => p.login(),
            Self::ChatGpt(p) => p.login().await,
            Self::ZCode(p) => p.login(language.unwrap_or_default(), timezone.unwrap_or_default()).await,
            Self::Grok(p) => p.login().await,
            Self::WorkBuddy(p) => p.login().await,
            Self::DeepAi(p) => p.login().await,
            Self::CoArena(_) | Self::Keys(_) => Err("This provider takes API keys, not a sign-in".into()),
        }
    }

    /// One account, or every one without `email`.
    fn remove(&self, email: Option<&str>, id: Option<&str>) -> Result<(), String> {
        match (self, email) {
            (Self::Antigravity(p), Some(email)) => p.remove(email),
            (Self::Antigravity(p), None) => p.remove_all(),
            (Self::Claude(p), Some(email)) => p.remove(email),
            (Self::Claude(p), None) => p.remove_all(),
            (Self::ChatGpt(p), Some(email)) => p.remove(email),
            (Self::ChatGpt(p), None) => p.remove_all(),
            (Self::ZCode(p), _) => p.remove(email),
            (Self::Grok(p), _) => p.remove(email),
            (Self::WorkBuddy(p), _) => p.remove(email),
            (Self::DeepAi(p), _) => return p.remove(email, id),
            (Self::CoArena(p) | Self::Keys(p), _) => p.remove(email),
        }
        Ok(())
    }

    /// Checks every account, as on startup.
    pub(crate) async fn check_all(&self) {
        match self {
            Self::Antigravity(p) => p.check_all(true).await,
            Self::Claude(p) => p.check_all().await,
            Self::ChatGpt(p) => p.check_all().await,
            Self::ZCode(p) => p.check_all().await,
            Self::Grok(p) => p.check_all().await,
            Self::WorkBuddy(p) => p.check_all().await,
            Self::DeepAi(p) => p.check_all().await,
            Self::CoArena(p) | Self::Keys(p) => p.check_all().await,
        }
    }

    /// The check every few minutes: mostly the accounts that hit a limit.
    pub(crate) async fn check_periodic(&self) {
        match self {
            Self::Antigravity(_) => {}
            Self::Claude(p) => p.check_limited().await,
            Self::ChatGpt(p) => p.check_periodic().await,
            Self::ZCode(p) => p.check_limited().await,
            Self::Grok(p) => p.check_all().await,
            Self::WorkBuddy(p) => p.check_all().await,
            Self::DeepAi(p) => p.check_limited().await,
            Self::CoArena(p) => p.refresh_coarena().await,
            Self::Keys(p) => p.check_limited().await,
        }
    }
}

fn shares(quota: Vec<Quota>, fixed_reset: bool) -> Vec<UsageBar> {
    quota
        .into_iter()
        .map(|q| UsageBar {
            idle: q.remaining >= 1.0 && !fixed_reset,
            label: Some(q.family),
            unit: UsageUnit::Share,
            used: 1.0 - q.remaining,
            total: 1.0,
            reset: q.reset,
            partial: false,
        })
        .collect()
}

fn credits(credits: Option<Credits>) -> Vec<UsageBar> {
    credits
        .map(|c| UsageBar {
            label: None,
            unit: UsageUnit::Credits,
            used: (c.total - c.remaining).max(0.0),
            total: c.total,
            reset: c.reset,
            idle: false,
            partial: false,
        })
        .into_iter()
        .collect()
}

/// Which providers are turned off; kept in `providers.json` of the data folder.
pub struct Switches {
    path: PathBuf,
    file: RwLock<SwitchesFile>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SwitchesFile {
    disabled: BTreeSet<String>,
    #[serde(default)]
    hidden_models: BTreeSet<ModelKey>,
}

impl Switches {
    pub fn load(data_dir: &Path) -> Arc<Self> {
        let path = data_dir.join("providers.json");
        let file: SwitchesFile =
            std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Arc::new(Self { path, file: RwLock::new(file) })
    }

    pub fn enabled(&self, provider: &str) -> bool {
        !self.file.read().unwrap().disabled.contains(provider)
    }

    fn set(&self, provider: &str, enabled: bool) -> std::io::Result<()> {
        self.change(|file| {
            if enabled {
                file.disabled.remove(provider);
            } else {
                file.disabled.insert(provider.to_owned());
            }
        })
    }

    fn hidden_models(&self) -> Vec<ModelKey> {
        self.file.read().unwrap().hidden_models.iter().cloned().collect()
    }

    fn set_hidden(&self, models: Vec<ModelKey>, hidden: bool) -> std::io::Result<()> {
        self.change(|file| {
            for model in models {
                if hidden {
                    file.hidden_models.insert(model);
                } else {
                    file.hidden_models.remove(&model);
                }
            }
        })
    }

    fn change(&self, edit: impl FnOnce(&mut SwitchesFile)) -> std::io::Result<()> {
        let mut current = self.file.write().unwrap();
        edit(&mut current);
        let file = current.clone();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&file).expect("serializable"))?;
        std::fs::rename(tmp, &self.path)
    }
}

/// The built-in providers, for checking them apart from the server.
#[derive(Clone)]
pub(crate) struct Registry {
    handles: Vec<(&'static str, ProviderAuth, Handle)>,
    switches: Arc<Switches>,
}

impl Registry {
    /// The providers that are on, account ones or key ones.
    pub(crate) fn enabled(&self, auth: ProviderAuth) -> Vec<Handle> {
        self.handles
            .iter()
            .filter(|(id, kind, _)| *kind == auth && self.switches.enabled(id))
            .map(|(.., handle)| handle.clone())
            .collect()
    }
}

/// The provider a built-in provider's old event is about, and the failed sign-in it reports.
fn event_provider(event: &Event) -> Option<(&'static str, Option<&str>)> {
    Some(match event {
        Event::AntigravityAccounts { .. } | Event::AntigravityQuota { .. } => ("antigravity", None),
        Event::AntigravityLoginFailed { message } => ("antigravity", Some(message)),
        Event::ClaudeAccounts { .. } | Event::ClaudeQuota { .. } => ("claude-account", None),
        Event::ChatGptAccounts { .. } | Event::ChatGptQuota { .. } => ("chatgpt-account", None),
        Event::ChatGptLoginFailed { message } => ("chatgpt-account", Some(message)),
        Event::GrokAccounts { .. } | Event::GrokQuota { .. } => ("grok-account", None),
        Event::GrokLoginFailed { message } => ("grok-account", Some(message)),
        Event::ZCodeAccounts { .. } | Event::ZCodeQuota { .. } => ("zcode-account", None),
        Event::ZCodeLoginFailed { message } => ("zcode-account", Some(message)),
        Event::WorkBuddyAccounts { .. } | Event::WorkBuddyCredits { .. } => ("workbuddy-account", None),
        Event::WorkBuddyLoginFailed { message } => ("workbuddy-account", Some(message)),
        Event::DeepAiAccounts { .. } | Event::DeepAiCredits { .. } => ("deepai-account", None),
        Event::DeepAiLoginFailed { message } => ("deepai-account", Some(message)),
        Event::AgentRouterAccounts { .. } => ("agentrouter", None),
        Event::CoArenaAccounts { .. } | Event::CoArenaQuota { .. } => ("coarena", None),
        Event::ClaudenAccounts { .. } => ("clauden", None),
        Event::GeminiAccounts { .. } => ("gemini", None),
        Event::NvidiaBuildAccounts { .. } => ("nvidiabuild", None),
        _ => return None,
    })
}

impl Server {
    pub(crate) fn handle_of(&self, provider: &str) -> Option<Handle> {
        Some(match provider {
            "antigravity" => Handle::Antigravity(self.antigravity.clone()),
            "claude-account" => Handle::Claude(self.claude.clone()),
            "chatgpt-account" => Handle::ChatGpt(self.chatgpt.clone()),
            "zcode-account" => Handle::ZCode(self.zcode.clone()),
            "grok-account" => Handle::Grok(self.grok.clone()),
            "workbuddy-account" => Handle::WorkBuddy(self.workbuddy.clone()),
            "deepai-account" => Handle::DeepAi(self.deepai.clone()),
            "coarena" => Handle::CoArena(self.coarena.clone()),
            "agentrouter" => Handle::Keys(self.agentrouter.clone()),
            "clauden" => Handle::Keys(self.clauden.clone()),
            "gemini" => Handle::Keys(self.gemini.clone()),
            "nvidiabuild" => Handle::Keys(self.nvidia_build.clone()),
            _ => return None,
        })
    }

    fn known(&self, provider: &str) -> Result<Handle, ProtocolError> {
        self.handle_of(provider).ok_or_else(|| failed(format!("Unknown provider: {provider}")))
    }

    pub(crate) fn registry(&self) -> Registry {
        let handles = BUILTIN.iter().filter_map(|(id, _, auth)| Some((*id, *auth, self.handle_of(id)?))).collect();
        Registry { handles, switches: self.switches.clone() }
    }

    pub(crate) fn catalog(&self) -> Vec<ProviderEntry> {
        let builtin = BUILTIN.iter().map(|(id, name, auth)| ProviderEntry {
            id: (*id).into(),
            name: (*name).into(),
            auth: *auth,
            accounts: self.handle_of(id).map_or(0, |handle| handle.accounts().len()),
            enabled: self.switches.enabled(id),
        });
        let custom = self.models.custom.catalog().into_iter().map(|provider| ProviderEntry {
            enabled: self.switches.enabled(&provider.id),
            id: provider.id,
            name: provider.name,
            auth: ProviderAuth::Custom,
            accounts: 1,
        });
        builtin.chain(custom).collect()
    }

    pub(crate) fn set_enabled(&self, provider: &str, enabled: bool) -> Result<Reply, ProtocolError> {
        self.switches.set(provider, enabled).map_err(|e| failed(e.to_string()))?;
        if enabled {
            self.check_all(provider)?;
        }
        let _ = self.events.send(Event::ProvidersChanged);
        let _ = self.events.send(Event::ProviderUpdated { provider: provider.to_owned() });
        Ok(Reply::Done(Done {}))
    }

    pub(crate) fn hidden_models(&self) -> Reply {
        Reply::HiddenModels(self.switches.hidden_models())
    }

    pub(crate) fn set_models_hidden(&self, models: Vec<ModelKey>, hidden: bool) -> Result<Reply, ProtocolError> {
        self.switches.set_hidden(models, hidden).map_err(|e| failed(e.to_string()))?;
        let _ = self.events.send(Event::ProvidersChanged);
        Ok(Reply::Done(Done {}))
    }

    pub(crate) fn accounts(&self, provider: &str) -> Result<Reply, ProtocolError> {
        Ok(Reply::Accounts(self.known(provider)?.accounts()))
    }

    pub(crate) fn usage(&self, provider: &str) -> Result<Reply, ProtocolError> {
        Ok(Reply::Usage(self.known(provider)?.usage()))
    }

    pub(crate) async fn login(
        &self,
        provider: &str,
        language: Option<String>,
        timezone: Option<String>,
    ) -> Result<Reply, ProtocolError> {
        let url = self.known(provider)?.login(language, timezone).await.map_err(failed)?;
        Ok(Reply::Login(nabu_protocol::LoginStarted { url }))
    }

    pub(crate) async fn finish_login(&self, provider: &str, code: &str) -> Result<Reply, ProtocolError> {
        match self.known(provider)? {
            Handle::Claude(claude) => claude.finish_login(code).await.map_err(failed)?,
            _ => return Err(failed(format!("{provider} signs in without a code"))),
        }
        Ok(Reply::Done(Done {}))
    }

    pub(crate) fn add_keys(&self, provider: &str, keys: Vec<String>) -> Result<Reply, ProtocolError> {
        let (Handle::CoArena(accounts) | Handle::Keys(accounts)) = self.known(provider)? else {
            return Err(failed(format!("{provider} signs in with an account, not API keys")));
        };
        let added = accounts.add(keys).map_err(failed)?;
        tokio::spawn(async move { accounts.check_added(added).await });
        Ok(Reply::Done(Done {}))
    }

    pub(crate) fn remove(&self, provider: &str, email: Option<&str>, id: Option<&str>) -> Result<Reply, ProtocolError> {
        self.known(provider)?.remove(email, id).map_err(failed)?;
        Ok(Reply::Done(Done {}))
    }

    pub(crate) fn check_all(&self, provider: &str) -> Result<Reply, ProtocolError> {
        // Custom providers are never checked
        if let Some(handle) = self.handle_of(provider) {
            tokio::spawn(async move { handle.check_all().await });
        }
        Ok(Reply::Done(Done {}))
    }

    /// Repeats the built-in providers' own events as `ProviderUpdated` and `ProviderLoginFailed`.
    pub(crate) fn forward_provider_events(&self) {
        let mut events = self.events.subscribe();
        let sender = self.events.clone();
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let Some((provider, failure)) = event_provider(&event) else { continue };
                let _ = sender.send(match failure {
                    Some(message) => Event::ProviderLoginFailed { provider: provider.into(), message: message.into() },
                    None => Event::ProviderUpdated { provider: provider.into() },
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("nabu-switches-{}", std::process::id()));
        let switches = Switches::load(&dir);
        assert!(switches.enabled("grok-account"));
        switches.set("grok-account", false).unwrap();
        assert!(!Switches::load(&dir).enabled("grok-account"));
        switches.set("grok-account", true).unwrap();
        assert!(Switches::load(&dir).enabled("grok-account"));
        let model = ModelKey { provider_id: "grok-account".into(), model_id: "grok-5".into() };
        switches.set_hidden(vec![model.clone()], true).unwrap();
        assert_eq!(Switches::load(&dir).hidden_models(), vec![model.clone()]);
        switches.set_hidden(vec![model], false).unwrap();
        assert!(Switches::load(&dir).hidden_models().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quotas_credits_and_money_become_the_same_bars() {
        let quota = |remaining| Quota { family: "5h".into(), remaining, reset: Some("2026-11-23T00:00:00Z".into()) };
        let [fresh] = shares(vec![quota(1.0)], false).try_into().unwrap();
        assert!(fresh.idle && fresh.used == 0.0 && fresh.label.as_deref() == Some("5h"));
        let [scheduled] = shares(vec![quota(1.0)], true).try_into().unwrap();
        assert!(!scheduled.idle);
        let [half] = shares(vec![quota(0.25)], false).try_into().unwrap();
        assert_eq!((half.used, half.total, half.unit), (0.75, 1.0, UsageUnit::Share));

        let [balance] = credits(Some(Credits { remaining: 30.0, total: 100.0, reset: None })).try_into().unwrap();
        assert_eq!((balance.used, balance.total, balance.unit), (70.0, 100.0, UsageUnit::Credits));
        assert!(credits(None).is_empty());
    }
}
