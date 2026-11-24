use serde_json::{Map, Value, json};

const ALLOWED: [&str; 7] = [
    "type",
    "description",
    "properties",
    "required",
    "items",
    "enum",
    "title",
];

/// Cleans a tool JSON Schema into the protobuf-friendly subset Cloud Code accepts.
pub fn clean(schema: &Value) -> Value {
    let mut s = sanitize(schema);
    enum_hints(&mut s);
    flatten_type_arrays(&mut s, None);
    strip(&mut s);
    s
}

fn reason_object(out: &mut Map<String, Value>) {
    out.insert(
        "properties".into(),
        json!({"reason": {"type": "string", "description": "Reason for calling this tool"}}),
    );
    out.insert("required".into(), json!(["reason"]));
}

// Keeps only allowed keywords; `const` becomes a one-value enum.
fn sanitize(schema: &Value) -> Value {
    let Value::Object(map) = schema else {
        let mut out = Map::new();
        out.insert("type".into(), json!("object"));
        reason_object(&mut out);
        return Value::Object(out);
    };
    let mut out = Map::new();
    for (k, v) in map {
        if k == "const" {
            out.insert("enum".into(), json!([v]));
            continue;
        }
        if !ALLOWED.contains(&k.as_str()) {
            continue;
        }
        let v = match (k.as_str(), v) {
            ("properties", Value::Object(props)) => Value::Object(
                props
                    .iter()
                    .map(|(pk, pv)| (pk.clone(), sanitize(pv)))
                    .collect(),
            ),
            ("items", Value::Array(items)) => Value::Array(items.iter().map(sanitize).collect()),
            (_, Value::Object(_)) => sanitize(v),
            _ => v.clone(),
        };
        out.insert(k.clone(), v);
    }
    let empty_type = match out.get("type") {
        None | Some(Value::Null) => true,
        Some(Value::String(t)) => t.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    };
    if empty_type {
        out.insert("type".into(), json!("object"));
    }
    let is_object = out
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("object"));
    let has_props = out
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    if is_object && !has_props {
        reason_object(&mut out);
    }
    Value::Object(out)
}

fn append_hint(map: &mut Map<String, Value>, hint: &str) {
    let desc = match map.get("description").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => format!("{d} ({hint})"),
        _ => hint.to_owned(),
    };
    map.insert("description".into(), Value::String(desc));
}

fn for_children(map: &mut Map<String, Value>, mut f: impl FnMut(Option<&str>, &mut Value)) {
    if let Some(Value::Object(props)) = map.get_mut("properties") {
        for (k, v) in props.iter_mut() {
            f(Some(k), v);
        }
    }
    match map.get_mut("items") {
        Some(Value::Array(items)) => items.iter_mut().for_each(|v| f(None, v)),
        Some(v @ Value::Object(_)) => f(None, v),
        _ => {}
    }
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn enum_hints(v: &mut Value) {
    let Value::Object(map) = v else { return };
    if let Some(Value::Array(en)) = map.get("enum")
        && (2..=10).contains(&en.len())
    {
        let list = en.iter().map(plain).collect::<Vec<_>>().join(", ");
        append_hint(map, &format!("Allowed: {list}"));
    }
    for_children(map, |_, c| enum_hints(c));
}

// `type: [a, null]` becomes `type: a` plus hints; nullable properties leave `required`.
fn flatten_type_arrays(v: &mut Value, nullable: Option<(&mut Vec<String>, &str)>) {
    let Value::Object(map) = v else { return };
    if let Some(Value::Array(types)) = map.get("type") {
        let names: Vec<String> = types
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let has_null = names.iter().any(|t| t == "null");
        let non_null: Vec<&String> = names
            .iter()
            .filter(|t| !t.is_empty() && *t != "null")
            .collect();
        let first = non_null
            .first()
            .map_or("string".to_owned(), |t| (*t).clone());
        let accepts = (non_null.len() > 1).then(|| {
            non_null
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        });
        map.insert("type".into(), Value::String(first));
        if let Some(a) = accepts {
            append_hint(map, &format!("Accepts: {a}"));
        }
        if has_null {
            append_hint(map, "nullable");
            if let Some((set, name)) = nullable {
                set.push(name.to_owned());
            }
        }
    }
    let mut child = Vec::new();
    if let Some(Value::Object(props)) = map.get_mut("properties") {
        for (k, p) in props.iter_mut() {
            flatten_type_arrays(p, Some((&mut child, k)));
        }
    }
    match map.get_mut("items") {
        Some(Value::Array(items)) => items.iter_mut().for_each(|i| flatten_type_arrays(i, None)),
        Some(i @ Value::Object(_)) => flatten_type_arrays(i, None),
        _ => {}
    }
    if !child.is_empty()
        && let Some(Value::Array(req)) = map.get_mut("required")
    {
        req.retain(|r| r.as_str().is_none_or(|r| !child.iter().any(|c| c == r)));
        if req.is_empty() {
            map.remove("required");
        }
    }
}

// Drops `title`, prunes `required` to defined properties, upper-cases types.
fn strip(v: &mut Value) {
    let Value::Object(map) = v else { return };
    map.remove("title");
    for_children(map, |_, c| strip(c));
    let defined: Option<Vec<String>> = map
        .get("properties")
        .and_then(Value::as_object)
        .filter(|p| !p.is_empty())
        .map(|p| p.keys().cloned().collect());
    if let (Some(defined), Some(Value::Array(req))) = (defined, map.get_mut("required")) {
        req.retain(|r| r.as_str().is_some_and(|r| defined.iter().any(|d| d == r)));
        if req.is_empty() {
            map.remove("required");
        }
    }
    if let Some(Value::String(t)) = map.get_mut("type") {
        *t = match t.to_ascii_lowercase().as_str() {
            "null" => "STRING".into(),
            other => other.to_ascii_uppercase(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_style_schema() {
        let s = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "minLength": 1, "title": "Path"},
                "mode": {"type": ["string", "null"], "enum": ["a", "b"]},
                "opts": {"anyOf": [{"type": "object"}, {"type": "null"}]},
                "tags": {"type": "array", "items": {"type": "integer"}}
            },
            "required": ["path", "mode", "missing"]
        });
        assert_eq!(
            clean(&s),
            json!({
                "type": "OBJECT",
                "properties": {
                    "path": {"type": "STRING"},
                    "mode": {"type": "STRING", "enum": ["a", "b"], "description": "Allowed: a, b (nullable)"},
                    "opts": {
                        "type": "OBJECT",
                        "properties": {"reason": {"type": "STRING", "description": "Reason for calling this tool"}},
                        "required": ["reason"]
                    },
                    "tags": {"type": "ARRAY", "items": {"type": "INTEGER"}}
                },
                "required": ["path"]
            })
        );
    }

    #[test]
    fn non_object_becomes_reason_tool() {
        let out = clean(&json!(true));
        assert_eq!(out["type"], "OBJECT");
        assert_eq!(out["required"], json!(["reason"]));
    }
}
