use std::path::Path;
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::{listing, opt_str, root, shown, size, str_list, walk};

pub fn def() -> ToolDef {
    super::def(
        "glob",
        "Finds files by name pattern. Skips what .gitignore and .ignore exclude and the .git folder; hidden files are included. \
A pattern without `/` matches file names at any depth (`*.rs`); with `/` it matches the path relative to `path` (`src/**/*.ts`). \
`*` stays inside one folder, `**` crosses folders, `{a,b}` gives alternatives. \
Give several patterns as a list; a pattern starting with `!` excludes (`[\"*.ts\", \"!**/vendor/**\"]`). \
`path` may be any folder, also outside the working directory. \
Each result is `path (N lines)`, relative to the working directory when inside it, else absolute. Sorted by path unless `sort` is `modified` (newest first). \
At most 200 files are listed, then a line says how many more matched.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "description": "Glob patterns; `!` in front excludes.",
                    "type": "array", "items": { "type": "string" }
                },
                "path": { "type": "string", "description": "Folder to search in. Default: the working directory." },
                "sort": { "type": "string", "enum": ["path", "modified"] }
            },
            "required": ["pattern"]
        }),
    )
}

pub fn run(directory: &Path, args: &Value) -> Result<String, String> {
    let patterns = str_list(args, "pattern");
    if patterns.is_empty() {
        return Err("missing argument `pattern`".into());
    }
    let filter = Filter::new(&patterns)?;
    let root = root(directory, args)?;
    let mut scanned = 0;
    let mut found: Vec<(SystemTime, String, String)> = walk(&root)
        .inspect(|_| scanned += 1)
        .filter(|path| filter.matches(&root, path))
        .map(|path| (modified(&path), shown(directory, &path), size(&path)))
        .collect();
    if found.is_empty() {
        return Ok(format!(
            "no files match {} in `{}` ({scanned} files scanned)",
            patterns.join(", "),
            shown(directory, &root)
        ));
    }
    match opt_str(args, "sort").unwrap_or("path") {
        "modified" => found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))),
        "path" => found.sort_by(|a, b| a.1.cmp(&b.1)),
        other => return Err(format!("unknown sort `{other}`; use path or modified")),
    }
    let found = found
        .into_iter()
        .map(|(_, path, size)| format!("{path} ({size})"))
        .collect();
    Ok(listing(found, "files", "narrow the pattern or the path"))
}

/// Include and `!` exclude patterns; with only excludes, everything else matches.
pub struct Filter {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl Filter {
    pub fn new(patterns: &[String]) -> Result<Self, String> {
        let mut filter = Self {
            include: Vec::new(),
            exclude: Vec::new(),
        };
        for pattern in patterns {
            match pattern.strip_prefix('!') {
                Some(excluded) => filter.exclude.push(Pattern::new(excluded)?),
                None => filter.include.push(Pattern::new(pattern)?),
            }
        }
        Ok(filter)
    }

    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        (self.include.is_empty() || self.include.iter().any(|p| p.matches(root, path)))
            && !self.exclude.iter().any(|p| p.matches(root, path))
    }
}

/// A glob where `*` stays inside one folder and `**` crosses folders.
/// Without `/` it matches file names at any depth, else paths from the search root.
struct Pattern {
    matcher: GlobMatcher,
    by_name: bool,
}

impl Pattern {
    fn new(pattern: &str) -> Result<Self, String> {
        let matcher = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("bad glob `{pattern}`: {e}"))?
            .compile_matcher();
        Ok(Self {
            matcher,
            by_name: !pattern.contains('/'),
        })
    }

    fn matches(&self, root: &Path, path: &Path) -> bool {
        let target = if self.by_name {
            path.file_name().map(Path::new)
        } else {
            path.strip_prefix(root).ok()
        };
        target.is_some_and(|t| self.matcher.is_match(t))
    }
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}
