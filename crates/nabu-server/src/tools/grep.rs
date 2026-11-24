use std::collections::BTreeSet;
use std::path::Path;

use ncore::core::ir::ToolDef;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use super::glob::Filter;
use super::{
    SHOWN, bool_arg, listing, num_arg, opt_str, root, shown, str_arg, str_list, text_of, walk,
};

const LINE_MAX: usize = 300;
const CONTEXT_MAX: usize = 10;

pub fn def() -> ToolDef {
    super::def(
        "grep",
        "Searches file contents with a regular expression (Rust regex syntax; `(?i)` and `|` work). \
Skips binary files, what .gitignore and .ignore exclude and the .git folder; hidden files are searched. \
`output`: `lines` (default) gives each file's path on its own line, then `N: text` for matching lines \
and `N- text` for context lines, groups split by `--`; \
`files` lists matching files as `path (N lines)`; `count` gives `path: K of N lines`. \
`max_per_file` has no default: every matching line is shown until the overall limit. \
`path` may be any file or folder, also outside the working directory (sibling repos, dependencies). \
`glob` filters files the same way as the glob tool: without `/` by file name, with `/` by path relative to `path`; \
several patterns as a list, `!` in front excludes (`[\"*.ts\", \"!**/vendor/**\"]`). \
Paths are relative to the working directory when inside it, else absolute. \
At most 200 output lines are listed, context lines and `--` included, then a line gives the totals; lines longer than 300 characters end with `…`. \
An empty result says how many files were searched.",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression." },
                "path": { "type": "string", "description": "File or folder to search. Default: the working directory." },
                "glob": {
                    "description": "Only files matching these globs; `!` in front excludes.",
                    "type": "array", "items": { "type": "string" }
                },
                "output": { "type": "string", "enum": ["lines", "files", "count"] },
                "context": { "type": "integer", "description": "Lines to show around each match, up to 10." },
                "max_per_file": { "type": "integer", "description": "Matching lines shown per file in `lines` output." },
                "ignore_case": { "type": "boolean" },
                "multiline": { "type": "boolean", "description": "Let `.` cross line breaks; `\\s` always does. For matches spanning lines." }
            },
            "required": ["pattern"]
        }),
    )
}

pub fn run(directory: &Path, args: &Value) -> Result<String, String> {
    let pattern = str_arg(args, "pattern")?;
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(bool_arg(args, "ignore_case"))
        .multi_line(true)
        .dot_matches_new_line(bool_arg(args, "multiline"))
        .build()
        .map_err(|e| format!("bad regular expression: {e}"))?;
    let output = opt_str(args, "output").unwrap_or("lines");
    if !["lines", "files", "count"].contains(&output) {
        return Err(format!(
            "unknown output `{output}`; use lines, files or count"
        ));
    }
    let context = num_arg(args, "context").unwrap_or(0).min(CONTEXT_MAX);
    let per_file = num_arg(args, "max_per_file").filter(|&n| n > 0);
    let globs = str_list(args, "glob");
    let filter = Filter::new(&globs)?;
    let root = root(directory, args)?;

    let mut out = Vec::new();
    let (mut searched, mut files, mut total) = (0, 0, 0);
    for path in walk(&root)
        .filter(|path| filter.matches(&root, path))
        .collect::<BTreeSet<_>>()
    {
        searched += 1;
        let Some(text) = text_of(&path) else { continue };
        let hits = hits(&regex, &text);
        if hits.is_empty() {
            continue;
        }
        files += 1;
        total += hits.len();
        let name = shown(directory, &path);
        let size = text.lines().count();
        match output {
            "files" => out.push(format!("{name} ({size} lines)")),
            "count" => out.push(format!("{name}: {} of {size} lines", hits.len())),
            _ => out.extend(lines(&name, &text, &hits, context, per_file)),
        }
    }
    let place = shown(directory, &root);
    if searched == 0 {
        return Ok(format!(
            "glob {} matched no files in `{place}`; nothing was searched",
            globs.join(", ")
        ));
    }
    if out.is_empty() {
        return Ok(format!(
            "no matches for `{pattern}` in `{place}` ({searched} files searched)"
        ));
    }
    let truncated = out.len() > SHOWN;
    let mut result = match output {
        "lines" => listing(
            out,
            "lines",
            "narrow the pattern or glob, lower `context` or set `max_per_file`",
        ),
        _ => listing(out, "files", "narrow the pattern or glob"),
    };
    if truncated {
        result.push_str(&format!(
            "\n({total} matching lines in {files} files in total)"
        ));
    }
    Ok(result)
}

/// 0-based numbers of the lines any match touches.
fn hits(regex: &Regex, text: &str) -> BTreeSet<usize> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line_at = |offset: usize| starts.partition_point(|&s| s <= offset) - 1;
    regex
        .find_iter(text)
        .flat_map(|m| line_at(m.start())..=line_at(m.end().saturating_sub(1).max(m.start())))
        .collect()
}

/// Matching lines with their context, `--` between groups that do not touch.
fn lines(
    name: &str,
    text: &str,
    hits: &BTreeSet<usize>,
    context: usize,
    per_file: Option<usize>,
) -> Vec<String> {
    let all: Vec<&str> = text.lines().collect();
    let kept: BTreeSet<usize> = hits
        .iter()
        .copied()
        .take(per_file.unwrap_or(usize::MAX))
        .collect();
    let shown: BTreeSet<usize> = kept
        .iter()
        .flat_map(|&i| i.saturating_sub(context)..=(i + context).min(all.len().saturating_sub(1)))
        .collect();
    let mut out = vec![name.to_owned()];
    let mut last: Option<usize> = None;
    for i in shown {
        if context > 0 && last.is_some_and(|l| i > l + 1) {
            out.push("--".into());
        }
        let mark = if kept.contains(&i) { ':' } else { '-' };
        out.push(format!(
            "{}{mark} {}",
            i + 1,
            cut(all.get(i).copied().unwrap_or(""))
        ));
        last = Some(i);
    }
    if kept.len() < hits.len() {
        out.push(format!(
            "({} more matching lines in this file)",
            hits.len() - kept.len()
        ));
    }
    out
}

fn cut(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    match line.char_indices().nth(LINE_MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}
