use bytes::Bytes;
use ncore::rt::header;
use ncore::{Endpoint, Format};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Model;

const CACHE_HEADER: &str = "x-deepai-cache-key";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub affinity: String,
    pub cache_key: String,
}

impl Session {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            affinity: random_id()?,
            cache_key: random_id()?,
        })
    }

    pub fn from_ids(
        affinity: impl Into<String>,
        cache_key: impl Into<String>,
    ) -> Result<Self, String> {
        let session = Self {
            affinity: affinity.into(),
            cache_key: cache_key.into(),
        };
        session.validate()?;
        Ok(session)
    }

    pub fn attach(&self, endpoint: Endpoint) -> Result<Endpoint, String> {
        self.validate()?;
        Ok(endpoint
            .with_header("x-session-affinity", &self.affinity)
            .with_header(CACHE_HEADER, &self.cache_key))
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.affinity) || !valid_id(&self.cache_key) {
            return Err("invalid DeepAI session identity".into());
        }
        Ok(())
    }
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| "random source unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

pub(crate) fn session(body: &mut Value, headers: &[(String, String)]) -> Result<String, String> {
    let affinity = header(headers, "x-session-affinity");
    let supplied = header(headers, CACHE_HEADER);
    let cache = body
        .get("prompt_cache_key")
        .map(|v| v.as_str().ok_or("invalid prompt cache key"))
        .transpose()?;
    for value in [affinity, supplied, cache].into_iter().flatten() {
        if !valid_id(value) {
            return Err("invalid DeepAI session identity".into());
        }
    }
    if supplied.is_some() && cache.is_some() && supplied != cache {
        return Err("conflicting DeepAI cache keys".into());
    }
    let cache = match cache.or(supplied).or(affinity) {
        Some(cache) => cache.to_owned(),
        None => random_id()?,
    };
    let affinity = affinity.map(str::to_owned).unwrap_or_else(|| cache.clone());
    body["prompt_cache_key"] = cache.into();
    Ok(affinity)
}

pub(crate) fn prepare(bytes: &Bytes, format: Format) -> Result<Value, String> {
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("DeepAI request exceeds size limit".into());
    }
    let mut body: Value =
        serde_json::from_slice(bytes).map_err(|_| "invalid DeepAI request JSON")?;
    let object = body.as_object().ok_or("DeepAI request must be an object")?;
    if object
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(|s| s.is_empty())
    {
        return Err("missing DeepAI model".into());
    }
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err("DeepAI stream must be boolean".into());
    }
    match format {
        Format::OpenAiChat => {
            let messages = body["messages"]
                .as_array_mut()
                .ok_or("missing chat messages")?;
            if messages.is_empty() {
                return Err("empty chat messages".into());
            }
            for message in messages {
                if !message.is_object() {
                    return Err("invalid chat message".into());
                }
                if message["role"] == "developer" {
                    message["role"] = "system".into();
                }
            }
            if body.get("stream_options").is_some_and(|v| !v.is_object()) {
                return Err("invalid stream options".into());
            }
            if body.get("stream_options").is_none() {
                body["stream_options"] = json!({});
            }
            body["stream_options"]["include_usage"] = true.into();
        }
        Format::OpenAiResponses => {
            if !body["input"].is_array() && !body["input"].is_string() {
                return Err("missing Responses input".into());
            }
            if body.get("include").is_none() {
                body["include"] = json!([]);
            }
            let include = body["include"]
                .as_array_mut()
                .ok_or("invalid Responses include")?;
            if include.iter().any(|v| !v.is_string()) {
                return Err("invalid Responses include".into());
            }
            if !include.iter().any(|v| v == "reasoning.encrypted_content") {
                include.push("reasoning.encrypted_content".into());
            }
        }
        _ => return Err("DeepAIAcc requires OpenAI Chat or Responses".into()),
    }
    body["store"] = false.into();
    body["stream"] = true.into();
    Ok(body)
}

pub(crate) fn validate_model(
    body: &mut Value,
    model: &Model,
    format: Format,
) -> Result<(), String> {
    let expected = match model.api {
        crate::ModelApi::Chat => Format::OpenAiChat,
        crate::ModelApi::Responses => Format::OpenAiResponses,
    };
    if format != expected {
        return Err(
            "DeepAI model requires a different native endpoint; consult the catalog".into(),
        );
    }
    let fields = ["max_tokens", "max_completion_tokens", "max_output_tokens"];
    let mut output_limit = None;
    for field in fields {
        if let Some(limit) = body.get(field) {
            let count = limit.as_u64().ok_or("invalid output token limit")?;
            if count == 0 || count > model.max_tokens {
                return Err("output token limit exceeds model capabilities".into());
            }
            if output_limit.is_some_and(|limit| limit != count) {
                return Err("conflicting output token limits".into());
            }
            output_limit = Some(count);
        }
    }
    if let Some(count) = output_limit {
        let object = body.as_object_mut().unwrap();
        for field in fields {
            object.remove(field);
        }
        let field = if format == Format::OpenAiChat {
            "max_completion_tokens"
        } else {
            "max_output_tokens"
        };
        object.insert(field.into(), count.into());
    }
    reasoning(body, model, format)
}

fn reasoning(body: &mut Value, model: &Model, format: Format) -> Result<(), String> {
    if body.get("reasoning").is_some_and(|v| !v.is_object()) {
        return Err("reasoning must be an object".into());
    }
    let flat = body.get("reasoning_effort");
    let nested = body.pointer("/reasoning/effort");
    if flat.is_some() && nested.is_some() && flat != nested {
        return Err("conflicting reasoning efforts".into());
    }
    let Some(effort) = flat.or(nested) else {
        return Ok(());
    };
    let effort = effort.as_str().ok_or("reasoning effort must be a string")?;
    let effort = if effort == "off" { "none" } else { effort }.to_owned();
    if !model.reasoning.accepts_effort(&effort) {
        return Err("unsupported reasoning effort; consult DeepAI model capabilities".into());
    }
    match format {
        Format::OpenAiChat => {
            if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
                reasoning.remove("effort");
            }
            if body
                .get("reasoning")
                .and_then(Value::as_object)
                .is_some_and(|v| v.is_empty())
            {
                body.as_object_mut().unwrap().remove("reasoning");
            }
            body["reasoning_effort"] = effort.into();
        }
        Format::OpenAiResponses => {
            body.as_object_mut().unwrap().remove("reasoning_effort");
            if body.get("reasoning").is_none() {
                body["reasoning"] = json!({});
            }
            if !body["reasoning"].is_object() {
                return Err("invalid Responses reasoning".into());
            }
            body["reasoning"]["effort"] = effort.into();
        }
        _ => unreachable!(),
    }
    Ok(())
}
