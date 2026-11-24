use chrono::{DateTime, Local, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Access {
    Available,
    PlanRequired,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reasoning {
    pub supported: bool,
    pub only: bool,
    pub can_disable: bool,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

impl Reasoning {
    pub(crate) fn from_model(model: &Value) -> Self {
        let supported = model["supportsReasoning"] == true;
        let only = model["onlyReasoning"] == true;
        let default_effort = model
            .pointer("/reasoning/defaultEffort")
            .or_else(|| model.pointer("/reasoning/effort"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut efforts = model
            .pointer("/reasoning/supportedEfforts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if efforts.is_empty()
            && let Some(default) = &default_effort
        {
            efforts.push(default.clone());
        }
        Self {
            supported,
            only,
            can_disable: supported
                && !only
                && model.pointer("/reasoning/canDisableThinking") != Some(&Value::Bool(false)),
            efforts,
            default_effort,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub access: Access,
    pub required_plan: Option<String>,
    pub free: bool,
    pub credits: Option<String>,
    pub active_promotion: Option<Value>,
    pub paid_alternative: Option<String>,
    pub reasoning: Reasoning,
    pub raw: Value,
}

/// A summary of credits, not token limits. Native decimal strings remain exact in `raw`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credits {
    pub raw: Value,
    pub fetched_at: u64,
}
impl Credits {
    pub fn remaining(&self) -> Option<&str> {
        self.raw["TotalCredits"].as_str()
    }
    pub fn plan_name(&self) -> Option<&str> {
        self.raw["SubscriptionPackageName"].as_str()
    }
    pub fn is_paid(&self) -> Option<bool> {
        self.raw["IsPaidUser"].as_bool()
    }
    pub fn groups(&self) -> &[Value] {
        self.raw["CreditGroups"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn packages(&self) -> &[Value] {
        self.raw["Packages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn rank(&self) -> Option<u8> {
        if self.is_paid() == Some(false) {
            return Some(1);
        }
        match self.plan_name()?.to_ascii_lowercase().as_str() {
            "free" | "youth" => Some(1),
            "pro" | "standard" => Some(2),
            "advanced" => Some(3),
            "flagship" => Some(4),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub raw: Value,
    pub fetched_at: u64,
}
impl Catalog {
    pub fn available(&self) -> impl Iterator<Item = &Model> {
        self.models.iter().filter(|m| m.access == Access::Available)
    }
    pub fn from_config(raw: Value, credits: Option<&Credits>, now: u64) -> Result<Self, String> {
        let list = raw["models"].as_array().ok_or("missing WorkBuddy models")?;
        let models = list
            .iter()
            .filter(|v| conversation_model(v))
            .filter_map(|v| {
                let id = v["id"].as_str()?;
                let name = v["name"].as_str().unwrap_or(id);
                let tier = v
                    .get("tier")
                    .filter(|v| v.is_object() && v["enabled"] != false)
                    .or_else(|| highest(&raw["modelTiers"], id));
                let required = tier.and_then(|t| t["requiredUserType"].as_str());
                let access = if v["disabled"] == true
                    || v["busyLevel"] == "unavailable"
                    || !listed(&raw["availableModels"], id)
                {
                    Access::Disabled
                } else if let Some(required) = required {
                    match (rank(required), credits.and_then(Credits::rank)) {
                        (Some(need), Some(have)) if have < need => Access::PlanRequired,
                        (Some(_), Some(_)) => Access::Available,
                        _ => Access::Unknown,
                    }
                } else {
                    Access::Available
                };
                let promotions = v.get("promotions").unwrap_or(&raw["modelPromotions"]);
                let active = promotions
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| {
                        (v.get("promotions").is_some() || contains(&p["modelIds"], id))
                            && promotion_active(p, now)
                    })
                    .max_by_key(|p| p["priority"].as_i64().unwrap_or(0))
                    .cloned();
                let base = v["credits"].as_str();
                let price = active
                    .as_ref()
                    .and_then(|p| p.pointer("/discount/discountedCredits"))
                    .and_then(Value::as_str)
                    .or(base);
                let free = price.and_then(coefficient) == Some(0.0);
                let paid = raw
                    .pointer("/productFeaturesConfig/ModelRateLimitCap/lines")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|p| p["freeId"] == id)
                    .and_then(|p| p["paidId"].as_str())
                    .map(str::to_owned);
                Some(Model {
                    id: id.into(),
                    name: name.into(),
                    display_name: if free {
                        format!("{name} Free")
                    } else {
                        name.into()
                    },
                    access,
                    required_plan: required.map(str::to_owned),
                    free,
                    credits: price.map(str::to_owned),
                    active_promotion: active,
                    paid_alternative: paid,
                    reasoning: Reasoning::from_model(v),
                    raw: v.clone(),
                })
            })
            .collect();
        Ok(Self {
            models,
            raw,
            fetched_at: now,
        })
    }
}

fn conversation_model(v: &Value) -> bool {
    !v["tags"].as_array().into_iter().flatten().any(|t| {
        matches!(
            t.as_str(),
            Some("text-to-image" | "image-to-image" | "text-to-video" | "image-to-video")
        )
    })
}
fn listed(v: &Value, id: &str) -> bool {
    !v.is_array() || contains(v, id)
}
fn contains(v: &Value, id: &str) -> bool {
    v.as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(id)))
}
fn highest<'a>(v: &'a Value, id: &str) -> Option<&'a Value> {
    v.as_array()?
        .iter()
        .filter(|t| t["enabled"] != false && contains(&t["modelIds"], id))
        .max_by_key(|t| t["priority"].as_i64().unwrap_or(0))
}
fn rank(s: &str) -> Option<u8> {
    match s {
        "trial" | "free" | "youth" => Some(1),
        "standard" | "pro" => Some(2),
        "advanced" => Some(3),
        "flagship" => Some(4),
        _ => None,
    }
}
fn coefficient(s: &str) -> Option<f64> {
    s.trim()
        .trim_start_matches('x')
        .split_whitespace()
        .next()?
        .trim_end_matches('x')
        .parse()
        .ok()
}
fn minute(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn promotion_active(p: &Value, now: u64) -> bool {
    if p["enabled"] == false {
        return false;
    }
    let s = &p["schedule"];
    for (key, start) in [("validFrom", true), ("validUntil", false)] {
        if let Some(value) = s[key].as_str() {
            let Ok(date) = DateTime::parse_from_rfc3339(value) else {
                return false;
            };
            if (start && (now as i128) < date.timestamp_millis() as i128)
                || (!start && (now as i128) >= date.timestamp_millis() as i128)
            {
                return false;
            }
        }
    }
    let Some(windows) = s["daily"].as_array().filter(|a| !a.is_empty()) else {
        return true;
    };
    let Some(dt) = DateTime::<Utc>::from_timestamp_millis(now.min(i64::MAX as u64) as i64) else {
        return false;
    };
    let current = if let Some(tz) = s["timezone"].as_str() {
        let Ok(tz) = tz.parse::<Tz>() else {
            return false;
        };
        let local = dt.with_timezone(&tz);
        local.hour() * 60 + local.minute()
    } else {
        let local = dt.with_timezone(&Local);
        local.hour() * 60 + local.minute()
    };
    windows.iter().any(|w| {
        let Some((start, end)) = w["start"]
            .as_str()
            .and_then(minute)
            .zip(w["end"].as_str().and_then(minute))
        else {
            return false;
        };
        if start == end {
            true
        } else if start < end {
            current >= start && current < end
        } else {
            current >= start || current < end
        }
    })
}
