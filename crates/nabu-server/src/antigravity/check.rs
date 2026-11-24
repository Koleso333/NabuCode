use std::sync::Arc;

use nabu_protocol::{AccountStatus, Model, Quota};
use ncore::core::ProviderError;
use ncore::{
    CancelToken, Core, Format, HttpRequest, InboundRequest, PassthroughBody, PassthroughOptions,
    Transport,
};
use ncore_antigravity::{Account, Antigravity, consts, oauth, quota};
use ncore_tokio::HyperTransport;
use serde_json::{Value, json};

use super::models;

// Cheapest models first; a check only needs one short answer.
const PROBE_MODELS: [&str; 3] = [
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-3-flash",
];

pub struct Report {
    pub tier: Option<String>,
    pub models: Vec<Model>,
    pub quota: Vec<Quota>,
    pub status: AccountStatus,
}

/// Token, tier and model list; with `probe` also one real request through the account.
pub async fn check(net: &Arc<HyperTransport>, account: &Account, probe: bool) -> Report {
    let mut report = Report {
        tier: None,
        models: Vec::new(),
        quota: Vec::new(),
        status: AccountStatus::Unchecked,
    };
    let token = match oauth::refresh(
        oauth::Sender::Transport(&**net),
        account.oauth_refresh_token(),
    )
    .await
    {
        Ok(t) => t.token,
        Err(e) => {
            report.status = AccountStatus::Error {
                message: format!("Token refresh failed: {e}"),
            };
            return report;
        }
    };
    match quota::load_tier(&**net, &token, account.project().as_deref()).await {
        Ok((tier, _)) => report.tier = Some(tier),
        Err(e) => {
            report.status = AccountStatus::Error {
                message: format!("Could not read the plan: {e}"),
            };
            return report;
        }
    }
    match fetch_models(net, &token, account).await {
        Ok(reply) => {
            report.models = models::chat_models(&reply);
            report.quota = models::quotas(&reply);
        }
        Err(message) => {
            report.status = AccountStatus::Error { message };
            return report;
        }
    }
    if probe {
        report.status = probe_account(net, account, &report.models).await;
    }
    report
}

/// Only the quota: a fresh token and the model list it comes with, without the plan or a probe.
pub async fn quota(net: &HyperTransport, account: &Account) -> Result<Vec<Quota>, String> {
    let token = oauth::refresh(oauth::Sender::Transport(net), account.oauth_refresh_token())
        .await
        .map_err(|e| format!("Token refresh failed: {e}"))?
        .token;
    Ok(models::quotas(&fetch_models(net, &token, account).await?))
}

async fn fetch_models(
    net: &HyperTransport,
    token: &str,
    account: &Account,
) -> Result<Value, String> {
    let mut headers = consts::client_headers();
    headers.push(("authorization".into(), format!("Bearer {token}")));
    let body = json!({ "project": account.project() }).to_string();
    let mut last = String::new();
    for base in ncore_antigravity::Config::default().endpoints {
        let req = HttpRequest {
            method: "POST".into(),
            url: format!("{base}/v1internal:fetchAvailableModels"),
            headers: headers.clone(),
            body: body.clone().into(),
        };
        let resp = net
            .send(req)
            .await
            .map_err(|e| format!("Model list request failed: {e}"))?;
        let status = resp.status;
        let raw = oauth::read_body(resp).await.map_err(|e| e.to_string())?;
        if status < 400 {
            return serde_json::from_slice(&raw).map_err(|e| format!("Bad model list: {e}"));
        }
        last = format!(
            "Model list: HTTP {status} {}",
            String::from_utf8_lossy(&raw)
        );
    }
    Err(last)
}

async fn probe_account(
    net: &Arc<HyperTransport>,
    account: &Account,
    models: &[Model],
) -> AccountStatus {
    let Some(model) = PROBE_MODELS
        .iter()
        .find(|p| models.iter().any(|m| m.id == **p))
        .map(|p| p.to_string())
        .or_else(|| {
            models
                .iter()
                .find(|m| m.family == "gemini")
                .map(|m| m.id.clone())
        })
    else {
        return AccountStatus::Error {
            message: "The account lists no Gemini model to test with".into(),
        };
    };

    let core = Core::builder(net.clone()).build();
    let endpoint =
        Antigravity::endpoint().with_plugin(Arc::new(Antigravity::new(vec![account.clone()])));
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {"maxOutputTokens": 64},
    });
    let req = InboundRequest {
        method: "POST".into(),
        path_and_query: format!("/v1beta/models/{model}:generateContent"),
        headers: vec![("content-type".into(), "application/json".into())],
        body: body.to_string().into(),
    };
    let resp = match core
        .passthrough(
            &endpoint,
            req,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return AccountStatus::Error {
                message: e.to_string(),
            };
        }
    };
    if resp.status < 400 {
        return AccountStatus::Work;
    }
    let bytes = match resp.body {
        PassthroughBody::Full { bytes, .. } => bytes,
        PassthroughBody::Stream(_) => Default::default(),
    };
    let error = ProviderError::parse(Format::Gemini, Some(resp.status), bytes);
    let message = format!("HTTP {}: {}", resp.status, error.message.trim());
    if resp.status == 429 {
        AccountStatus::Limit { message }
    } else {
        AccountStatus::Error { message }
    }
}
