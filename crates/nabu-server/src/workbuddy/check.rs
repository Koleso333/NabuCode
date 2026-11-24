use nabu_protocol::{AccountStatus, Credits, Model};
use ncore_workbuddyacc::{AccountSnapshot, Model as NativeModel};
use serde_json::Value;

pub struct Report {
    pub tier: Option<String>,
    pub models: Vec<Model>,
    pub free: Vec<String>,
    pub credits: Option<Credits>,
    pub status: AccountStatus,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            tier: None,
            models: vec![],
            free: vec![],
            credits: None,
            status: AccountStatus::Unchecked,
        }
    }
}

pub fn report(snapshot: AccountSnapshot) -> Result<Report, String> {
    let credits = balance(&snapshot.credits)?;
    let free = snapshot
        .catalog
        .available()
        .filter(|m| m.free)
        .map(|m| m.id.clone())
        .collect::<Vec<_>>();
    let mut models = snapshot.catalog.available().map(model).collect::<Vec<_>>();
    models.sort_by(crate::models::strongest_first);
    let status = if models.is_empty() {
        AccountStatus::Error {
            message: "The account has no WorkBuddy models available".into(),
        }
    } else if credits.remaining <= 0.0 && free.is_empty() {
        AccountStatus::Limit {
            message: "WorkBuddy credits are used up".into(),
        }
    } else {
        AccountStatus::Work
    };
    Ok(Report {
        tier: snapshot.credits.plan_name().map(str::to_owned),
        models,
        free,
        credits: Some(credits),
        status,
    })
}

pub fn balance(credits: &ncore_workbuddyacc::Credits) -> Result<Credits, String> {
    let total = credits
        .packages()
        .iter()
        .try_fold(0.0, |sum, p| {
            number(&p["CycleTotalCapacity"]).map(|n| sum + n)
        })
        .filter(|n| n.is_finite())
        .ok_or("WorkBuddy didn't return the total credits")?;
    let remaining = credits
        .remaining()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or("WorkBuddy didn't return the credits left")?;
    Ok(Credits {
        remaining,
        total: total.max(remaining),
        reset: None,
    })
}

pub use crate::quota::combine_credits as combine;

fn model(model: &NativeModel) -> Model {
    let raw = &model.raw;
    let family = if model.id.starts_with("hy") {
        "hunyuan"
    } else if model.id.starts_with("max-") || model.id.starts_with("default-") {
        "auto"
    } else {
        model.id.split('-').next().unwrap_or("workbuddy")
    };
    Model {
        id: model.id.clone(),
        name: format!("{}{}", model.name, if model.free { " (Free)" } else { "" }),
        family: family.into(),
        efforts: model.reasoning.efforts.clone(),
        context: raw
            .pointer("/contextWindow/defaultLength")
            .and_then(Value::as_u64)
            .or_else(|| raw["maxInputTokens"].as_u64())
            .unwrap_or(0),
        output: raw["maxOutputTokens"].as_u64().unwrap_or(0),
        images: raw["supportsImages"] == true,
        fast: false,
        older: false,
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncore_workbuddyacc::{Catalog, Credits as NativeCredits};
    use serde_json::json;

    #[test]
    fn account_credits_add_remaining_and_capacity_instead_of_averaging() {
        let a = Credits {
            remaining: 120.0,
            total: 120.0,
            reset: None,
        };
        let b = Credits {
            remaining: 100.0,
            total: 100.0,
            reset: None,
        };
        let sum = combine([&a, &b].into_iter()).unwrap();
        assert_eq!(sum.remaining, 220.0);
        assert_eq!(sum.total, 220.0);
        let spent = Credits {
            remaining: 0.0,
            total: 120.0,
            reset: None,
        };
        let sum = combine([&spent, &b].into_iter()).unwrap();
        assert_eq!(sum.remaining, 100.0);
        assert_eq!(sum.total, 220.0);
        assert!(combine(std::iter::empty()).is_none());
    }

    #[test]
    fn report_filters_pro_models_and_keeps_free_models_at_zero_credits() {
        let credits = NativeCredits {
            raw: json!({"IsPaidUser":false,"SubscriptionPackageName":"Free","TotalCredits":"0","Packages":[{"CycleTotalCapacity":"30"},{"CycleTotalCapacity":"100"}]}),
            fetched_at: 0,
        };
        let config = json!({"models":[{"id":"hy4free","name":"Hy4","credits":"x0.00","supportsReasoning":true,"onlyReasoning":true,"reasoning":{"supportedEfforts":["high"]},"maxInputTokens":1000000,"maxOutputTokens":64000},{"id":"hy4std","name":"Hy4","credits":"x0.29"},{"id":"gpt-pro","name":"GPT Pro"}],"modelTiers":[{"enabled":true,"modelIds":["gpt-pro"],"requiredUserType":"standard"}]});
        let catalog = Catalog::from_config(config, Some(&credits), 0).unwrap();
        let report = report(AccountSnapshot { catalog, credits }).unwrap();
        assert_eq!(report.status, AccountStatus::Work);
        assert_eq!(report.models.len(), 2);
        assert!(report.models.iter().any(|m| m.name == "Hy4 (Free)"));
        assert!(report.models.iter().all(|m| m.id != "gpt-pro"));
        assert_eq!(report.credits.unwrap().total, 130.0);
    }
}
