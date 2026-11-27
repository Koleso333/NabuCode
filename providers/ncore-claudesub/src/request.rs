use bytes::Bytes;
use ncore::core::json::{self, JsonPath, Patch};
use serde_json::value::RawValue;

pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Adds one separate first system block. Everything outside `system`, including schemas,
/// tool input ordering, cache marks and signed history, stays byte-for-byte intact.
pub fn prepare(body: &[u8]) -> Result<Bytes, String> {
    let path = JsonPath::parse("/system").unwrap();
    let span = json::find(body, &path).map_err(|e| e.to_string())?;
    let identity = serde_json::json!({"type": "text", "text": IDENTITY});
    let mut blocks = Vec::<Box<RawValue>>::new();
    if let Some(span) = span {
        let raw = &body[span.start..span.end];
        match raw.first() {
            Some(b'[') => {
                blocks = serde_json::from_slice(raw).map_err(|_| "invalid system blocks")?;
                if let Some(first) = blocks.first() {
                    let v: serde_json::Value = serde_json::from_str(first.get())
                        .map_err(|_| "invalid first system block")?;
                    if v.get("type").and_then(|v| v.as_str()) == Some("text")
                        && v.get("text").and_then(|v| v.as_str()) == Some(IDENTITY)
                    {
                        return Ok(Bytes::copy_from_slice(body));
                    }
                }
            }
            Some(b'"') => {
                let text: String =
                    serde_json::from_slice(raw).map_err(|_| "invalid system text")?;
                // A string still needs conversion to a separate identity block.
                if text != IDENTITY {
                    blocks.push(
                        RawValue::from_string(
                            serde_json::json!({"type": "text", "text": text}).to_string(),
                        )
                        .unwrap(),
                    );
                }
            }
            _ => return Err("system must be a string or array".into()),
        }
    }
    blocks.insert(0, RawValue::from_string(identity.to_string()).unwrap());
    let patch = Patch::set("/system", blocks).map_err(|e| e.to_string())?;
    json::apply(body, &[patch])
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}
