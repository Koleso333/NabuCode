use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct RequestUsage {
    pub request_id: u64,
    pub model: Option<String>,
    pub raw: Value,
}

fn count(value: &Value, paths: &[&str]) -> Option<u64> {
    let values = paths
        .iter()
        .filter_map(|p| value.pointer(p).and_then(Value::as_u64))
        .collect::<Vec<_>>();
    values
        .iter()
        .copied()
        .find(|n| *n > 0)
        .or_else(|| values.first().copied())
}

fn detail(usage: &mut Value, group: &str, key: &str, value: Option<u64>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if usage[group][key].as_u64() == Some(value) {
        return false;
    }
    if usage[group].is_null() {
        usage[group] = json!({});
    }
    if let Some(group) = usage[group].as_object_mut() {
        group.insert(key.into(), value.into());
        return true;
    }
    false
}

pub(crate) fn normalize(usage: &mut Value) -> bool {
    if !usage.is_object() {
        return false;
    }
    let cache_read = count(
        usage,
        &[
            "/cache_read_input_tokens",
            "/cacheReadInputTokens",
            "/inputTokensDetails/cached_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/prompt_cache_hit_tokens",
        ],
    );
    let cache_write = count(
        usage,
        &[
            "/cache_creation_input_tokens",
            "/cacheCreationInputTokens",
            "/prompt_cache_write_tokens",
            "/prompt_tokens_details/cache_write_tokens",
        ],
    );
    let reasoning = count(
        usage,
        &[
            "/completion_tokens_details/reasoning_tokens",
            "/completion_thinking_tokens",
        ],
    );
    let mut changed = detail(usage, "prompt_tokens_details", "cached_tokens", cache_read);
    changed |= detail(
        usage,
        "prompt_tokens_details",
        "cache_write_tokens",
        cache_write,
    );
    changed |= detail(
        usage,
        "completion_tokens_details",
        "reasoning_tokens",
        reasoning,
    );
    changed
}

pub(crate) fn merge(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            if let Some(existing) = target.get_mut(key)
                && existing.is_object()
                && value.is_object()
            {
                merge(existing, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}
