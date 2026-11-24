use serde_json::Value;

#[derive(Clone, Debug)]
pub struct RequestUsage {
    pub request_id: u64,
    pub model: Option<String>,
    pub raw: Value,
    pub samples: u32,
}

pub(crate) struct Sample {
    model: Option<String>,
    pub usage: Value,
}

pub(crate) fn sample(bytes: &[u8]) -> Option<Sample> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let response = value
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    let usage = response.get("usage").filter(|v| v.is_object())?;
    Some(Sample {
        model: response["model"].as_str().map(str::to_owned),
        usage: usage.clone(),
    })
}

pub(crate) fn merge(target: &mut Value, source: &Value) {
    if let Some(fields) = source.as_object() {
        if !target.is_object() {
            *target = Value::Object(Default::default());
        }
        for (name, value) in fields {
            if value.is_null() {
                continue;
            }
            if value.is_object() {
                merge(&mut target[name], value);
            } else {
                target[name] = value.clone();
            }
        }
    }
}

pub(crate) fn update(current: &mut Option<RequestUsage>, request_id: u64, sample: Sample) {
    if current.as_ref().is_none_or(|u| u.request_id != request_id) {
        *current = Some(RequestUsage {
            request_id,
            model: sample.model,
            raw: sample.usage,
            samples: 1,
        });
        return;
    }
    let current = current.as_mut().unwrap();
    if sample.model.is_some() {
        current.model = sample.model;
    }
    merge(&mut current.raw, &sample.usage);
    current.samples = current.samples.saturating_add(1);
}
