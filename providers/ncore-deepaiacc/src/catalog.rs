use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::quota::Amount;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reasoning {
    pub supported: bool,
    pub efforts: Vec<String>,
    pub can_disable: bool,
}

impl Reasoning {
    fn stock(model_id: &str, supported: bool) -> Self {
        let mut efforts = Vec::new();
        if supported {
            efforts.extend(
                ["off", "none", "minimal", "low", "medium", "high"]
                    .into_iter()
                    .map(str::to_owned),
            );
            if model_id == "muse-ember" {
                efforts.push("xhigh".into());
            }
            if matches!(model_id, "deepai-cyber" | "muse-ember") {
                efforts.push("max".into());
            }
        }
        Self {
            supported,
            efforts,
            can_disable: supported,
        }
    }

    pub fn accepts_effort(&self, effort: &str) -> bool {
        self.efforts.iter().any(|supported| supported == effort)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModelApi {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub reasoning: Reasoning,
    pub input: Vec<String>,
    pub context_window: u64,
    pub max_tokens: u64,
    pub premier: bool,
    pub api: ModelApi,
    /// Typical estimates, not exact debits or an affordability guarantee.
    pub theos_per_request: Option<Amount>,
    pub theos_per_second: Option<BTreeMap<String, Amount>>,
}

impl Model {
    pub fn accepts_effort(&self, effort: &str) -> bool {
        self.reasoning.accepts_effort(effort)
    }

    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|input| input == "image")
    }

    fn from_value(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("invalid DeepAI model catalog")?;
        let text = |name| {
            object
                .get(name)
                .and_then(Value::as_str)
                .ok_or("invalid DeepAI model catalog")
        };
        let id = text("id")?;
        let name = text("name")?;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            || name.trim().is_empty()
            || name.encode_utf16().count() > 100
        {
            return Err("invalid DeepAI model catalog".into());
        }
        let supported = object
            .get("reasoning")
            .and_then(Value::as_bool)
            .ok_or("invalid DeepAI model catalog")?;
        let input = object
            .get("input")
            .and_then(Value::as_array)
            .ok_or("invalid DeepAI model catalog")?
            .iter()
            .map(|input| match input.as_str() {
                Some(input @ ("text" | "image")) => Ok(input.to_owned()),
                _ => Err("invalid DeepAI model catalog"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !input.iter().any(|input| input == "text") {
            return Err("invalid DeepAI model catalog".into());
        }
        let context_window = safe_budget(&value["contextWindow"])?;
        let max_tokens = safe_budget(&value["maxTokens"])?;
        if max_tokens > context_window {
            return Err("invalid DeepAI model catalog".into());
        }
        let premier = match object.get("premier") {
            None => false,
            Some(value) => value.as_bool().ok_or("invalid DeepAI model catalog")?,
        };
        let api = match object.get("api") {
            None => ModelApi::Chat,
            Some(Value::String(api)) if api == "openai-responses" => ModelApi::Responses,
            Some(_) => return Err("invalid DeepAI model catalog".into()),
        };
        let theos_per_request = object.get("theosPerRequest").map(price).transpose()?;
        let theos_per_second = object
            .get("theosPerSecond")
            .map(|value| {
                value
                    .as_object()
                    .ok_or("invalid DeepAI model catalog")?
                    .iter()
                    .map(|(effort, value)| {
                        if !matches!(effort.as_str(), "none" | "low" | "medium" | "high") {
                            return Err("invalid DeepAI model catalog".into());
                        }
                        Ok((effort.clone(), price(value)?))
                    })
                    .collect::<Result<BTreeMap<_, _>, String>>()
            })
            .transpose()?;
        Ok(Self {
            id: id.into(),
            name: name.into(),
            reasoning: Reasoning::stock(id, supported),
            input,
            context_window,
            max_tokens,
            premier,
            api,
            theos_per_request,
            theos_per_second,
        })
    }
}

fn price(value: &Value) -> Result<Amount, String> {
    Amount::new(value.as_str().ok_or("invalid DeepAI model catalog")?)
        .map_err(|_| "invalid DeepAI model catalog".into())
}

fn safe_budget(value: &Value) -> Result<u64, String> {
    let budget = value.as_u64().or_else(|| {
        let number = value.as_f64()?;
        (number.is_finite()
            && number > 0.0
            && number <= 9_007_199_254_740_991.0
            && number.fract() == 0.0)
            .then_some(number as u64)
    });
    budget
        .filter(|budget| (1..=9_007_199_254_740_991).contains(budget))
        .ok_or_else(|| "invalid DeepAI model catalog".into())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalog {
    pub models: Vec<Model>,
    pub fetched_at: u64,
}

impl Catalog {
    pub fn from_json(value: Value, fetched_at: u64) -> Result<Self, String> {
        let list = value["models"]
            .as_array()
            .filter(|list| (1..=100).contains(&list.len()))
            .ok_or("invalid DeepAI model catalog")?;
        let mut ids = BTreeSet::new();
        let mut models = Vec::with_capacity(list.len());
        for value in list {
            let model = Model::from_value(value)?;
            if !ids.insert(model.id.clone()) {
                return Err("invalid DeepAI model catalog".into());
            }
            models.push(model);
        }
        Ok(Self { models, fetched_at })
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }
}
