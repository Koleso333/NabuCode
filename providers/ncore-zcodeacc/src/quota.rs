use std::collections::BTreeSet;

use serde_json::Value;

use crate::credentials::unix_ms;

/// Native balance data preserves individual plans, promo buckets, expiry and unknown fields.
#[derive(Debug, Clone)]
pub struct UsageSnapshot {
    pub value: Value,
    pub fetched_at: u64,
    pub stale: bool,
    pub refresh_error: Option<String>,
}

impl UsageSnapshot {
    /// Catalog candidates from currently active balance buckets, including exhausted ones.
    pub fn model_ids(&self) -> Vec<String> {
        let Some(balances) = self.value["balances"].as_array() else {
            return Vec::new();
        };
        let plans = self.value["plans"].as_array();
        let now = self.value["server_time"]
            .as_u64()
            .unwrap_or(self.fetched_at / 1000)
            .saturating_add(unix_ms().saturating_sub(self.fetched_at) / 1000);
        let mut ids = BTreeSet::new();
        for bucket in balances {
            if number(&bucket["expires_at"]).is_some_and(|expiry| expiry > 0 && expiry <= now) {
                continue;
            }
            if let Some(plans) = plans {
                let owners: Vec<_> = plans
                    .iter()
                    .filter(|plan| {
                        if let Some(user_plan) = bucket["user_plan_id"].as_str() {
                            plan["user_plan_id"].as_str() == Some(user_plan)
                        } else {
                            bucket["plan_id"]
                                .as_str()
                                .is_some_and(|id| plan["plan_id"].as_str() == Some(id))
                        }
                    })
                    .collect();
                if !owners.is_empty()
                    && !owners.iter().any(|p| {
                        p["status"].as_str() == Some("active")
                            && !number(&p["ends_at"])
                                .is_some_and(|expiry| expiry > 0 && expiry <= now)
                    })
                {
                    continue;
                }
            }
            let mut found = false;
            if let Some(capabilities) = bucket["capabilities"].as_array() {
                for capability in capabilities.iter().filter_map(Value::as_str) {
                    if let Some(id) = capability
                        .strip_prefix("model:")
                        .filter(|id| !id.trim().is_empty())
                    {
                        ids.insert(id.to_ascii_lowercase());
                        found = true;
                    }
                }
            }
            if !found
                && let Some(id) = bucket["show_name"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
            {
                ids.insert(id.to_ascii_lowercase());
            }
        }
        ids.into_iter().collect()
    }
}

fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
