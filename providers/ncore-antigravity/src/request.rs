use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::consts::{
    Family, GEMINI_SKIP_SIGNATURE, MIN_SIGNATURE_LEN, family, is_thinking, masked_system_text,
};
use crate::schema;

/// Model and method taken from a Gemini path like `/v1beta/models/m:streamGenerateContent?alt=sse`.
pub fn parse_gemini_url(url: &str) -> Option<(String, String)> {
    let path = url.split('?').next()?;
    let tail = &path[path.find("/models/")? + "/models/".len()..];
    let (model, method) = tail.split_once(':')?;
    Some((model.to_owned(), method.to_owned()))
}

fn obj(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("object")
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// Account-independent rewrite of a Gemini request for Cloud Code.
pub fn prepare(body: &mut Value, model: &str, gemini_cap: Option<u64>) {
    let fam = family(model);
    let root = obj(body);
    fix_contents(root, fam);
    fix_tools(root, fam);
    fix_generation(root, model, fam, gemini_cap);
    inject_system(root);
}

fn sig_ok(part: &Map<String, Value>) -> bool {
    part.get("thoughtSignature")
        .and_then(Value::as_str)
        .is_some_and(|s| s.len() >= MIN_SIGNATURE_LEN)
}

fn empty_text(p: &Map<String, Value>, fam: Family) -> bool {
    p.get("text").and_then(Value::as_str) == Some("")
        && p.get("thought").and_then(Value::as_bool) != Some(true)
        && (fam == Family::Claude || !p.contains_key("thoughtSignature"))
}

// Drops empty texts and unsigned thoughts, signs Gemini function calls, fills empty turns.
fn fix_contents(root: &mut Map<String, Value>, fam: Family) {
    let Some(Value::Array(contents)) = root.get_mut("contents") else {
        return;
    };
    for c in contents.iter_mut() {
        let Value::Object(c) = c else { continue };
        let is_model = c.get("role").and_then(Value::as_str) == Some("model");
        let parts = c.entry("parts").or_insert_with(|| json!([]));
        if let Value::Array(parts) = parts {
            parts.retain(|p| p.as_object().is_none_or(|p| !empty_text(p, fam)));
            if is_model {
                parts.retain(|p| {
                    let Value::Object(p) = p else { return true };
                    p.get("thought").and_then(Value::as_bool) != Some(true) || sig_ok(p)
                });
                // Claude has no use for Gemini's skip placeholder and rejects it.
                if fam == Family::Claude {
                    for p in parts.iter_mut() {
                        if let Value::Object(p) = p
                            && p.get("thoughtSignature").and_then(Value::as_str) == Some(GEMINI_SKIP_SIGNATURE)
                        {
                            p.remove("thoughtSignature");
                        }
                    }
                }
                if fam == Family::Gemini {
                    for p in parts.iter_mut() {
                        if let Value::Object(p) = p
                            && p.contains_key("functionCall")
                            && !p.contains_key("thoughtSignature")
                        {
                            p.insert("thoughtSignature".into(), json!(GEMINI_SKIP_SIGNATURE));
                        }
                    }
                }
            }
            if parts.is_empty() {
                parts.push(json!({"text": "."}));
            }
        }
    }
}

fn fix_tools(root: &mut Map<String, Value>, fam: Family) {
    let mut any = false;
    if let Some(Value::Array(tools)) = root.get_mut("tools") {
        for tool in tools.iter_mut() {
            let Some(Value::Array(decls)) = tool.get_mut("functionDeclarations") else {
                continue;
            };
            for (i, d) in decls.iter_mut().enumerate() {
                let d = obj(d);
                let raw = d
                    .remove("parametersJsonSchema")
                    .or_else(|| d.remove("parameters"))
                    .unwrap_or_else(|| json!({"type": "object"}));
                d.insert("parameters".into(), schema::clean(&raw));
                let name = d
                    .get("name")
                    .and_then(Value::as_str)
                    .map(sanitize_name)
                    .unwrap_or_else(|| format!("tool-{i}"));
                d.insert("name".into(), json!(name));
                d.entry("description").or_insert_with(|| json!(""));
                any = true;
            }
        }
    }
    if any && fam == Family::Claude && !root.contains_key("toolConfig") {
        root.insert(
            "toolConfig".into(),
            json!({"functionCallingConfig": {"mode": "VALIDATED"}}),
        );
    }
}

// Cloud Code wants maxOutputTokens strictly above the thinking budget.
fn fix_generation(root: &mut Map<String, Value>, model: &str, fam: Family, cap: Option<u64>) {
    let gc = obj(root.entry("generationConfig").or_insert_with(|| json!({})));
    let max = |gc: &Map<String, Value>| {
        gc.get("maxOutputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    if is_thinking(model) {
        let tc = obj(gc.entry("thinkingConfig").or_insert_with(|| json!({})));
        tc.entry("includeThoughts").or_insert(json!(true));
        if fam == Family::Claude
            && !tc.contains_key("thinkingBudget")
            && let Some(level) = tc.remove("thinkingLevel")
        {
            let budget = match level.as_str().unwrap_or("").to_ascii_uppercase().as_str() {
                "MINIMAL" | "LOW" => 1024,
                "MEDIUM" => 8192,
                _ => 24576,
            };
            tc.insert("thinkingBudget".into(), json!(budget));
        }
        if !tc.contains_key("thinkingLevel") && !tc.contains_key("thinkingBudget") {
            tc.insert(
                "thinkingBudget".into(),
                json!(if fam == Family::Gemini { 8192 } else { 24576 }),
            );
        }
        let budget = tc.get("thinkingBudget").and_then(Value::as_i64);
        if let Some(budget) = budget.filter(|b| *b >= 0) {
            let budget = if fam == Family::Gemini {
                cap.map_or(budget as u64, |cap| {
                    (budget as u64).min(cap.saturating_sub(1))
                })
            } else {
                budget as u64
            };
            gc.get_mut("thinkingConfig").expect("thinking config")["thinkingBudget"] =
                json!(budget);
            if max(gc) <= budget {
                let room = if fam == Family::Claude { 8192 } else { 1 };
                gc.insert("maxOutputTokens".into(), json!(budget.saturating_add(room)));
            }
        }
        if fam == Family::Claude && model.eq_ignore_ascii_case("claude-opus-4-6-thinking") {
            gc.insert("maxOutputTokens".into(), json!(max(gc).max(57344)));
        }
    }
    if fam == Family::Gemini
        && let Some(cap) = cap
        && max(gc) > cap
    {
        gc.insert("maxOutputTokens".into(), json!(cap));
    }
}

fn inject_system(root: &mut Map<String, Value>) {
    let mut parts = vec![json!({"text": masked_system_text()})];
    if let Some(Value::Array(existing)) = root.get("systemInstruction").and_then(|s| s.get("parts"))
    {
        parts.extend(
            existing
                .iter()
                .filter(|p| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !t.is_empty())
                })
                .cloned(),
        );
    }
    root.insert(
        "systemInstruction".into(),
        json!({"role": "user", "parts": parts}),
    );
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Stable id from the account and first user text, so Cloud Code can keep its cache warm.
pub fn session_id(body: &Value, email: &str) -> String {
    let digest = Sha256::digest(format!("{email}:{}", first_user_text(body)).as_bytes());
    hex(&digest)[..32].to_owned()
}

/// Key of a conversation: the same for every request that continues it.
pub fn conversation(body: &Value) -> String {
    hex(&Sha256::digest(first_user_text(body).as_bytes()))[..32].to_owned()
}

fn first_user_text(body: &Value) -> String {
    body.get("contents")
        .and_then(Value::as_array)
        .and_then(|cs| {
            cs.iter()
                .find(|c| c.get("role").and_then(Value::as_str).unwrap_or("user") == "user")
        })
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

pub fn uuid4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Wraps a prepared Gemini request into the Cloud Code envelope.
pub fn envelope(mut google: Value, model: &str, project: &str, session: &str) -> Value {
    obj(&mut google).insert("sessionId".into(), json!(session));
    json!({
        "project": project,
        "model": model,
        "request": google,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": format!("agent-{}", uuid4()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url() {
        assert_eq!(
            parse_gemini_url("https://x/v1beta/models/gemini-3:streamGenerateContent?alt=sse"),
            Some(("gemini-3".into(), "streamGenerateContent".into()))
        );
        assert_eq!(parse_gemini_url("https://x/v1/messages"), None);
    }

    #[test]
    fn prepares_gemini_thinking_request() {
        let mut b = json!({
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [
                    {"text": "hmm", "thought": true},
                    {"functionCall": {"name": "f", "args": {}}}
                ]}
            ],
            "tools": [{"functionDeclarations": [{"name": "my.tool", "parametersJsonSchema": {"type": "object", "properties": {"a": {"type": "string"}}}}]}],
            "generationConfig": {"maxOutputTokens": 64000}
        });
        prepare(&mut b, "gemini-3.1-pro-low", Some(16384));
        let parts = &b["contents"][1]["parts"];
        assert_eq!(parts.as_array().unwrap().len(), 1);
        assert_eq!(parts[0]["thoughtSignature"], GEMINI_SKIP_SIGNATURE);
        let d = &b["tools"][0]["functionDeclarations"][0];
        assert_eq!(d["name"], "my_tool");
        assert_eq!(d["parameters"]["properties"]["a"]["type"], "STRING");
        assert!(d.get("parametersJsonSchema").is_none());
        assert_eq!(
            b["generationConfig"],
            json!({"maxOutputTokens": 16384, "thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8192}})
        );
        let sys = &b["systemInstruction"];
        assert_eq!(sys["role"], "user");
        assert_eq!(sys["parts"][1]["text"], "be brief");
        assert!(b.get("toolConfig").is_none());
    }

    #[test]
    fn claude_gets_budget_room_and_validated_tools() {
        let mut b = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "tools": [{"functionDeclarations": [{"name": "f"}]}],
            "generationConfig": {"maxOutputTokens": 1000}
        });
        prepare(&mut b, "claude-sonnet-4-5-thinking", Some(16384));
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 24576 + 8192);
        assert_eq!(
            b["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn empty_texts_dropped_for_claude() {
        let mut b = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"text": ""}, {"functionCall": {"name": "f", "args": {}}}, {"text": ""}]}
        ]});
        prepare(&mut b, "claude-sonnet-4-6", None);
        assert_eq!(
            b["contents"][1]["parts"],
            json!([{"functionCall": {"name": "f", "args": {}}}])
        );
        let mut g = json!({"contents": [{"role": "model", "parts": [{"text": "", "thoughtSignature": "s"}]}]});
        prepare(&mut g, "gemini-3-flash", None);
        assert_eq!(g["contents"][0]["parts"][0]["thoughtSignature"], "s");
    }

    #[test]
    fn session_id_is_stable() {
        let b = json!({"contents": [{"role": "user", "parts": [{"text": "a"}, {"text": "b"}]}]});
        let s = session_id(&b, "x@y");
        assert_eq!(s.len(), 32);
        assert_eq!(s, session_id(&b, "x@y"));
        assert_ne!(s, session_id(&b, "z@y"));
        assert_eq!(uuid4().len(), 36);
    }
}

#[cfg(test)]
mod thinking_regressions {
    use super::*;

    #[test]
    fn claude_calls_lose_the_gemini_skip_signature() {
        let sig = "s".repeat(MIN_SIGNATURE_LEN);
        let mut b = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"text": "hmm", "thought": true, "thoughtSignature": sig},
                {"functionCall": {"name": "f", "args": {}}, "thoughtSignature": GEMINI_SKIP_SIGNATURE}
            ]}
        ]});
        prepare(&mut b, "claude-opus-4-6-thinking", None);
        let parts = &b["contents"][1]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], sig);
        assert!(parts[1].get("thoughtSignature").is_none());
    }

    #[test]
    fn opus_keeps_explicit_thinking_settings_and_output_room() {
        for budget in [0, 1024, 8192, -1] {
            let mut body = json!({"generationConfig":{"maxOutputTokens":4096,"stopSequences":["END"],"thinkingConfig":{"thinkingBudget":budget,"includeThoughts":false}}});
            prepare(&mut body, "claude-opus-4-6-thinking", Some(16384));
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                budget
            );
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["includeThoughts"],
                false
            );
            assert_eq!(body["generationConfig"]["maxOutputTokens"], 57344);
            assert_eq!(body["generationConfig"]["stopSequences"], json!(["END"]));
        }
    }

    #[test]
    fn level_only_requests_include_thoughts_without_budget_injection() {
        let mut body = json!({"generationConfig":{"thinkingConfig":{"thinkingLevel":"HIGH"}}});
        prepare(&mut body, "gemini-3-flash", Some(16384));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"thinkingLevel":"HIGH","includeThoughts":true})
        );
    }
}

#[cfg(test)]
mod effort_regressions {
    use super::*;

    #[test]
    fn claude_effort_uses_a_budget_while_gemini_keeps_its_level() {
        for (level, budget) in [("LOW", 1024), ("MEDIUM", 8192), ("HIGH", 24576)] {
            let mut body = json!({"generationConfig":{"thinkingConfig":{"thinkingLevel":level}}});
            prepare(&mut body, "claude-opus-4-6-thinking", None);
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                budget
            );
            assert!(
                body["generationConfig"]["thinkingConfig"]
                    .get("thinkingLevel")
                    .is_none()
            );
            assert_eq!(body["generationConfig"]["maxOutputTokens"], 57344);
        }
    }
}
