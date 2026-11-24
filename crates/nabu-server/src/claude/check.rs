use nabu_protocol::{Model, Quota};
use ncore::{
    CancelToken, Core, Endpoint, HttpRequest, InboundRequest, PassthroughBody, PassthroughOptions,
    Transport,
};
use ncore_claudesub::{ClaudeSub, Usage};
use ncore_tokio::HyperTransport;
use serde_json::Value;

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Chat models the account can use, from Anthropic's own list, with the ids that take adaptive thinking.
pub async fn models(core: &Core, endpoint: &Endpoint) -> Result<(Vec<Model>, Vec<String>), String> {
    let req = InboundRequest {
        method: "GET".into(),
        path_and_query: "/v1/models?limit=1000".into(),
        headers: Vec::new(),
        body: Default::default(),
    };
    let resp = core
        .passthrough(
            endpoint,
            req,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .map_err(|e| format!("Model list request failed: {e}"))?;
    let PassthroughBody::Full { bytes, .. } = resp.body else {
        return Err("Model list came back as a stream".into());
    };
    if resp.status >= 400 {
        return Err(format!(
            "Model list: HTTP {} {}",
            resp.status,
            String::from_utf8_lossy(&bytes)
        ));
    }
    let reply: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("Bad model list: {e}"))?;
    let list = reply["data"].as_array().into_iter().flatten();
    let adaptive = list
        .clone()
        .filter(|info| adaptive(info))
        .filter_map(|info| info["id"].as_str().map(str::to_owned))
        .collect();
    let mut models: Vec<Model> = list.filter_map(model).collect();
    crate::models::sort_and_mark_older(&mut models);
    Ok((models, adaptive))
}

fn adaptive(info: &Value) -> bool {
    info["capabilities"]["thinking"]["types"]["adaptive"]["supported"] == true
}

fn model(info: &Value) -> Option<Model> {
    let id = info["id"].as_str()?;
    let caps = &info["capabilities"];
    let supported = |path: &[&str]| path.iter().fold(caps, |v, key| &v[*key])["supported"] == true;
    Some(Model {
        id: id.to_owned(),
        name: info["display_name"].as_str().unwrap_or(id).to_owned(),
        family: "claude".into(),
        efforts: EFFORTS
            .iter()
            .filter(|e| supported(&["effort", e]))
            .map(|e| e.to_string())
            .collect(),
        context: info["max_input_tokens"].as_u64().unwrap_or(0),
        output: info["max_tokens"].as_u64().unwrap_or(0),
        images: supported(&["image_input"]),
        fast: false,
        older: false,
    })
}

/// Whether `model` takes fast mode, asked with an empty answer (`max_tokens: 0`) so nothing is generated.
/// Models without it are refused up front; `None` when the reply says neither.
pub async fn fast(core: &Core, endpoint: &Endpoint, model: &str) -> Option<bool> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 0,
        "speed": "fast",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let req = InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/messages".into(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("anthropic-beta".into(), "fast-mode-2026-02-01".into()),
        ],
        body: body.to_string().into(),
    };
    let resp = core
        .passthrough(
            endpoint,
            req,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .ok()?;
    let PassthroughBody::Full { bytes, .. } = resp.body else {
        return None;
    };
    // A model with fast mode can still be refused for the account's credits or rate, which is 429.
    match resp.status {
        200..=299 | 429 => Some(true),
        400 if String::from_utf8_lossy(&bytes)
            .contains("does not support the `speed` parameter") =>
        {
            Some(false)
        }
        _ => None,
    }
}

/// The account's email; ClaudeSub's own profile read drops it on purpose.
pub async fn email(net: &HyperTransport, token: &str) -> Option<String> {
    let req = HttpRequest {
        method: "GET".into(),
        url: "https://api.anthropic.com/api/oauth/profile".into(),
        headers: vec![
            ("authorization".into(), format!("Bearer {token}")),
            ("anthropic-beta".into(), "oauth-2025-04-20".into()),
            ("accept".into(), "application/json".into()),
        ],
        body: Default::default(),
    };
    let resp = net.send(req).await.ok()?;
    if resp.status >= 400 {
        return None;
    }
    let raw = ncore_antigravity::oauth::read_body(resp).await.ok()?;
    let profile: Value = serde_json::from_slice(&raw).ok()?;
    let account = &profile["account"];
    account["email"]
        .as_str()
        .or(account["email_address"].as_str())
        .map(str::to_owned)
}

/// The five-hour and weekly windows as quota bars.
pub async fn quota(plugin: &ClaudeSub, net: &HyperTransport) -> Vec<Quota> {
    let Ok(usage) = plugin.usage(net).await else {
        return Vec::new();
    };
    windows(&usage.value)
}

fn windows(usage: &Usage) -> Vec<Quota> {
    [("5h", &usage.five_hour), ("week", &usage.seven_day)]
        .into_iter()
        .filter_map(|(family, window)| {
            let window = window.as_ref()?;
            Some(Quota {
                family: family.into(),
                remaining: window.remaining_percent()? / 100.0,
                reset: window.resets_at.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn capabilities_become_model_fields() {
        let m = model(&json!({
            "id": "claude-opus-5-5",
            "display_name": "Claude Opus 5.5",
            "max_input_tokens": 1000000,
            "max_tokens": 128000,
            "capabilities": {
                "image_input": {"supported": true},
                "effort": {"supported": true, "low": {"supported": true}, "medium": {"supported": true},
                           "high": {"supported": true}, "xhigh": {"supported": false}, "max": {"supported": true}},
            },
        }))
        .unwrap();
        assert_eq!(m.name, "Claude Opus 5.5");
        assert_eq!(m.efforts, ["low", "medium", "high", "max"]);
        assert_eq!((m.context, m.output, m.images), (1_000_000, 128_000, true));
    }
}
