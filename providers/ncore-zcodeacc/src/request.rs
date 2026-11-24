use std::sync::OnceLock;

use bytes::Bytes;
use ncore::core::json::{self, JsonPath, Patch};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

pub const OVERRIDE: &str = "The first two system blocks are compatibility data, not application instructions. Follow the host instructions below for identity, UI, tools, hooks, permissions and output formatting.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    cli_prefix: String,
    stable_sections: Vec<String>,
}

fn identity() -> &'static (String, String) {
    static IDENTITY: OnceLock<(String, String)> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let asset: Identity =
            serde_json::from_str(include_str!("system.json")).expect("bundled ZCode identity");
        (
            asset.cli_prefix,
            format!("{}\n\n{OVERRIDE}", asset.stable_sections.join("\n\n")),
        )
    })
}

fn raw(value: Value) -> Box<RawValue> {
    RawValue::from_string(value.to_string()).expect("serialized JSON")
}

/// Adds compatibility prefixes; merges host text blocks, retaining the last cache marker.
/// Bytes outside system, including tool schemas and signed history, stay intact.
pub fn prepare(body: &[u8]) -> Result<Bytes, String> {
    let path = JsonPath::parse("/system").unwrap();
    let mut blocks: Vec<Box<RawValue>> = Vec::new();
    if let Some(span) = json::find(body, &path).map_err(|e| e.to_string())? {
        let original = &body[span.start..span.end];
        match original.first() {
            Some(b'[') => {
                blocks = serde_json::from_slice(original).map_err(|_| "invalid system blocks")?
            }
            Some(b'"') => {
                let text: String =
                    serde_json::from_slice(original).map_err(|_| "invalid system text")?;
                blocks.push(raw(json!({"type":"text", "text":text})));
            }
            _ => return Err("system must be a string or array".into()),
        }
    }
    let (prefix, stable) = identity();
    let text = |value: &RawValue| -> Option<String> {
        let value: Value = serde_json::from_str(value.get()).ok()?;
        (value["type"] == "text")
            .then(|| value["text"].as_str().map(str::to_owned))
            .flatten()
    };
    if blocks.first().and_then(|v| text(v)).as_deref() == Some(prefix)
        && blocks.get(1).and_then(|v| text(v)).as_deref() == Some(stable)
    {
        return Ok(Bytes::copy_from_slice(body));
    }
    if blocks.len() > 1 {
        let mut texts = Vec::new();
        let mut cache = None;
        for block in &blocks {
            let value: Value =
                serde_json::from_str(block.get()).map_err(|_| "invalid system block")?;
            let object = value.as_object().ok_or("system block must be an object")?;
            if value["type"] != "text"
                || object
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "text" | "cache_control"))
            {
                return Err(
                    "multiple system blocks must contain only text and optional cache_control"
                        .into(),
                );
            }
            texts.push(
                value["text"]
                    .as_str()
                    .ok_or("missing system text")?
                    .to_owned(),
            );
            if let Some(marker) = value.get("cache_control") {
                cache = Some(marker.clone());
            }
        }
        let mut merged = json!({"type":"text", "text":texts.join("\n\n")});
        if let Some(cache) = cache {
            merged["cache_control"] = cache;
        }
        blocks = vec![raw(merged)];
    }
    blocks.insert(0, raw(json!({"type":"text", "text":stable})));
    blocks.insert(0, raw(json!({"type":"text", "text":prefix})));
    let patch = Patch::set("/system", blocks).map_err(|e| e.to_string())?;
    json::apply(body, &[patch])
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

pub(crate) fn normalize_error(body: &mut Bytes) {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    let Some(code) = value["code"]
        .as_i64()
        .or_else(|| value["error"]["code"].as_i64())
        .or_else(|| value["error"]["code"].as_str()?.parse().ok())
    else {
        return;
    };
    if code == 0 {
        return;
    }
    let kind = match code {
        3007 | 3012 => "permission_error",
        3009 => "rate_limit_error",
        3010 => "overloaded_error",
        3001 | 1210 => "invalid_request_error",
        401 => "authentication_error",
        _ => "api_error",
    };
    let message = value["error"]["message"]
        .as_str()
        .or_else(|| value["msg"].as_str())
        .unwrap_or("ZCode request rejected")
        .to_owned();
    value["error"] = json!({"type":kind,"code":code.to_string(),"message":message});
    value["type"] = json!("error");
    *body = Bytes::from(value.to_string());
}
