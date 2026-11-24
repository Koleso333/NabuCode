use crate::api;
use bytes::Bytes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) fn has_reasoning_options(body: &Value) -> bool {
    body.get("reasoning_effort").is_some() || body.pointer("/reasoning/effort").is_some()
}

pub(crate) fn prepare_reasoning(body: &mut Value, configuration: &Value) -> Result<(), String> {
    let flat = body.get("reasoning_effort");
    let nested = body.pointer("/reasoning/effort");
    if flat.is_some() && nested.is_some() && flat != nested {
        return Err("conflicting reasoning efforts".into());
    }
    let effort = flat
        .or(nested)
        .and_then(Value::as_str)
        .ok_or("reasoning effort must be a string")?
        .to_owned();
    let id = body["model"].as_str().ok_or("missing model")?;
    let model = configuration["models"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["id"] == id))
        .ok_or("model has no verified WorkBuddy reasoning capabilities")?;
    let caps = crate::catalog::Reasoning::from_model(model);
    let off = matches!(effort.as_str(), "none" | "off");
    if off && !caps.can_disable {
        return Err(format!(
            "reasoning cannot be disabled for WorkBuddy model {id}"
        ));
    }
    if !off && (!caps.supported || !caps.efforts.contains(&effort)) {
        return Err(format!(
            "unsupported reasoning effort {effort} for {id}; supported: {}",
            caps.efforts.join(", ")
        ));
    }
    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("effort");
        if let Some(summary) = reasoning.remove("summary")
            && !off
            && body.get("reasoning_summary").is_none()
        {
            body["reasoning_summary"] = summary;
        }
    }
    if body
        .get("reasoning")
        .and_then(Value::as_object)
        .is_some_and(|r| r.is_empty())
    {
        body.as_object_mut().unwrap().remove("reasoning");
    }
    if off {
        body.as_object_mut().unwrap().remove("reasoning_effort");
        body.as_object_mut().unwrap().remove("reasoning_summary");
    } else {
        body["reasoning_effort"] = effort.into();
        if body.get("reasoning_summary").is_none() {
            body["reasoning_summary"] = "auto".into();
        }
    }
    Ok(())
}

pub(crate) fn prepare(body: &Bytes) -> Result<Bytes, String> {
    let mut v: Value = serde_json::from_slice(body).map_err(|_| "invalid chat JSON")?;
    let messages = v["messages"]
        .as_array_mut()
        .ok_or("missing chat messages")?;
    if messages.is_empty() {
        return Err("empty chat messages".into());
    }
    for m in messages.iter_mut() {
        if m["role"] == "developer" {
            m["role"] = "system".into();
        }
    }
    if messages[0]["role"] != "system" {
        messages.insert(0, json!({"role":"system","content":""}));
    }
    if v["tool_choice"].is_object() {
        let choice = &v["tool_choice"];
        if choice["type"] != "function" {
            return Err("unsupported WorkBuddy tool choice object".into());
        }
        let name = choice["function"]["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("missing selected tool name")?
            .to_owned();
        if !v["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == name))
        {
            return Err("selected tool is not defined".into());
        }
        v["tool_choice"] = name.into();
    }
    Ok(Bytes::from(v.to_string()))
}

pub(crate) fn correlation(
    body: &[u8],
    salt: &[u8],
) -> Result<Vec<(String, String)>, ncore::Error> {
    let v: Value = serde_json::from_slice(body).map_err(|_| api::fail("invalid chat JSON"))?;
    let messages = v["messages"]
        .as_array()
        .ok_or_else(|| api::fail("missing messages"))?;
    let first = messages
        .iter()
        .find(|m| m["role"] == "user")
        .cloned()
        .unwrap_or(Value::Null);
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .cloned()
        .unwrap_or(Value::Null);
    let digest = |label: &str, value: &Value| {
        let mut h = Sha256::new();
        h.update(salt);
        h.update(label.as_bytes());
        h.update(value.to_string().as_bytes());
        h.finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| api::fail("random source unavailable"))?;
    let message = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(vec![
        ("x-conversation-id".into(), digest("conversation", &first)),
        (
            "x-conversation-request-id".into(),
            digest("turn", &json!([first, last])),
        ),
        ("x-conversation-message-id".into(), message.clone()),
        ("x-request-id".into(), message),
    ])
}

pub(crate) fn normalize_error(bytes: &mut Bytes) {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    if v.get("error").is_some() || v["code"].as_i64().is_none_or(|n| n == 0) {
        return;
    }
    let code = v["code"].to_string();
    *bytes=Bytes::from(json!({"error":{"code":code,"message":v["displayMsg"]["en"].as_str().or_else(||v["msg"].as_str()).unwrap_or("WorkBuddy request failed"),"details":v}}).to_string());
}

pub(crate) fn normalize_event(bytes: &mut Bytes) {
    normalize_error(bytes);
    let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    let mut changed = false;
    for choice in value["choices"].as_array_mut().into_iter().flatten() {
        if let Some(choice) = choice.as_object_mut()
            && choice
                .get("finish_reason")
                .is_some_and(|v| v.as_str() == Some(""))
        {
            choice.remove("finish_reason");
            changed = true;
        }
    }
    if let Some(usage) = value.get_mut("usage") {
        changed |= crate::usage::normalize(usage);
    }
    if changed {
        *bytes = Bytes::from(value.to_string());
    }
}
