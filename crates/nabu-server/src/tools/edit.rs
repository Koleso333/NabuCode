use std::path::{Path, PathBuf};

use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::read::numbered;
use super::write::TextFile;
use super::{opt_str, resolve, shown};

/// Unchanged lines shown around each change.
const AROUND: usize = 2;
/// A changed piece longer than this shows only its first and last lines.
const PIECE_MAX: usize = 60;

pub fn def() -> ToolDef {
    super::def(
        "edit",
        "Changes an existing file by replacing exact text, matched against the file as it is on disk now. \
Each edit replaces `old` with `new`. `old` must match the file exactly, indentation included. \
When it matches more than once the edit fails with the line numbers of the matches, unless `all: true` asks to replace every match. \
Copy `old` from read output without the `N\\t` line number prefix. Take the shortest text that is unique; \
when a snippet repeats, add a nearby distinctive line. `word: true` matches `old` only as a whole word, \
so with `all: true` it renames an identifier without touching longer names that contain it; \
it is plain text matching, so it also hits the same word in strings, comments and unrelated locals. \
`lines: [from, to]` counts only matches that start in those lines, to pick one match or to keep `all` in one function; \
the numbers are those of the file as earlier edits of this call have left it. \
Several edits go in one call, also to several files: `path` on an edit overrides the call's `path`. \
Edits to one file apply in order, each to the result of the previous; either every edit of the call applies \
or no file is changed. When `old` is not found, the error shows the closest text in the file. \
On success each file's changed lines come back with their new numbers and 2 lines around them. \
The file keeps its line endings (LF or CRLF) and BOM; use plain `\\n` in `old` and `new` even for CRLF files.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File for edits without their own `path`. Absolute, or relative to the working directory." },
                "edits": {
                    "type": "array",
                    "description": "Replacements, applied in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "This edit's file, when it differs from the call's `path`." },
                            "old": { "type": "string", "description": "Exact text to replace." },
                            "new": { "type": "string", "description": "Text to put in its place; empty to delete." },
                            "all": { "type": "boolean", "description": "Replace every match of `old`. Default false." },
                            "word": { "type": "boolean", "description": "Match `old` only where it is not part of a longer word. Default false." },
                            "lines": {
                                "type": "array",
                                "description": "`[from, to]`: only matches starting in these lines count, 1-based and inclusive.",
                                "items": { "type": "integer" },
                                "minItems": 2,
                                "maxItems": 2
                            }
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["edits"]
        }),
    )
}

/// One file of the call with its edits and their numbers in the call.
struct Target<'a> {
    path: PathBuf,
    edits: Vec<(usize, &'a Value)>,
}

pub fn run(directory: &Path, args: &Value) -> Result<String, String> {
    let edits = args
        .get("edits")
        .and_then(Value::as_array)
        .filter(|e| !e.is_empty());
    let edits = edits.ok_or("`edits` must be a non-empty list of `{old, new}`")?;
    let targets = targets(directory, args, edits)?;
    let fail = |e: String, number: usize, path: &Path| match edits.len() {
        1 => format!("{e}\n(the file was not changed)"),
        n => format!(
            "edit {number} of {n} ({}): {e}\n(no edit was applied, no file was changed)",
            shown(directory, path)
        ),
    };
    let mut done = Vec::new();
    for target in &targets {
        let mut file =
            TextFile::load(&target.path).map_err(|e| fail(e, target.edits[0].0, &target.path))?;
        let before = file.text.lines().count();
        let mut spans = Vec::new();
        let mut replaced = 0;
        for &(number, edit) in &target.edits {
            replaced += apply(&mut file.text, &mut spans, edit)
                .map_err(|e| fail(e, number, &target.path))?;
        }
        done.push((target, file, before, spans, replaced));
    }
    let mut out = Vec::new();
    for (target, file, before, spans, replaced) in &done {
        file.save(&target.path)?;
        let lines: Vec<&str> = file.text.lines().collect();
        let mut count = match target.edits.len() {
            1 => "1 edit".to_owned(),
            n => format!("{n} edits"),
        };
        if *replaced > target.edits.len() {
            count += &format!(", {replaced} replacements");
        }
        let name = shown(directory, &target.path);
        let head = format!(
            "edited {name} ({count}; {before} -> {} lines). Changed lines now:",
            lines.len()
        );
        out.push(format!("{head}\n{}", changed(&file.text, &lines, spans)));
    }
    Ok(out.join("\n\n"))
}

/// The edits grouped by file in order of first mention; an edit's own `path` wins over the call's.
fn targets<'a>(
    directory: &Path,
    args: &Value,
    edits: &'a [Value],
) -> Result<Vec<Target<'a>>, String> {
    let default = opt_str(args, "path");
    let mut targets: Vec<Target> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let path = opt_str(edit, "path").or(default);
        let path = resolve(
            directory,
            path.ok_or(format!(
                "edit {} has no `path`, and the call gives none",
                i + 1
            ))?,
        );
        match targets.iter_mut().find(|t| t.path == path) {
            Some(target) => target.edits.push((i + 1, edit)),
            None => targets.push(Target {
                path,
                edits: vec![(i + 1, edit)],
            }),
        }
    }
    Ok(targets)
}

/// Applies one edit to `text`, keeping `spans` on the changed bytes; gives the number of replacements.
fn apply(
    text: &mut String,
    spans: &mut Vec<(usize, usize)>,
    edit: &Value,
) -> Result<usize, String> {
    let field = |key: &str| {
        edit.get(key)
            .and_then(Value::as_str)
            .map(|s| s.replace("\r\n", "\n"))
    };
    let (Some(old), Some(new)) = (field("old"), field("new")) else {
        return Err("each edit needs string fields `old` and `new`".into());
    };
    if old.is_empty() {
        return Err("`old` is empty; to create a file or rewrite all of it use write".into());
    }
    if old == new {
        return Err("`old` and `new` are the same".into());
    }
    let flag = |key: &str| edit.get(key).and_then(Value::as_bool).unwrap_or(false);
    let word = flag("word");
    let found: Vec<usize> = text
        .match_indices(&old)
        .map(|(at, _)| at)
        .filter(|&at| !word || whole_word(text, at, at + old.len()))
        .collect();
    let all = flag("all");
    if found.is_empty() && word && text.contains(&old) {
        return Err(
            "`old` occurs only inside longer words; drop `word: true` to match those".into(),
        );
    }
    if found.is_empty() {
        return Err(missing(text, &old));
    }
    let found = match within(edit)? {
        Some((from, to)) => {
            let inside: Vec<usize> = found
                .iter()
                .copied()
                .filter(|&at| (from..=to).contains(&line_at(text, at)))
                .collect();
            if inside.is_empty() {
                let at: Vec<String> = found
                    .iter()
                    .map(|&at| line_at(text, at).to_string())
                    .collect();
                return Err(format!(
                    "`old` is not in lines {from}-{to}; it is at lines {}",
                    at.join(", ")
                ));
            }
            inside
        }
        None => found,
    };
    if found.len() > 1 && !all {
        let at: Vec<String> = found
            .iter()
            .map(|&at| line_at(text, at).to_string())
            .collect();
        return Err(format!(
            "`old` matches {} places, at lines {}; add nearby lines to `old` to pick one, or set `all: true` to replace every one",
            found.len(),
            at.join(", ")
        ));
    }
    // From the end, so the earlier offsets stay valid.
    for &at in found.iter().rev() {
        text.replace_range(at..at + old.len(), &new);
        shift(spans, at, at + old.len(), new.len());
    }
    Ok(found.len())
}

/// Moves the spans after a replacement of `from..to` by `len` bytes and adds the new span, merged with any it touches.
fn shift(spans: &mut Vec<(usize, usize)>, from: usize, to: usize, len: usize) {
    let moved = |at: usize| at + len - (to - from);
    let mut merged = (from, from + len);
    spans.retain_mut(|span| {
        if span.1 < from {
            return true;
        }
        if span.0 > to {
            *span = (moved(span.0), moved(span.1));
            return true;
        }
        merged = (
            merged.0.min(span.0),
            merged.1.max(if span.1 > to { moved(span.1) } else { 0 }),
        );
        false
    });
    spans.push(merged);
}

/// The `lines: [from, to]` an edit is limited to, if any.
fn within(edit: &Value) -> Result<Option<(usize, usize)>, String> {
    let Some(range) = edit.get("lines") else {
        return Ok(None);
    };
    let pair = range
        .as_array()
        .filter(|p| p.len() == 2)
        .and_then(|p| Some((p[0].as_u64()? as usize, p[1].as_u64()? as usize)));
    pair.filter(|(from, to)| from <= to)
        .map(Some)
        .ok_or_else(|| "`lines` must be `[from, to]` with from <= to".into())
}

/// Whether `from..to` has no letter, digit or `_` right before or after it.
fn whole_word(text: &str, from: usize, to: usize) -> bool {
    let inner = |c: char| c.is_alphanumeric() || c == '_';
    !text[..from].chars().next_back().is_some_and(inner)
        && !text[to..].chars().next().is_some_and(inner)
}

/// 1-based line of a byte offset, including offsets inside a UTF-8 character.
fn line_at(text: &str, at: usize) -> usize {
    text.as_bytes()[..at]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
        + 1
}

/// The changed lines with a little around them, as numbered pieces split by `--`.
fn changed(text: &str, lines: &[&str], spans: &[(usize, usize)]) -> String {
    if lines.is_empty() {
        return "(the file is empty now)".into();
    }
    let mut ranges: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(from, to)| {
            (
                line_at(text, from).min(lines.len()),
                line_at(text, to.max(from + 1) - 1).min(lines.len()),
            )
        })
        .map(|(from, to)| {
            (
                from.saturating_sub(AROUND).max(1),
                (to + AROUND).min(lines.len()),
            )
        })
        .collect();
    ranges.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in ranges {
        match merged.last_mut() {
            Some(last) if from <= last.1 + 1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    let pieces: Vec<String> = merged
        .iter()
        .map(|&(from, to)| piece(lines, from, to))
        .collect();
    pieces.join("\n--\n")
}

fn piece(lines: &[&str], from: usize, to: usize) -> String {
    if to - from < PIECE_MAX {
        return numbered(lines, from, to).join("\n");
    }
    let edge = PIECE_MAX / 2 - 1;
    let head = numbered(lines, from, from + edge - 1).join("\n");
    let tail = numbered(lines, to - edge + 1, to).join("\n");
    format!("{head}\n… ({} lines)\n{tail}", to - from + 1 - 2 * edge)
}

/// Why `old` was not found, with the closest text from the file.
fn missing(text: &str, old: &str) -> String {
    let numbered_line = |l: &str| {
        l.split_once('\t').is_some_and(|(n, _)| {
            !n.trim().is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit())
        })
    };
    if old.lines().filter(|l| !l.is_empty()).all(numbered_line) {
        return "`old` was not found: its lines start with `N\\t` numbers from read output, which are not part of the file".into();
    }
    let lines: Vec<&str> = text.lines().collect();
    let want: Vec<&str> = old.trim_matches('\n').lines().map(str::trim).collect();
    let fits = |at: usize| {
        want.iter()
            .enumerate()
            .all(|(i, w)| lines.get(at + i).is_some_and(|l| l.trim() == *w))
    };
    if let Some(at) = (0..lines.len()).find(|&at| fits(at)) {
        let (from, to) = (at + 1, at + want.len());
        return format!(
            "`old` was not found exactly, but it matches lines {from}-{to} when spaces at line starts and ends are ignored. Those lines are:\n{}",
            numbered(&lines, from, to).join("\n")
        );
    }
    let first = want.iter().find(|w| !w.is_empty()).copied().unwrap_or("");
    let hits: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim() == first)
        .collect();
    let Some(&at) = hits.first() else {
        return "`old` was not found, nor its first line; the file may differ from what you expect, read it again".into();
    };
    let to = (at + want.len() + AROUND).min(lines.len());
    let more = match hits.len() {
        1 => String::new(),
        n => format!(" (it is on {n} lines; the first one shown)"),
    };
    format!(
        "`old` was not found. Its first line is at line {}{more}, but the lines after it differ. The file there:\n{}",
        at + 1,
        numbered(&lines, at + 1, to).join("\n")
    )
}
