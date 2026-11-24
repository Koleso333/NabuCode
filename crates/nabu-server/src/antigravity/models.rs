use nabu_protocol::{Model, Quota};
use serde_json::Value;

const EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// Chat models from a raw `fetchAvailableModels` reply, named from their ids.
pub fn chat_models(reply: &Value) -> Vec<Model> {
    let mut out: Vec<Model> = reply["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, info)| chat_model(id, info))
        .collect();
    out.sort_by(crate::models::strongest_first);
    out
}

/// Quota per family; every model of a family shares one, so the lowest one stands for it.
pub fn quotas(reply: &Value) -> Vec<Quota> {
    let mut out: Vec<Quota> = Vec::new();
    for (id, info) in reply["models"].as_object().into_iter().flatten() {
        let (Some(model), Some(remaining)) = (
            chat_model(id, info),
            info["quotaInfo"]["remainingFraction"].as_f64(),
        ) else {
            continue;
        };
        let reset = info["quotaInfo"]["resetTime"].as_str().map(str::to_owned);
        match out.iter_mut().find(|q| q.family == model.family) {
            Some(q) if remaining < q.remaining => {
                *q = Quota {
                    family: model.family,
                    remaining,
                    reset,
                }
            }
            Some(_) => {}
            None => out.push(Quota {
                family: model.family,
                remaining,
                reset,
            }),
        }
    }
    out.sort_by(|a, b| a.family.cmp(&b.family));
    out
}

fn chat_model(id: &str, info: &Value) -> Option<Model> {
    let family = family(id)?;
    if info["isInternal"] == true || ["image", "agent", "latest"].iter().any(|w| id.contains(w)) {
        return None;
    }
    let name = display_name(id);
    // Google keeps old ids as aliases of newer models; their display name gives them away.
    if let Some(shown) = info["displayName"].as_str()
        && shown.split(" (").next() != Some(name.as_str())
    {
        return None;
    }
    Some(Model {
        id: id.to_owned(),
        name,
        family: family.to_owned(),
        efforts: efforts(id, family),
        context: info["maxTokens"].as_u64().unwrap_or(0),
        output: info["maxOutputTokens"].as_u64().unwrap_or(0),
        images: info["supportsImages"] == true,
        fast: false,
        older: false,
    })
}

fn family(id: &str) -> Option<&'static str> {
    if id.starts_with("claude-") {
        Some("claude")
    } else if id.starts_with("gemini-") {
        Some("gemini")
    } else {
        None
    }
}

/// `claude-opus-4-6-thinking` → `Claude Opus 4.6`, `gemini-3.8-flash-tiered` → `Gemini 3.8 Flash`.
pub fn display_name(id: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for part in id.split('-') {
        if ["thinking", "tiered"].contains(&part) || EFFORTS.contains(&part) {
            continue;
        }
        let numeric = part.chars().all(|c| c.is_ascii_digit() || c == '.');
        match words.last_mut() {
            Some(last) if numeric && last.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                last.push('.');
                last.push_str(part);
            }
            _ => words.push(capitalize(part)),
        }
    }
    words.join(" ")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

// Measured against Cloud Code: Gemini 3+ takes thinkingLevel, tiered models reject MINIMAL.
fn efforts(id: &str, family: &str) -> Vec<String> {
    let major = id
        .split('-')
        .nth(1)
        .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    if family != "gemini" || major < Some(3) {
        return Vec::new();
    }
    EFFORTS
        .iter()
        .filter(|e| !(id.contains("tiered") && **e == "minimal"))
        .map(|e| e.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quota_is_per_family() {
        let reply = json!({"models": {
            "claude-opus-4-6-thinking": {"quotaInfo": {"remainingFraction": 0.8, "resetTime": "2026-11-02T01:24:04Z"}},
            "claude-sonnet-4-6": {"quotaInfo": {"remainingFraction": 0.8, "resetTime": "2026-11-02T01:24:04Z"}},
            "gemini-3-flash": {"quotaInfo": {"remainingFraction": 1, "resetTime": "2026-11-04T01:12:30Z"}},
            "gemini-3.1-flash-image": {"quotaInfo": {"remainingFraction": 0.1}},
        }});
        let quota = quotas(&reply);
        assert_eq!(quota.len(), 2);
        assert_eq!(
            (quota[0].family.as_str(), quota[0].remaining),
            ("claude", 0.8)
        );
        assert_eq!(
            (quota[1].family.as_str(), quota[1].remaining),
            ("gemini", 1.0)
        );
    }

    #[test]
    fn names_come_from_ids() {
        assert_eq!(display_name("claude-opus-4-6-thinking"), "Claude Opus 4.6");
        assert_eq!(display_name("claude-sonnet-4-6"), "Claude Sonnet 4.6");
        assert_eq!(display_name("gemini-3.8-flash-tiered"), "Gemini 3.8 Flash");
        assert_eq!(
            display_name("gemini-3.5-flash-lite"),
            "Gemini 3.5 Flash Lite"
        );
        assert_eq!(display_name("gemini-3.6-flash-high"), "Gemini 3.6 Flash");
    }

    #[test]
    fn keeps_chat_models_and_drops_aliases() {
        let reply = json!({"models": {
            "claude-opus-4-6-thinking": {"displayName": "Claude Opus 4.6 (Thinking)"},
            "gemini-3.8-flash-tiered": {},
            "gemini-2.5-flash": {"displayName": "Gemini 3.5 Flash Lite"},
            "gemini-3.1-flash-image": {"displayName": "Gemini 3.1 Flash Image"},
            "chat_20706": {"isInternal": true},
            "gpt-oss-120b-medium": {"displayName": "GPT-OSS 120B (Medium)"},
        }});
        let ids: Vec<_> = chat_models(&reply).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["claude-opus-4-6-thinking", "gemini-3.8-flash-tiered"]);
    }

    #[test]
    fn strongest_models_come_first() {
        let ids = [
            "gemini-3.1-flash-lite",
            "gemini-2.5-pro",
            "claude-sonnet-4-6",
            "gemini-3-flash",
            "gemini-3.5-flash-lite",
            "claude-opus-4-6-thinking",
            "gemini-3.8-flash-tiered",
        ];
        let mut models: Vec<Model> = ids
            .iter()
            .filter_map(|id| chat_model(id, &json!({})))
            .collect();
        models.sort_by(crate::models::strongest_first);
        let names: Vec<_> = models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Claude Opus 4.6",
                "Claude Sonnet 4.6",
                "Gemini 3.8 Flash",
                "Gemini 3 Flash",
                "Gemini 2.5 Pro",
                "Gemini 3.5 Flash Lite",
                "Gemini 3.1 Flash Lite",
            ]
        );
    }

    #[test]
    fn efforts_follow_measured_support() {
        assert_eq!(
            efforts("gemini-3-flash", "gemini"),
            ["minimal", "low", "medium", "high"]
        );
        assert_eq!(
            efforts("gemini-3.8-flash-tiered", "gemini"),
            ["low", "medium", "high"]
        );
        assert!(efforts("gemini-2.5-pro", "gemini").is_empty());
        assert!(efforts("claude-opus-4-6-thinking", "claude").is_empty());
    }
}
