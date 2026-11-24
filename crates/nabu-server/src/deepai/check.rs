use chrono::{DateTime, SecondsFormat, Utc};
use nabu_protocol::{AccountStatus, Credits, Model};
use ncore_deepaiacc::{AccountSnapshot, Allowance, Amount, Model as NativeModel, ModelApi};
use std::collections::HashMap;

pub struct Report {
    pub tier: Option<String>,
    pub models: Vec<Model>,
    pub free: Vec<String>,
    pub apis: HashMap<String, ModelApi>,
    pub credits: Option<Credits>,
    pub max_credits: Option<f64>,
    pub enabled: bool,
    pub paid: bool,
    pub status: AccountStatus,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            tier: None,
            models: vec![],
            free: vec![],
            apis: HashMap::new(),
            credits: None,
            max_credits: None,
            enabled: false,
            paid: false,
            status: AccountStatus::Unchecked,
        }
    }
}

pub fn error(error: ncore::Error) -> String {
    match error {
        ncore::Error::Provider(error) => {
            let status = error
                .status
                .map(|status| format!("HTTP {status}"))
                .unwrap_or_else(|| "API".into());
            let code = error.code.as_deref().unwrap_or("request_failed");
            format!("DeepAI — {status}, {code}: {}", error.message)
        }
        ncore::Error::Plugin { name, message } if name == "deepaiacc" => {
            format!("DeepAI — {message}")
        }
        ncore::Error::Cancelled => "The DeepAI check was cancelled".into(),
        _ => "The request to DeepAI failed".into(),
    }
}

pub fn report(snapshot: AccountSnapshot) -> Result<Report, String> {
    let mut report = Report {
        free: snapshot
            .catalog
            .models
            .iter()
            .filter(|model| is_free(model))
            .map(|model| model.id.clone())
            .collect(),
        apis: snapshot
            .catalog
            .models
            .iter()
            .map(|model| (model.id.clone(), model.api))
            .collect(),
        models: snapshot.catalog.models.iter().map(model).collect(),
        ..Default::default()
    };
    report.models.sort_by(crate::models::strongest_first);
    refresh(&mut report, &snapshot.allowance)?;
    Ok(report)
}

pub fn refresh(report: &mut Report, allowance: &Allowance) -> Result<(), String> {
    let credits = balance(allowance)?;
    report.enabled = allowance.enabled;
    report.paid = allowance
        .balance
        .as_ref()
        .is_some_and(|amount| !is_zero(amount));
    report.credits = credits;
    report.max_credits = allowance.granted.as_ref().and_then(Amount::as_f64);
    report.status = status(report);
    Ok(())
}

pub fn status(report: &Report) -> AccountStatus {
    if !report.enabled {
        AccountStatus::Limit {
            message: "Access to Theos DeepAI is turned off".into(),
        }
    } else if report.models.is_empty() {
        AccountStatus::Error {
            message: "The account has no DeepAI models available".into(),
        }
    } else if !report.paid && report.free.is_empty() {
        AccountStatus::Limit {
            message: "DeepAI credits are used up".into(),
        }
    } else {
        AccountStatus::Work
    }
}

pub fn balance(allowance: &Allowance) -> Result<Option<Credits>, String> {
    if !allowance.enabled && allowance.balance.is_none() && allowance.granted.is_none() {
        return Ok(None);
    }
    let remaining = allowance
        .balance
        .as_ref()
        .and_then(Amount::as_f64)
        .ok_or("DeepAI didn't return the credits left")?;
    let total = allowance
        .granted
        .as_ref()
        .and_then(Amount::as_f64)
        .ok_or("DeepAI didn't return the total credits")?;
    Ok(Some(Credits {
        remaining,
        total: total.max(remaining),
        reset: allowance
            .refresh_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|time| {
                time.with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Millis, true)
            }),
    }))
}

fn is_zero(amount: &Amount) -> bool {
    amount
        .as_str()
        .bytes()
        .all(|byte| matches!(byte, b'0' | b'.'))
}

fn is_free(model: &NativeModel) -> bool {
    model.theos_per_request.as_ref().is_some_and(is_zero)
}

fn model(model: &NativeModel) -> Model {
    let mut efforts = vec![];
    for effort in &model.reasoning.efforts {
        let effort = if effort == "none" { "off" } else { effort };
        if !efforts.iter().any(|known| known == effort) {
            efforts.push(effort.to_owned());
        }
    }
    Model {
        id: model.id.clone(),
        name: format!(
            "{}{}",
            model.name,
            if is_free(model) { " (Free)" } else { "" }
        ),
        family: model.id.split('-').next().unwrap_or("deepai").into(),
        efforts,
        context: model.context_window,
        output: model.max_tokens,
        images: model.supports_images(),
        fast: false,
        older: false,
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
