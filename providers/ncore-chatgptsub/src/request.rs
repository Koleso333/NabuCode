use bytes::Bytes;
use serde_json::{Value, json};

/// Keeps the host's instructions, history, tools and opaque reasoning items intact.
pub fn prepare(body: &[u8]) -> Result<Bytes, String> {
    let mut value: Value =
        serde_json::from_slice(body).map_err(|_| "invalid Responses request JSON")?;
    let object = value
        .as_object_mut()
        .ok_or("Responses request must be an object")?;
    match object.get("instructions") {
        Some(Value::String(_)) => {}
        None | Some(Value::Null) => {
            object.insert("instructions".into(), json!(""));
        }
        _ => return Err("Responses instructions must be a string".into()),
    }
    if let Some(input) = object.get_mut("input") {
        if let Some(text) = input.as_str() {
            *input = json!([{"role":"user", "content":text}]);
        } else if !input.is_array() {
            return Err("Responses input must be a string or array".into());
        }
        if let Some(items) = input.as_array_mut() {
            for item in items {
                if item.get("role").and_then(Value::as_str) == Some("system") {
                    item["role"] = json!("developer");
                }
            }
        }
    }
    object.insert("store".into(), json!(false));
    object.insert("stream".into(), json!(true));
    for name in [
        "max_tokens",
        "max_output_tokens",
        "max_completion_tokens",
        "truncation",
        "background",
        "stream_options",
    ] {
        object.remove(name);
    }
    let include = object.entry("include").or_insert_with(|| json!([]));
    let include = include
        .as_array_mut()
        .ok_or("Responses include must be an array")?;
    if include.iter().any(|v| !v.is_string()) {
        return Err("Responses include entries must be strings".into());
    }
    if !include.iter().any(|v| v == "reasoning.encrypted_content") {
        include.push(json!("reasoning.encrypted_content"));
    }
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| "cannot encode Responses request".into())
}
