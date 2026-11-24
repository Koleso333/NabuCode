use std::path::Path;

use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::{SHOWN, listing, num_arg, root, shown, size, walker};

const DEPTH: usize = 2;
const DEPTH_MAX: usize = 6;

pub fn def() -> ToolDef {
    super::def(
        "list",
        "Shows a folder as a tree: folders end with `/`, files show `(N lines)`. Two levels deep by default. \
Skips what .gitignore and .ignore exclude and the .git folder; hidden files are shown. \
`path` may be any folder, also outside the working directory. At most 200 entries are listed.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Folder to show. Default: the working directory." },
                "depth": { "type": "integer", "description": "Levels to show, 1 to 6. Default 2." }
            }
        }),
    )
}

pub fn run(directory: &Path, args: &Value) -> Result<String, String> {
    let root = root(directory, args)?;
    if !root.is_dir() {
        return Err(format!("{} is a file; use read", root.display()));
    }
    let depth = num_arg(args, "depth").unwrap_or(DEPTH).clamp(1, DEPTH_MAX);
    let entries: Vec<ignore::DirEntry> = walker(&root)
        .max_depth(Some(depth))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.depth() > 0)
        .collect();
    if entries.is_empty() {
        return Ok(format!("`{}` is empty", shown(directory, &root)));
    }
    let lines = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let indent = "  ".repeat(e.depth() - 1);
            let name = e.file_name().to_string_lossy();
            match e.file_type().is_some_and(|t| t.is_dir()) {
                true => format!("{indent}{name}/"),
                // Sizes only for what is shown: reading every file of a big tree is wasted work.
                false if i < SHOWN => format!("{indent}{name} ({})", size(e.path())),
                false => format!("{indent}{name}"),
            }
        })
        .collect();
    Ok(listing(
        lines,
        "entries",
        "list a subfolder or lower `depth`",
    ))
}
