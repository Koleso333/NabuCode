use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ncore::core::ir::{Block, Media, MediaSource, ToolDef};
use serde_json::{Value, json};

use super::outline::outline;
use super::{bool_arg, num_arg, resolve, shown, str_arg};

const LIMIT: usize = 2000;
const LINE_MAX: usize = 2000;
const IMAGE_LIMIT: usize = 5 * 1024 * 1024;

pub fn def() -> ToolDef {
    super::def(
        "read",
        "Reads a local text file or image. PNG, JPEG, GIF and WebP images up to 5 MiB are returned visually; pass only `path` for images. \
Text lines come back numbered as `N\\t<line>` (the same numbers grep gives); the numbers are not part of the file. \
Reads up to 2000 lines from `offset`; when lines remain after them, a last line gives the total and the next offset. \
`ranges` reads several pieces in one call, e.g. `[[40, 80], [300, 340]]`, 1-based and inclusive, split by `--`. \
Ranges past the end are cut to the file. Lines longer than 2000 characters are cut. \
`outline: true` lists only declaration lines (functions, methods, types, classes, impls, top-level constants) \
with their numbers, to pick `ranges` in a large file before reading it. It is a line heuristic for Rust, TS/JS, Python, Go, \
Java, Kotlin, C/C++, C# and similar; it may miss or add a line, so read the range to be sure.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute, or relative to the working directory." },
                "offset": { "type": "integer", "description": "First line to read, 1-based. Default 1." },
                "limit": { "type": "integer", "description": "Lines to read. Default 2000." },
                "outline": { "type": "boolean", "description": "Only declaration lines, with numbers." },
                "ranges": {
                    "type": "array",
                    "description": "Line ranges `[from, to]` instead of offset/limit.",
                    "items": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 }
                }
            },
            "required": ["path"]
        }),
    )
}

pub fn run(directory: &Path, args: &Value) -> Result<Vec<Block>, String> {
    let path = resolve(directory, str_arg(args, "path")?);
    let read_error = |e| format!("cannot read {}: {e}", path.display());
    let mut file = BufReader::new(File::open(&path).map_err(read_error)?);
    let mut bytes = Vec::new();
    if let Some(mime) = image_mime(file.fill_buf().map_err(read_error)?) {
        file.take((IMAGE_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if bytes.len() > IMAGE_LIMIT {
            return Err(format!("{} exceeds the 5 MiB image limit", path.display()));
        }
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        return Ok(vec![
            Block::text(format!("Image: {}", shown(directory, &path))),
            Block::Media {
                media: Media {
                    mime: Some(mime.into()),
                    source: MediaSource::Base64(STANDARD.encode(bytes)),
                    filename,
                },
                cache: None,
                ext: Default::default(),
            },
        ]);
    }
    file.read_to_end(&mut bytes).map_err(read_error)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} is not a UTF-8 text file", path.display()))?;
    read_text(&text, args).map(|text| vec![Block::text(text)])
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn read_text(text: &str, args: &Value) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok("(empty file)".into());
    }
    if bool_arg(args, "outline") {
        return Ok(outline(&lines, cut));
    }
    match args.get("ranges").and_then(Value::as_array) {
        Some(ranges) => read_ranges(&lines, ranges),
        None => read_from(&lines, args),
    }
}

fn read_from(lines: &[&str], args: &Value) -> Result<String, String> {
    let offset = num_arg(args, "offset").unwrap_or(1).max(1);
    let limit = num_arg(args, "limit").unwrap_or(LIMIT).max(1);
    if offset > lines.len() {
        return Err(format!(
            "offset {offset} is past the end; the file has {} lines",
            lines.len()
        ));
    }
    let end = (offset - 1 + limit).min(lines.len());
    let mut out = numbered(lines, offset, end);
    if end < lines.len() {
        out.push(format!(
            "(lines {offset}-{end} of {}; use offset={} to continue)",
            lines.len(),
            end + 1
        ));
    }
    Ok(out.join("\n"))
}

fn read_ranges(lines: &[&str], ranges: &[Value]) -> Result<String, String> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for range in ranges {
        let pair = range
            .as_array()
            .filter(|p| p.len() == 2)
            .and_then(|p| Some((p[0].as_u64()?, p[1].as_u64()?)));
        let Some((from, to)) = pair else {
            return Err("each range must be `[from, to]` with two line numbers".into());
        };
        let (from, to) = (from.max(1) as usize, (to as usize).min(lines.len()));
        if from <= to {
            spans.push((from, to));
        }
    }
    if spans.is_empty() {
        return Err(format!(
            "no range falls inside the file; it has {} lines",
            lines.len()
        ));
    }
    spans.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in spans {
        match merged.last_mut() {
            Some(last) if from <= last.1 + 1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    let parts: Vec<String> = merged
        .iter()
        .map(|&(from, to)| numbered(lines, from, to).join("\n"))
        .collect();
    Ok(format!(
        "{}\n(file has {} lines)",
        parts.join("\n--\n"),
        lines.len()
    ))
}

/// Lines `from..=to`, 1-based, with their numbers.
pub(super) fn numbered(lines: &[&str], from: usize, to: usize) -> Vec<String> {
    lines[from - 1..to]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}\t{}", from + i, cut(line)))
        .collect()
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(LINE_MAX) {
        Some((at, _)) => format!("{}… (line cut at {LINE_MAX} characters)", &line[..at]),
        None => line.to_owned(),
    }
}
