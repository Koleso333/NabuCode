use ncore::{Error, HttpRequest, Transport};
use serde_json::{Value, json};

use crate::account::Account;
use crate::consts::{
    ENDPOINT_DAILY, ENDPOINT_PROD, Family, client_headers, client_metadata, family,
};
use crate::oauth::{self, Sender};

/// Remaining quota of one model on one account.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelQuota {
    pub model: String,
    /// 0.0..=1.0; `None` when the backend did not say.
    pub remaining: Option<f64>,
    pub reset_time: Option<String>,
}

/// Quota report for one account, as ReTools showed it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountQuota {
    pub email: String,
    /// `ok`, `disabled` or `error`.
    pub status: String,
    /// `free`, `pro`, `ultra` or `unknown`.
    pub tier: String,
    pub project_id: Option<String>,
    pub models: Vec<ModelQuota>,
    pub error: Option<String>,
}

/// Only `paidTier` marks a subscription; `standard-tier` is the base tier every account gets.
fn parse_tier(v: &Value) -> &'static str {
    let Some(paid) = v["paidTier"]["id"].as_str() else {
        return "free";
    };
    let low = paid.to_ascii_lowercase();
    if low.contains("ultra") {
        "ultra"
    } else if low.contains("free") {
        "free"
    } else {
        "pro"
    }
}

/// Tier and project reported by `loadCodeAssist`.
pub async fn load_tier(
    transport: &dyn Transport,
    token: &str,
    project_hint: Option<&str>,
) -> Result<(String, Option<String>), Error> {
    let sender = Sender::Transport(transport);
    let mut metadata = client_metadata();
    if let Some(p) = project_hint {
        metadata["duetProject"] = json!(p);
    }
    let mut last = None;
    for ep in [ENDPOINT_PROD, ENDPOINT_DAILY] {
        let mut headers = client_headers();
        headers.push(("authorization".into(), format!("Bearer {token}")));
        let req = HttpRequest {
            method: "POST".into(),
            url: format!("{ep}/v1internal:loadCodeAssist"),
            headers,
            body: json!({"metadata": metadata, "mode": 1}).to_string().into(),
        };
        let resp = sender.send(req).await?;
        let status = resp.status;
        let raw = oauth::read_body(resp).await?;
        if status >= 400 {
            last = Some(format!("{ep}: {status} {}", String::from_utf8_lossy(&raw)));
            continue;
        }
        let v: Value = serde_json::from_slice(&raw).map_err(|e| crate::fail(e.to_string()))?;
        let project = match &v["cloudaicompanionProject"] {
            Value::String(p) => Some(p.clone()),
            o => o["id"].as_str().map(str::to_owned),
        };
        return Ok((parse_tier(&v).into(), project));
    }
    Err(crate::fail(
        last.unwrap_or_else(|| "loadCodeAssist failed".into()),
    ))
}

/// Refreshes the token, reads the tier and lists model quotas; errors land in the report.
pub async fn check_account(
    transport: &dyn Transport,
    account: &Account,
    endpoints: &[String],
) -> AccountQuota {
    let mut out = AccountQuota {
        email: account.email.clone(),
        status: "ok".into(),
        tier: "unknown".into(),
        project_id: None,
        models: Vec::new(),
        error: None,
    };
    if !account.enabled {
        out.status = "disabled".into();
        return out;
    }
    let res = async {
        let token = oauth::refresh(Sender::Transport(transport), account.oauth_refresh_token())
            .await?
            .token;
        let plain = account
            .refresh_token
            .split('|')
            .nth(1)
            .filter(|s| !s.is_empty());
        let hint = plain.map(str::to_owned).or(account.project_id.clone());
        let (tier, found) = load_tier(transport, &token, hint.as_deref()).await?;
        let project = found.or_else(|| account.project());
        let models = models_with_token(transport, &token, project.as_deref(), endpoints).await?;
        Ok::<_, Error>((tier, project, models))
    }
    .await;
    match res {
        Ok((tier, project, models)) => {
            out.tier = tier;
            out.project_id = project;
            out.models = models;
        }
        Err(e) => {
            out.status = "error".into();
            out.error = Some(e.to_string());
        }
    }
    out
}

/// Lists the models an account can use, with their quotas.
pub async fn fetch_models(
    transport: &dyn Transport,
    account: &Account,
    endpoints: &[String],
) -> Result<Vec<ModelQuota>, Error> {
    let sender = Sender::Transport(transport);
    let token = oauth::refresh(sender, account.oauth_refresh_token())
        .await?
        .token;
    models_with_token(transport, &token, account.project().as_deref(), endpoints).await
}

async fn models_with_token(
    transport: &dyn Transport,
    token: &str,
    project: Option<&str>,
    endpoints: &[String],
) -> Result<Vec<ModelQuota>, Error> {
    let sender = Sender::Transport(transport);
    let body = match project {
        Some(p) => json!({"project": p}),
        None => json!({}),
    };
    let mut last = None;
    for ep in endpoints {
        let mut headers = client_headers();
        headers.push(("authorization".into(), format!("Bearer {token}")));
        let req = HttpRequest {
            method: "POST".into(),
            url: format!(
                "{}/v1internal:fetchAvailableModels",
                ep.trim_end_matches('/')
            ),
            headers,
            body: body.to_string().into(),
        };
        let resp = sender.send(req).await?;
        let status = resp.status;
        let raw = oauth::read_body(resp).await?;
        if status >= 400 {
            last = Some(format!("{ep}: {status} {}", String::from_utf8_lossy(&raw)));
            continue;
        }
        let v: Value = serde_json::from_slice(&raw).map_err(|e| crate::fail(e.to_string()))?;
        let mut out: Vec<ModelQuota> = v["models"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(id, _)| family(id) != Family::Unknown)
            .filter_map(|(id, m)| {
                let q = m.get("quotaInfo")?.as_object()?;
                let reset = q
                    .get("resetTime")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let remaining = q
                    .get("remainingFraction")
                    .and_then(Value::as_f64)
                    .or(reset.as_ref().map(|_| 0.0));
                Some(ModelQuota {
                    model: id.clone(),
                    remaining,
                    reset_time: reset,
                })
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model));
        return Ok(out);
    }
    Err(crate::fail(
        last.unwrap_or_else(|| "fetchAvailableModels failed".into()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_comes_from_paid_tier_only() {
        let free = json!({"allowedTiers":[{"id":"standard-tier","isDefault":true}]});
        assert_eq!(parse_tier(&free), "free");
        let pro = json!({
            "paidTier":{"id":"g1-pro-tier"},
            "currentTier":{"id":"standard-tier"},
            "allowedTiers":[{"id":"standard-tier","isDefault":true}]
        });
        assert_eq!(parse_tier(&pro), "pro");
        assert_eq!(
            parse_tier(&json!({"paidTier":{"id":"g1-ultra-tier"}})),
            "ultra"
        );
    }
}
