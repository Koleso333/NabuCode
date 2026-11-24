//! What `edit` and `write` changed in files, for the UI to show as diffs.
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use super::{opt_str, resolve, shown};

/// Lines of context around each change.
const CONTEXT: usize = 3;

/// The files a call may change, with their text before it.
pub struct Before(Vec<(PathBuf, Option<String>)>);

impl Before {
    /// `None` for tools that change no files.
    pub fn take(directory: &Path, tool: &str, args: &Value) -> Option<Self> {
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut add = |path: Option<&str>| {
            let path = path.map(|p| resolve(directory, p));
            if let Some(path) = path.filter(|p| !paths.contains(p)) {
                paths.push(path);
            }
        };
        match tool {
            "write" => add(opt_str(args, "path")),
            "edit" => {
                for edit in args
                    .get("edits")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    add(opt_str(edit, "path").or(opt_str(args, "path")));
                }
            }
            _ => return None,
        }
        Some(Self(
            paths
                .into_iter()
                .map(|path| {
                    let before = text(&path);
                    (path, before)
                })
                .collect(),
        ))
    }

    /// Each changed file as `{filePath, relativePath, type, patch, additions, deletions}`.
    pub fn files(&self, directory: &Path) -> Vec<Value> {
        let mut files = Vec::new();
        for (path, before) in &self.0 {
            let Some(after) = text(path) else { continue };
            let old = before.as_deref().unwrap_or_default();
            if old == after {
                continue;
            }
            let name = shown(directory, path);
            let diff = TextDiff::from_lines(old, &after);
            let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
            files.push(json!({
                "filePath": path.to_string_lossy().replace('\\', "/"),
                "relativePath": name,
                "type": if before.is_some() { "update" } else { "add" },
                "patch": diff.unified_diff().context_radius(CONTEXT).header(&name, &name).to_string(),
                "additions": count(ChangeTag::Insert),
                "deletions": count(ChangeTag::Delete),
            }));
        }
        files
    }
}

/// The file's text with LF line ends; `None` when it is missing or not text.
fn text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.replace("\r\n", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_files_come_as_patches() {
        let root = std::env::temp_dir().join("nabucode-changes");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let args = json!({"path": "a.txt", "edits": [{"old": "two", "new": "2"}, {"path": "b.txt", "old": "x", "new": "y"}]});
        let before = Before::take(&root, "edit", &args).unwrap();
        std::fs::write(root.join("a.txt"), "one\n2\nthree\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let files = before.files(&root);
        assert_eq!(files[0]["relativePath"], "a.txt");
        assert_eq!(
            (
                files[0]["type"].as_str(),
                files[0]["additions"].as_u64(),
                files[0]["deletions"].as_u64()
            ),
            (Some("update"), Some(1), Some(1))
        );
        assert!(files[0]["patch"].as_str().unwrap().contains("-two\n+2\n"));
        assert_eq!(files[1]["type"], "add");
        assert!(Before::take(&root, "read", &args).is_none());
    }
}
