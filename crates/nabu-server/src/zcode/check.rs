use std::collections::HashMap;

use nabu_protocol::{AccountStatus, Model, Quota};
use ncore_zcodeacc::UsageSnapshot;
use serde_json::Value;

pub struct Report {
    pub tier: Option<String>,
    pub models: Vec<Model>,
    pub quota: Vec<Quota>,
    pub remaining: HashMap<String, f64>,
    pub status: AccountStatus,
}

pub fn report(snapshot: &UsageSnapshot) -> Report {
    let ids = snapshot.model_ids();
    let models = ids.iter().map(|id| model(id)).collect::<Vec<_>>();
    let plans = snapshot.value["plans"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp() as f64;
    let active = |plan: &&Value| {
        plan["status"].as_str() == Some("active")
            && !number(&plan["ends_at"]).is_some_and(|end| end > 0.0 && end <= now)
    };
    let tier = plans
        .iter()
        .filter(active)
        .find(|plan| {
            plan["name"]
                .as_str()
                .is_some_and(|n| n.contains("Start Plan"))
        })
        .or_else(|| plans.iter().filter(active).next())
        .and_then(|p| p["name"].as_str())
        .map(|name| name.trim_start_matches("ZCode ").to_owned());
    let mut quota = Vec::new();
    let mut remaining = HashMap::<String, f64>::new();
    for bucket in snapshot.value["balances"].as_array().into_iter().flatten() {
        if number(&bucket["expires_at"]).is_some_and(|end| end > 0.0 && end <= now) {
            continue;
        }
        let owner = plans.iter().find(|plan| {
            if let Some(id) = bucket["user_plan_id"].as_str() {
                plan["user_plan_id"].as_str() == Some(id)
            } else {
                bucket["plan_id"]
                    .as_str()
                    .is_some_and(|id| plan["plan_id"].as_str() == Some(id))
            }
        });
        if owner.is_some_and(|p| !active(&p)) {
            continue;
        }
        let total = number(&bucket["total_units"]);
        let available =
            number(&bucket["available_units"]).or_else(|| number(&bucket["remaining_units"]));
        let share = total
            .zip(available)
            .filter(|(total, _)| *total > 0.0)
            .map(|(t, a)| (a / t).clamp(0.0, 1.0));
        let capabilities = bucket["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|s| s.strip_prefix("model:"))
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let bucket_models = if capabilities.is_empty() {
            bucket["show_name"]
                .as_str()
                .map(|s| vec![s.to_ascii_lowercase()])
                .unwrap_or_default()
        } else {
            capabilities
        };
        for id in bucket_models.iter().filter(|id| ids.contains(id)) {
            let value = remaining.entry(id.clone()).or_default();
            *value = value.max(share.unwrap_or(1.0));
        }
        let Some(share) = share else {
            continue;
        };
        let label = bucket["show_name"].as_str().unwrap_or("GLM");
        let plan = owner
            .and_then(|p| p["name"].as_str())
            .unwrap_or("Start Plan");
        let reset = number(&bucket["period_end"])
            .or_else(|| number(&bucket["expires_at"]))
            .and_then(|n| chrono::DateTime::from_timestamp(n as i64, 0))
            .map(|at| at.to_rfc3339());
        quota.push(Quota {
            family: format!("{label} · {plan}"),
            remaining: share,
            reset,
        });
    }
    let status = if models.is_empty() {
        AccountStatus::Error {
            message: "The account has no active ZCode plan with models available".into(),
        }
    } else if ids
        .iter()
        .all(|id| remaining.get(id).is_some_and(|n| *n <= 0.0))
    {
        AccountStatus::Limit {
            message: "The ZCode quota is used up".into(),
        }
    } else {
        AccountStatus::Work
    };
    Report {
        tier,
        models,
        quota,
        remaining,
        status,
    }
}

fn model(id: &str) -> Model {
    let current = id.starts_with("glm-5.3");
    Model {
        id: id.into(),
        name: id
            .to_ascii_uppercase()
            .replace("FLASH", "Flash")
            .replace("TURBO", "Turbo"),
        family: "glm".into(),
        efforts: if current {
            vec!["low".into(), "high".into(), "max".into()]
        } else {
            vec!["high".into(), "max".into()]
        },
        context: if current { 1_000_000 } else { 200_000 },
        output: if current { 128_000 } else { 64_000 },
        images: id.starts_with("glm-5.3-flash"),
        fast: false,
        older: !current,
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn independent_model_and_promo_buckets_do_not_block_other_models() {
        let snapshot = UsageSnapshot {
            value: json!({"plans":[{"user_plan_id":"start","name":"ZCode Start Plan","status":"active"},
            {"user_plan_id":"promo","name":"ZCODE WEEKEND BUILD","status":"active"}],"balances":[
            {"user_plan_id":"start","capabilities":["model:glm-5.3"],"show_name":"GLM-5.3","total_units":3000000,"remaining_units":0},
            {"user_plan_id":"start","capabilities":["model:glm-5.3-flash"],"show_name":"GLM-5.3-Flash","total_units":5000000,"remaining_units":0},
            {"user_plan_id":"promo","capabilities":["model:glm-5.3-flash"],"show_name":"GLM-5.3-Flash","total_units":500000000,"remaining_units":500000000}]}),
            fetched_at: chrono::Utc::now().timestamp_millis() as u64,
            stale: false,
            refresh_error: None,
        };
        let report = report(&snapshot);
        assert_eq!(report.status, AccountStatus::Work);
        assert_eq!(report.remaining["glm-5.3"], 0.0);
        assert_eq!(report.remaining["glm-5.3-flash"], 1.0);
        assert_eq!(report.quota.len(), 3);
        assert_eq!(report.tier.as_deref(), Some("Start Plan"));
        assert!(
            report
                .models
                .iter()
                .all(|m| m.efforts == ["low", "high", "max"])
        );
    }
}
