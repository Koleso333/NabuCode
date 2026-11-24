use std::path::Path;

use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::{resolve, shown, str_arg};

pub fn def() -> ToolDef {
    super::def(
        "write",
        "Writes a whole file: creates it, with missing parent folders, or replaces all of an existing file's content without asking. \
The result says which of the two happened. For changes inside an existing file use edit instead, it costs far fewer tokens. \
An existing file keeps its line endings (LF or CRLF) and BOM whatever `content` uses.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute, or relative to the working directory." },
                "content": { "type": "string", "description": "The full new content of the file." }
            },
            "required": ["path", "content"]
        }),
    )
}

pub fn run(directory: &Path, args: &Value) -> Result<String, String> {
    let path = resolve(directory, str_arg(args, "path")?);
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or("missing string argument `content`")?;
    if path.is_dir() {
        return Err(format!("{} is a folder", path.display()));
    }
    let name = shown(directory, &path);
    let lines = content.lines().count();
    if path.exists() {
        let old = TextFile::load(&path)?;
        let before = old.text.lines().count();
        TextFile {
            text: content.replace("\r\n", "\n"),
            ..old
        }
        .save(&path)?;
        return Ok(format!(
            "replaced {name}: {before} lines before, {lines} now"
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(format!("created {name} ({lines} lines)"))
}

/// A text file with LF line endings in memory; CRLF and BOM come back on save.
pub struct TextFile {
    pub text: String,
    crlf: bool,
    bom: bool,
}

impl TextFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("{} does not exist; use write to create it", path.display())
            }
            _ => format!("cannot read {}: {e}", path.display()),
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not a UTF-8 text file", path.display()))?;
        let bom = text.starts_with('\u{feff}');
        let text = text.trim_start_matches('\u{feff}');
        Ok(Self {
            crlf: text.contains("\r\n"),
            bom,
            text: text.replace("\r\n", "\n"),
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut out = if self.bom {
            "\u{feff}".to_owned()
        } else {
            String::new()
        };
        out += &if self.crlf {
            self.text.replace('\n', "\r\n")
        } else {
            self.text.clone()
        };
        std::fs::write(path, out).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}
