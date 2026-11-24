use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use nabu_protocol::{Model, Quota};
use ncore_chatgptsub::ChatGptSub;
use ncore_tokio::HyperTransport;
use serde_json::Value;

use crate::models::{mark_older, model_line};

const FALLBACK_VERSION: &str = "0.169.0";
const CODEX_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

pub struct CodexVersion {
    current: String,
    next_check: Instant,
}

impl Default for CodexVersion {
    fn default() -> Self {
        Self {
            current: FALLBACK_VERSION.into(),
            next_check: Instant::now(),
        }
    }
}

impl CodexVersion {
    pub fn current(&self) -> &str {
        &self.current
    }

    pub async fn refresh_if_due(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_check {
            return false;
        }

        let release = latest_release().await;
        let delay = if release.is_some() { 60 * 60 } else { 5 * 60 };
        self.next_check = now + Duration::from_secs(delay);
        if let Some(version) = release
            && version_parts(&version) > version_parts(&self.current)
        {
            self.current = version;
        }
        true
    }
}

async fn latest_release() -> Option<String> {
    let response = reqwest::Client::builder()
        .user_agent("NabuCode")
        .timeout(Duration::from_secs(4))
        .build()
        .ok()?
        .get(CODEX_RELEASE_URL)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let release: Value = serde_json::from_slice(&response.bytes().await.ok()?).ok()?;
    release_version(&release).map(str::to_owned)
}

fn release_version(release: &Value) -> Option<&str> {
    let version = release["tag_name"].as_str()?.strip_prefix("rust-v")?;
    version_parts(version)?;
    Some(version)
}

fn version_parts(version: &str) -> Option<[u32; 3]> {
    let mut parts = version.split('.');
    let numbers = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(numbers)
}

/// Models the account can use, in the backend's own order (strongest first).
pub async fn models(
    plugin: &ChatGptSub,
    net: &HyperTransport,
    client_version: &str,
) -> Result<Vec<Model>, String> {
    let catalog = plugin
        .models(net, client_version)
        .await
        .map_err(|e| format!("Model list request failed: {e}"))?;
    let mut list: Vec<&Value> = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list")
        .collect();
    list.sort_by_key(|m| m["priority"].as_i64().unwrap_or(i64::MAX));
    let mut models: Vec<Model> = list
        .into_iter()
        .filter_map(crate::models::openai_model)
        .collect();
    // The backend puts the newest model first, but Astra is the strongest line.
    models.sort_by_key(|m| model_line(m) != "astra");
    mark_older(&mut models);
    Ok(models)
}

pub struct Usage {
    pub plan: Option<String>,
    pub email: Option<String>,
    pub quota: Vec<Quota>,
}

/// Plan, email and the rate-limit windows as quota bars.
pub async fn usage(plugin: &ChatGptSub, net: &HyperTransport) -> Usage {
    let Ok(snapshot) = plugin.usage(net).await else {
        return Usage {
            plan: None,
            email: None,
            quota: Vec::new(),
        };
    };
    let value = &snapshot.value;
    let limits = &value["rate_limit"];
    let quota = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| window(&limits[*key]))
        .collect();
    Usage {
        plan: value["plan_type"].as_str().map(plan_name),
        email: value["email"].as_str().map(str::to_owned),
        quota,
    }
}

fn window(window: &Value) -> Option<Quota> {
    let used = window["used_percent"].as_f64()?;
    let family = match window["limit_window_seconds"].as_u64() {
        Some(18_000) => "5h".to_owned(),
        Some(604_800) => "week".to_owned(),
        Some(seconds) if seconds % 86_400 == 0 => format!("{}d", seconds / 86_400),
        Some(seconds) => format!("{}h", seconds / 3600),
        None => "limit".to_owned(),
    };
    let reset = window["reset_at"]
        .as_i64()
        .or_else(|| {
            window["reset_after_seconds"]
                .as_i64()
                .map(|s| chrono::Utc::now().timestamp() + s)
        })
        .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
        .map(|at| at.to_rfc3339());
    Some(Quota {
        family,
        remaining: (1.0 - used / 100.0).clamp(0.0, 1.0),
        reset,
    })
}

/// `plus` → `Plus`.
fn plan_name(plan: &str) -> String {
    let mut chars = plan.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The email inside the access token, for when the usage reply has none.
pub fn email(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?)
            .ok()?;
    claims["https://api.openai.com/profile"]["email"]
        .as_str()
        .or(claims["email"].as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_release_versions_are_stable_and_compared_numerically() {
        let stable = json!({"tag_name": "rust-v0.170.0"});
        assert_eq!(release_version(&stable), Some("0.170.0"));
        assert!(version_parts("0.170.0") > version_parts("0.99.9"));
        assert_eq!(
            release_version(&json!({"tag_name": "rust-v0.171.0-alpha.1"})),
            None
        );
        assert_eq!(release_version(&json!({"tag_name": "v0.171.0"})), None);
    }

    #[test]
    fn catalog_entries_become_models() {
        let entry = json!({
            "slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "visibility": "list", "priority": 1,
            "context_window": 500000, "input_modalities": ["text", "image"],
            "supported_reasoning_levels": [{"effort": "low"}, {"effort": "ultra"}],
            "service_tiers": [{"id": "priority", "name": "Fast"}],
        });
        let m = crate::models::openai_model(&entry).unwrap();
        assert_eq!(
            (m.id.as_str(), m.name.as_str(), m.context, m.images, m.fast),
            ("gpt-6.1-sol", "GPT-6.1-Sol", 500_000, true, true)
        );
        assert_eq!(m.efforts, ["low", "ultra"]);
        let mut models: Vec<Model> = ["GPT-6.1-Sol", "GPT-6.1 Terra", "GPT-6-Sol", "GPT-5.6-Luna"]
            .iter()
            .map(|name| Model {
                name: name.to_string(),
                ..m.clone()
            })
            .collect();
        mark_older(&mut models);
        assert_eq!(
            models.iter().map(|m| m.older).collect::<Vec<_>>(),
            [false, false, true, false]
        );
    }

    #[test]
    fn windows_become_quota() {
        let five = window(&json!({"used_percent": 25.0, "limit_window_seconds": 18000, "reset_at": 1_800_000_000})).unwrap();
        assert_eq!((five.family.as_str(), five.remaining), ("5h", 0.75));
        assert!(five.reset.unwrap().starts_with("2027-01-15T"));
        assert_eq!(
            window(&json!({"used_percent": 100, "limit_window_seconds": 604800}))
                .unwrap()
                .family,
            "week"
        );
        assert_eq!(plan_name("pro"), "Pro");
    }
}
