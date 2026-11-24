use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nabu_protocol::{Model, ModelRef, Provider};
use ncore::core::{ErrorKind, ProviderError, ir};
use ncore::{Core, Endpoint, GenerateOptions};

use crate::antigravity::Accounts;
use crate::chatgpt::ChatGptAccounts;
use crate::claude::ClaudeAccounts;

/// Where requests for each provider go.
pub struct Models {
    pub core: Core,
    antigravity: Arc<Accounts>,
    claude: Arc<ClaudeAccounts>,
    chatgpt: Arc<ChatGptAccounts>,
    zcode: Arc<crate::zcode::ZCodeAccounts>,
    grok: Arc<crate::grok::GrokAccounts>,
    workbuddy: Arc<crate::workbuddy::WorkBuddyAccounts>,
    deepai: Arc<crate::deepai::DeepAIAccounts>,
    agentrouter: Arc<crate::api_keys::ApiKeyAccounts>,
    coarena: Arc<crate::api_keys::ApiKeyAccounts>,
    clauden: Arc<crate::api_keys::ApiKeyAccounts>,
    gemini: Arc<crate::api_keys::ApiKeyAccounts>,
    nvidia_build: Arc<crate::api_keys::ApiKeyAccounts>,
    pub custom: crate::custom_providers::CustomProviders,
    switches: Arc<crate::providers::Switches>,
    /// When each provider's quota was last asked for.
    quota_asked: Mutex<HashMap<String, Instant>>,
}

/// How one request is sent.
pub struct Route {
    pub endpoint: Endpoint,
    pub options: GenerateOptions,
    /// The account chosen among several, when the provider has a choice.
    pub account: Option<String>,
}

impl Models {
    pub fn new(
        core: Core,
        antigravity: Arc<Accounts>,
        claude: Arc<ClaudeAccounts>,
        chatgpt: Arc<ChatGptAccounts>,
        zcode: Arc<crate::zcode::ZCodeAccounts>,
        workbuddy: Arc<crate::workbuddy::WorkBuddyAccounts>,
        deepai: Arc<crate::deepai::DeepAIAccounts>,
        grok: Arc<crate::grok::GrokAccounts>,
        agentrouter: Arc<crate::api_keys::ApiKeyAccounts>,
        coarena: Arc<crate::api_keys::ApiKeyAccounts>,
        clauden: Arc<crate::api_keys::ApiKeyAccounts>,
        gemini: Arc<crate::api_keys::ApiKeyAccounts>,
        nvidia_build: Arc<crate::api_keys::ApiKeyAccounts>,
        custom: crate::custom_providers::CustomProviders,
        switches: Arc<crate::providers::Switches>,
    ) -> Self {
        Self {
            core,
            antigravity,
            claude,
            chatgpt,
            zcode,
            workbuddy,
            deepai,
            grok,
            agentrouter,
            coarena,
            clauden,
            gemini,
            nvidia_build,
            custom,
            switches,
            quota_asked: Mutex::default(),
        }
    }

    /// The endpoint for `model` in `session`; also applies what its provider needs to `request`.
    pub fn route(
        &self,
        session: &str,
        model: &ModelRef,
        request: &mut ir::Request,
    ) -> Result<Route, String> {
        match model.provider_id.as_str() {
            "antigravity" => {
                let endpoint = self
                    .antigravity
                    .endpoint()
                    .ok_or("No working Antigravity account")?;
                Ok(Route {
                    endpoint,
                    options: GenerateOptions::default(),
                    account: None,
                })
            }
            "claude-account" => self.claude.route(session, model, request),
            "chatgpt-account" => self.chatgpt.route(session, model, request),
            "zcode-account" => self.zcode.route(session, model, request),
            "grok-account" => self.grok.route(session, model),
            "workbuddy-account" => self.workbuddy.route(session, model, request),
            "deepai-account" => self.deepai.route(session, model, request),
            "agentrouter" => self.agentrouter.route(session, model, request),
            "coarena" => self.coarena.route(session, model, request),
            "clauden" => self.clauden.route(session, model, request),
            "gemini" => self.gemini.route(session, model, request),
            "nvidiabuild" => self.nvidia_build.route(session, model, request),
            _ => self.custom.route(session, model),
        }
    }

    /// Connected providers, in the order the UI lists them.
    pub fn providers(&self) -> Vec<Provider> {
        [
            self.claude.provider(),
            self.chatgpt.provider(),
            self.zcode.provider(),
            self.workbuddy.provider(),
            self.deepai.provider(),
            self.grok.provider(),
            self.antigravity.provider(),
            self.agentrouter.provider(),
            self.coarena.provider(),
            self.clauden.provider(),
            self.gemini.provider(),
            self.nvidia_build.provider(),
        ]
        .into_iter()
        .flatten()
        .chain(self.custom.catalog())
        .filter(|provider| self.switches.enabled(&provider.id))
        .collect()
    }

    pub fn limits(&self, model: &ModelRef) -> Option<(u64, u64)> {
        let provider = self
            .providers()
            .into_iter()
            .find(|p| p.id == model.provider_id)?;
        let model = provider
            .models
            .into_iter()
            .find(|m| m.id == model.model_id)?;
        Some((model.context, model.output))
    }

    /// Whether `model` reads images; one the providers no longer list is taken to.
    pub fn takes_images(&self, model: &ModelRef) -> bool {
        self.providers()
            .into_iter()
            .find(|p| p.id == model.provider_id)
            .and_then(|p| p.models.into_iter().find(|m| m.id == model.model_id))
            .is_none_or(|m| m.images)
    }

    /// Display name of `model`, or its id when the provider no longer lists it.
    pub fn name(&self, model: &ModelRef) -> String {
        self.providers()
            .into_iter()
            .find(|p| p.id == model.provider_id)
            .and_then(|p| p.models.into_iter().find(|m| m.id == model.model_id))
            .map_or_else(|| model.model_id.clone(), |m| m.name)
    }

    /// The route's account answered that it is out of quota.
    pub fn limited(&self, model: &ModelRef, account: &str, error: &mut ProviderError) -> bool {
        let message = &error.message;
        match model.provider_id.as_str() {
            "deepai-account" => return self.deepai.limited(account, &model.model_id, error),
            "agentrouter" => return self.agentrouter.limited(account, error),
            "gemini" => return self.gemini.gemini_refused(account, &model.model_id, error),
            _ if error.kind != ErrorKind::RateLimited => return false,
            "claude-account" => self.claude.limited(account, message),
            "chatgpt-account" => self.chatgpt.limited(account, message),
            "coarena" => return self.coarena.limited(account, error),
            "clauden" => return self.clauden.limited(account, error),
            "nvidiabuild" => return self.nvidia_build.limited(account, error),
            "zcode-account" => return self.zcode.limited(account, &model.model_id, message),
            "grok-account" => return self.grok.limited(account),
            "workbuddy-account" => {
                return self.workbuddy.limited(account, &model.model_id, message);
            }
            _ => {}
        }
        true
    }

    /// Reads the provider's quota again in the background, unless it was asked for less than `gap` ago.
    pub fn refresh_quota(&self, provider: &str, gap: Duration) {
        {
            let mut asked = self.quota_asked.lock().unwrap();
            if asked.get(provider).is_some_and(|at| at.elapsed() < gap) {
                return;
            }
            asked.insert(provider.to_owned(), Instant::now());
        }
        match provider {
            "claude-account" => {
                let accounts = self.claude.clone();
                tokio::spawn(async move { accounts.refresh_quota().await });
            }
            "chatgpt-account" => {
                let accounts = self.chatgpt.clone();
                tokio::spawn(async move { accounts.refresh_quota().await });
            }
            "zcode-account" => {
                let accounts = self.zcode.clone();
                tokio::spawn(async move { accounts.refresh_quota().await });
            }
            "grok-account" => {
                let accounts = self.grok.clone();
                tokio::spawn(async move { accounts.refresh_quota().await });
            }
            "workbuddy-account" => {
                let accounts = self.workbuddy.clone();
                tokio::spawn(async move { accounts.refresh_credits().await });
            }
            "deepai-account" => {
                let accounts = self.deepai.clone();
                tokio::spawn(async move { accounts.refresh_credits().await });
            }
            "antigravity" => {
                let accounts = self.antigravity.clone();
                tokio::spawn(async move { accounts.refresh_quota().await });
            }
            _ => {}
        }
    }
}

/// Claude, GPT, Gemini, then other families; strongest lines and newer versions first.
pub fn strongest_first(a: &Model, b: &Model) -> std::cmp::Ordering {
    let a_rank = rank(a);
    let b_rank = rank(b);
    a_rank
        .0
        .cmp(&b_rank.0)
        .then_with(|| {
            if a_rank.0 == 3 {
                a.family.cmp(&b.family)
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| a_rank.cmp(&b_rank))
        .then_with(|| a.name.cmp(&b.name))
}

/// Sorts strongest first and marks every model but the newest of its tier as older.
pub fn sort_and_mark_older(models: &mut [Model]) {
    models.sort_by(strongest_first);
    mark_older(models);
}

pub(crate) fn model_line(model: &Model) -> String {
    model
        .name
        .rsplit(['-', ' '])
        .next()
        .unwrap_or("")
        .to_lowercase()
}

pub(crate) fn mark_older(models: &mut [Model]) {
    for i in 0..models.len() {
        let (newer, rest) = models.split_at_mut(i);
        let tier = |m: &Model| {
            let (family, lite, tier, _, gemini) = rank(m);
            let line = if m.family == "gpt" {
                model_line(m)
            } else {
                m.family.clone()
            };
            (family, lite, tier, gemini, line)
        };
        rest[0].older = newer.iter().any(|m| tier(m) == tier(&rest[0]));
    }
}

fn rank(model: &Model) -> (u8, bool, u8, std::cmp::Reverse<Vec<u32>>, u8) {
    let words: Vec<String> = model
        .name
        .split([' ', '-'])
        .map(str::to_lowercase)
        .collect();
    let has = |w: &str| words.iter().any(|x| x == w);
    let family = match model.family.as_str() {
        "claude" => 0,
        "gpt" => 1,
        "gemini" => 2,
        _ => 3,
    };
    let claude_tier = ["fable", "opus", "sonnet", "haiku"]
        .iter()
        .position(|t| has(t))
        .unwrap_or(4) as u8;
    let version: Vec<u32> = words
        .iter()
        .skip_while(|w| !w.starts_with(|c: char| c.is_ascii_digit()))
        .take_while(|w| w.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .flat_map(|v| v.split('.').filter_map(|n| n.parse().ok()))
        .collect();
    let gemini_tier = ["pro", "flash"].iter().position(|t| has(t)).unwrap_or(2) as u8;
    let tier = match model.family.as_str() {
        "claude" => claude_tier,
        "gpt" => u8::from(model_line(model) != "astra"),
        _ => 0,
    };
    (
        family,
        has("lite"),
        tier,
        std::cmp::Reverse(version),
        gemini_tier,
    )
}

pub(crate) fn openai_model(info: &serde_json::Value) -> Option<Model> {
    let id = info["slug"].as_str().or_else(|| info["id"].as_str())?;
    let images = info["input_modalities"]
        .as_array()
        .is_none_or(|m| m.iter().any(|v| v == "image"));
    Some(Model {
        id: id.to_owned(),
        name: info["display_name"]
            .as_str()
            .or_else(|| info["name"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| openai_name(id)),
        family: "gpt".into(),
        efforts: info["supported_reasoning_levels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|level| level["effort"].as_str().map(str::to_owned))
            .collect(),
        context: info["context_window"].as_u64().unwrap_or(0),
        output: 0,
        images,
        fast: info["service_tiers"]
            .as_array()
            .is_some_and(|tiers| tiers.iter().any(|t| t["id"] == "priority")),
        older: false,
    })
}

fn openai_name(id: &str) -> String {
    id.split('-')
        .map(|word| {
            if word == "gpt" {
                return "GPT".into();
            }
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(name: &str) -> Model {
        Model {
            id: name.to_lowercase().replace([' ', '.'], "-"),
            name: name.into(),
            family: "claude".into(),
            efforts: Vec::new(),
            context: 0,
            output: 0,
            images: true,
            fast: false,
            older: false,
        }
    }

    #[test]
    fn only_the_newest_of_a_tier_is_current() {
        let mut models: Vec<Model> = [
            "Claude Opus 4.8",
            "Claude Haiku 4.5",
            "Claude Fable 5.1",
            "Claude Opus 5.5",
            "Claude Fable 5",
        ]
        .map(claude)
        .into();
        sort_and_mark_older(&mut models);
        let listed: Vec<_> = models.iter().map(|m| (m.name.as_str(), m.older)).collect();
        assert_eq!(
            listed,
            [
                ("Claude Fable 5.1", false),
                ("Claude Fable 5", true),
                ("Claude Opus 5.5", false),
                ("Claude Opus 4.8", true),
                ("Claude Haiku 4.5", false),
            ]
        );
    }
}
