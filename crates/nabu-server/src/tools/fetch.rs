use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ncore::core::ir::ToolDef;
use regex::{Regex, RegexBuilder};
use reqwest::Url;
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};

use super::browser;
use super::web::{clip, get, squeeze};
use super::{num_arg, opt_str, str_arg};

pub const BRIEF: &str =
    "read a web page as Markdown with line numbers, in parts, or only the lines matching a pattern";

const LINES: usize = 300;
const LINES_MAX: usize = 2000;
/// Characters one call shows at most, whatever `limit` says.
const CHARS: usize = 40_000;
const LINE_CHARS: usize = 2000;
const MATCHES: usize = 40;
/// Characters shown on each side of a match inside a long line.
const WINDOW: usize = 300;
const OUTLINE: usize = 60;
const BYTES_MAX: usize = 10 << 20;
/// Fewer letters than this on a page with scripts: the scripts build its content.
const BUILT_BY_SCRIPTS: usize = 50;
const KEPT: Duration = Duration::from_secs(15 * 60);
const KEPT_PAGES: usize = 30;

pub fn def() -> ToolDef {
    super::def(
        "fetch",
        "Reads a web page as Markdown with numbered lines: the main content (the page's <main> or <article> when it has one), \
without scripts, menus and footers; links are absolute. JSON, plain text and other text files come as they are. \
A long page comes in parts: the first call shows its outline (headings with line numbers) and the first 300 lines; \
continue with `offset`, or use `find` to get only the lines matching a pattern, with their numbers. \
Pages are kept for 15 minutes, so further calls on the same URL do not download it again. \
A page that comes out without text is opened again in a headless Chrome so its scripts can build the content (a few seconds); \
`js` does that for any page, for when the content you need is missing.",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http or https URL." },
                "offset": { "type": "integer", "description": "First line to show (1-based). Default 1." },
                "limit": { "type": "integer", "description": "Lines to show, up to 2000. Default 300." },
                "find": {
                    "type": "string",
                    "description": "Regex, case-insensitive: show only matching lines, each with 2 lines around it."
                },
                "raw": { "type": "boolean", "description": "The HTML source instead of Markdown." },
                "js": { "type": "boolean", "description": "Run the page's JavaScript in a headless Chrome before reading it." }
            },
            "required": ["url"]
        }),
    )
}

/// A downloaded page, as Markdown and as the source it came from.
struct Page {
    url: String,
    title: String,
    lines: Vec<String>,
    source: Vec<String>,
    /// How the page was got, when not by a plain download.
    note: String,
}

static PAGES: LazyLock<Mutex<HashMap<String, (Instant, Arc<Page>)>>> =
    LazyLock::new(Default::default);

pub async fn run(args: Value) -> Result<String, String> {
    let url = str_arg(&args, "url")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("`{url}` is not an http(s) URL"));
    }
    let js = flag(&args, "js");
    let key = if js {
        format!("js {url}")
    } else {
        url.to_owned()
    };
    let page = match kept(&key) {
        Some(page) => page,
        None => {
            let page = Arc::new(download(url, js).await?);
            keep(&key, page.clone());
            page
        }
    };
    let lines = if flag(&args, "raw") {
        &page.source
    } else {
        &page.lines
    };
    show(&page.title, &page.url, lines, &page.note, &args)
}

/// The page's lines as `fetch` and `browser_read` show them: a part with the outline, or the lines `find` matches.
pub(super) fn show(title: &str, url: &str, lines: &[String], note: &str, args: &Value) -> Result<String, String> {
    let head = format!("{title}\n{url} ({} lines){note}", lines.len());
    if lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(format!("{head}\nthe page has no text"));
    }
    if let Some(pattern) = opt_str(args, "find") {
        let found = find(lines, pattern)?;
        let outline = if found.starts_with("no lines match") {
            outline(lines)
        } else {
            String::new()
        };
        return Ok(format!("{head}\n{found}\n{outline}").trim_end().to_owned());
    }
    let offset = num_arg(args, "offset").unwrap_or(1).max(1);
    let limit = num_arg(args, "limit").unwrap_or(LINES).clamp(1, LINES_MAX);
    if offset > lines.len() {
        return Err(format!(
            "`offset` {offset} is past the end: the page has {} lines",
            lines.len()
        ));
    }
    let (shown, last) = part(lines, offset, limit);
    let whole = offset == 1 && last == lines.len();
    let outline = if offset == 1 && !whole {
        outline(lines)
    } else {
        String::new()
    };
    let rest = if last < lines.len() {
        format!(
            "\n(lines {offset}-{last} of {}; continue with offset {}, or use find)",
            lines.len(),
            last + 1
        )
    } else {
        String::new()
    };
    Ok(format!("{head}\n{outline}{shown}{rest}"))
}

fn kept(url: &str) -> Option<Arc<Page>> {
    let pages = PAGES.lock().unwrap();
    pages
        .get(url)
        .filter(|(at, _)| at.elapsed() < KEPT)
        .map(|(_, page)| page.clone())
}

/// Keeps the page, dropping expired ones and the oldest past the limit.
fn keep(url: &str, page: Arc<Page>) {
    let mut pages = PAGES.lock().unwrap();
    pages.retain(|_, (at, _)| at.elapsed() < KEPT);
    if pages.len() >= KEPT_PAGES
        && let Some(oldest) = pages
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone())
    {
        pages.remove(&oldest);
    }
    pages.insert(url.to_owned(), (Instant::now(), page));
}

fn flag(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

async fn download(url: &str, js: bool) -> Result<Page, String> {
    let response = get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    let final_url = response.url().to_string();
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "HTTP {status} from {final_url}\n{}",
            clip(&squeeze(&strip(&body)), 500)
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n as usize > BYTES_MAX)
    {
        return Err(format!("the page is larger than {} MB", BYTES_MAX >> 20));
    }
    if kind.contains("pdf") {
        return Err("this is a PDF; reading PDFs is not supported yet".into());
    }
    if !textual(&kind) {
        return Err(format!("this is not a text page but `{kind}`"));
    }
    let mut body = response
        .text()
        .await
        .map_err(|e| format!("reading the page failed: {e}"))?;
    if kind.contains("json") && serde_json::from_str::<serde::de::IgnoredAny>(&body).is_ok() {
        body = pretty_json(&body);
    }
    let source = split(&body);
    if !kind.contains("html") {
        let title = final_url
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&final_url)
            .to_owned();
        return Ok(Page {
            url: final_url,
            title,
            lines: source.clone(),
            source,
            note: String::new(),
        });
    }
    let base = Url::parse(&final_url).map_err(|e| e.to_string())?;
    let (title, text) = convert(body.clone(), base.clone()).await?;
    let built_by_scripts = body.contains("<script")
        && text.split_whitespace().map(str::len).sum::<usize>() < BUILT_BY_SCRIPTS;
    if !js && !built_by_scripts {
        return Ok(Page {
            url: final_url,
            title,
            lines: split(&text),
            source,
            note: String::new(),
        });
    }
    let (body, downloaded) = match browser::render(&final_url).await {
        Ok(rendered) => rendered,
        Err(e) if js => return Err(e),
        Err(e) => {
            let note = format!(
                "\nthe page builds its content with JavaScript, and running it failed: {e}"
            );
            return Ok(Page {
                url: final_url,
                title,
                lines: split(&text),
                source,
                note,
            });
        }
    };
    let note = match downloaded {
        true => "\nrendered with JavaScript; a headless Chrome was downloaded for that first",
        false => "\nrendered with JavaScript",
    };
    let (title, text) = convert(body.clone(), base).await?;
    Ok(Page {
        url: final_url,
        title,
        lines: split(&text),
        source: split(&body),
        note: note.into(),
    })
}

pub(super) async fn convert(html: String, base: Url) -> Result<(String, String), String> {
    tokio::task::spawn_blocking(move || markdown(&html, &base))
        .await
        .map_err(|e| format!("converting the page crashed: {e}"))
}

fn textual(kind: &str) -> bool {
    kind.is_empty()
        || kind.starts_with("text/")
        || ["json", "xml", "javascript", "yaml", "toml"]
            .iter()
            .any(|t| kind.contains(t))
}

pub(super) fn split(text: &str) -> Vec<String> {
    text.lines().map(|l| l.trim_end().to_owned()).collect()
}

/// Minified JSON one value per line, keys in their order, so `find` and parts work on it.
fn pretty_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len() * 2);
    let (mut depth, mut in_string, mut escaped) = (0, false, false);
    let mut chars = json.chars().peekable();
    let new_line = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                let close = if c == '{' { '}' } else { ']' };
                while chars.next_if(|c| c.is_whitespace()).is_some() {}
                out.push(c);
                if chars.next_if_eq(&close).is_some() {
                    out.push(close);
                } else {
                    depth += 1;
                    new_line(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                new_line(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                new_line(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// The page title and its main content as Markdown.
fn markdown(html: &str, base: &Url) -> (String, String) {
    let doc = Html::parse_document(html);
    let title = doc
        .select(&selector("title"))
        .next()
        .map(|t| squeeze(&t.text().collect::<String>()))
        .unwrap_or_default();
    let main = main_part(&doc).map_or_else(|| html.to_owned(), |e| e.html());
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "template", "nav", "footer", "aside", "svg", "form",
            "button", "iframe", "dialog",
        ])
        .options(htmd::options::Options {
            bullet_list_marker: htmd::options::BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Default::default()
        })
        .build();
    let text = converter.convert(&main).unwrap_or_default();
    (title, tidy(&links(&text, base)))
}

/// `<main>`, else the longest `<article>`, else the body.
fn main_part(doc: &Html) -> Option<ElementRef<'_>> {
    let length = |e: &ElementRef| e.text().map(str::len).sum::<usize>();
    doc.select(&selector("main, [role=main]"))
        .next()
        .or_else(|| doc.select(&selector("article")).max_by_key(length))
        .or_else(|| doc.select(&selector("body")).next())
}

/// Images as `[image: alt]`, or nothing without alt; links absolute and readable, without titles;
/// heading anchors dropped, in-page and script links left as their text.
fn links(markdown: &str, base: &Url) -> String {
    // Labels may hold escaped brackets, like `\[1\]`.
    static IMAGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"!\[((?:\\.|[^\]\\])*)\]\([^)]*\)").unwrap());
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[((?:\\.|[^\]\\])*)\]\(([^)\s]+)[^)]*\)").unwrap());
    let text = IMAGE.replace_all(markdown, |c: &regex::Captures| match c[1].trim() {
        "" => String::new(),
        alt => format!("[image: {alt}]"),
    });
    LINK.replace_all(&text, |c: &regex::Captures| {
        let (label, target) = (&c[1], &c[2]);
        if ["", "§", "¶", "#"].contains(&label.trim()) {
            return String::new();
        }
        if target.starts_with('#')
            || target.starts_with("data:")
            || target.starts_with("javascript:")
        {
            return label.to_owned();
        }
        let url = base
            .join(target)
            .map_or_else(|_| target.to_owned(), |u| u.to_string());
        format!("[{label}]({})", readable(&url))
    })
    .into_owned()
}

/// Percent-encoded letters decoded, so a Cyrillic URL costs its length and not three times it.
fn readable(url: &str) -> String {
    match percent_encoding::percent_decode_str(url).decode_utf8() {
        Ok(text)
            if !text.contains(|c: char| {
                c.is_whitespace() || c.is_control() || "()<>\"".contains(c)
            }) =>
        {
            text.into_owned()
        }
        _ => url.to_owned(),
    }
}

/// No runs of blank lines, no blank lines at the ends; table cells without their padding.
fn tidy(text: &str) -> String {
    static PADDING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" {2,}|-{4,}").unwrap());
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        let line = match line.starts_with('|') {
            true => PADDING
                .replace_all(
                    line,
                    |c: &regex::Captures| if c[0].starts_with(' ') { " " } else { "---" },
                )
                .into_owned(),
            false => line.to_owned(),
        };
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Numbered lines from `offset`, stopping at `limit` lines or the character budget; and the last line shown.
fn part(lines: &[String], offset: usize, limit: usize) -> (String, usize) {
    let mut out = Vec::new();
    let mut chars = 0;
    let mut last = offset - 1;
    for (i, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        chars += line.len() + 8;
        if chars > CHARS && !out.is_empty() {
            break;
        }
        out.push(format!("{}\t{}", i + 1, clip(line, LINE_CHARS)));
        last = i + 1;
    }
    (out.join("\n"), last)
}

/// Markdown headings with their line numbers.
fn outline(lines: &[String]) -> String {
    let headings: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('#') && l.trim_start_matches('#').starts_with(' '))
        .map(|(i, l)| format!("{}\t{l}", i + 1))
        .collect();
    if headings.len() < 2 {
        return String::new();
    }
    let more = headings.len().saturating_sub(OUTLINE);
    let mut text = headings
        .into_iter()
        .take(OUTLINE)
        .collect::<Vec<_>>()
        .join("\n");
    if more > 0 {
        text.push_str(&format!("\n({more} more headings)"));
    }
    format!("Outline:\n{text}\n\n")
}

/// Lines matching `pattern`; a pattern escaped twice (`\\.` for a dot) is retried escaped once.
fn find(lines: &[String], pattern: &str) -> Result<String, String> {
    let regex = |p: &str| {
        RegexBuilder::new(p)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("bad regular expression: {e}"))
    };
    let text = found(lines, &regex(pattern)?);
    let once = pattern.replace("\\\\", "\\");
    if !text.starts_with("no lines match") || once == pattern {
        return Ok(text);
    }
    match regex(&once).map(|r| found(lines, &r)) {
        Ok(retried) if !retried.starts_with("no lines match") => Ok(format!(
            "no lines match `{pattern}`, where `\\\\` is a literal backslash; with `{once}`: {retried}"
        )),
        _ => Ok(text),
    }
}

/// Matching lines with 2 lines around each, groups apart by `--`.
fn found(lines: &[String], regex: &Regex) -> String {
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| regex.is_match(l))
        .map(|(i, _)| i)
        .collect();
    if hits.is_empty() {
        return format!("no lines match `{}`", regex.as_str());
    }
    let mut out: Vec<String> = Vec::new();
    let mut end = 0;
    for &hit in hits.iter().take(MATCHES) {
        let from = hit.saturating_sub(2).max(end);
        if from > end && end > 0 {
            out.push("--".into());
        }
        for i in from..(hit + 3).min(lines.len()) {
            let mark = if i == hit || regex.is_match(&lines[i]) {
                ":"
            } else {
                "-"
            };
            out.push(format!("{}{mark} {}", i + 1, around(&lines[i], regex)));
        }
        end = (hit + 3).min(lines.len());
    }
    let mut text = format!("{} matching lines\n{}", hits.len(), out.join("\n"));
    if hits.len() > MATCHES {
        text.push_str(&format!(
            "\n({} more matches not shown; narrow the pattern or read with offset)",
            hits.len() - MATCHES
        ));
    }
    text
}

/// A long line as the part around its first match, else its start.
fn around(line: &str, regex: &Regex) -> String {
    if line.chars().count() <= LINE_CHARS {
        return line.to_owned();
    }
    let Some(hit) = regex.find(line) else {
        return clip(line, WINDOW);
    };
    let from = line[..hit.start()]
        .char_indices()
        .rev()
        .nth(WINDOW - 1)
        .map_or(0, |(i, _)| i);
    let to = line[hit.end()..]
        .char_indices()
        .nth(WINDOW)
        .map_or(line.len(), |(i, _)| hit.end() + i);
    let dots = |cut: bool| if cut { "…" } else { "" };
    format!(
        "{}{}{}",
        dots(from > 0),
        &line[from..to],
        dots(to < line.len())
    )
}

/// Tags removed, for short error bodies.
fn strip(html: &str) -> String {
    static TAG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<script.*?</script>|<style.*?</style>|<[^>]*>").unwrap());
    TAG.replace_all(html, " ").into_owned()
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_content_becomes_markdown_with_absolute_links() {
        let html = r#"<html><head><title>Doc</title></head><body><nav><a href="/x">menu</a></nav>
<main><h1>Title</h1><p>See <a href="../b.html">b</a> and <img src="i.png"><img alt="chart" src="c.png"></p>
<pre><code>let x = 1;</code></pre><ul><li>one</li><li>two</li></ul></main><footer>foot</footer></body></html>"#;
        let base = Url::parse("https://site.dev/docs/a/page.html").unwrap();
        let (title, text) = markdown(html, &base);
        assert_eq!(title, "Doc");
        assert_eq!(
            text,
            "# Title\n\nSee [b](https://site.dev/docs/b.html) and [image: chart]\n\n```\nlet x = 1;\n```\n\n- one\n- two"
        );
    }

    #[test]
    fn anchors_titles_and_padding_are_dropped() {
        let base = Url::parse("https://ru.wikipedia.org/wiki/A").unwrap();
        let text = "## [§](#x)Fairness\n[Сеть](/wiki/%D0%A1%D0%B5%D1%82%D1%8C \"Сеть\")[\\[1\\]](#cite-1)\n| a     | b |\n| ----- | --- |";
        assert_eq!(
            tidy(&links(text, &base)),
            "## Fairness\n[Сеть](https://ru.wikipedia.org/wiki/Сеть)\\[1\\]\n| a | b |\n| --- | --- |"
        );
    }

    #[test]
    fn find_groups_nearby_matches() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let regex = RegexBuilder::new("^line (5|6|15)$").build().unwrap();
        assert_eq!(
            found(&lines, &regex),
            "3 matching lines\n3- line 3\n4- line 4\n5: line 5\n6: line 6\n7- line 7\n8- line 8\n--\n13- line 13\n14- line 14\n15: line 15\n16- line 16\n17- line 17"
        );
    }

    #[tokio::test]
    #[ignore = "downloads pages"]
    async fn live() {
        for url in std::env::var("FETCH_URLS").unwrap().split(' ') {
            let args = serde_json::json!({"url": url, "limit": 40});
            println!(
                "=== {url}
{}
",
                run(args).await.unwrap_or_else(|e| format!("ERROR {e}"))
            );
        }
        let args = serde_json::json!({"url": "https://docs.rs/tokio/latest/tokio/macro.select.html", "find": "cancel(lation)? safe"});
        println!(
            "=== find
{}",
            run(args).await.unwrap()
        );
    }

    #[test]
    fn find_retries_a_pattern_escaped_twice() {
        let lines: Vec<String> = ["Tokio v1.54.1", "other"].map(String::from).to_vec();
        let text = find(&lines, r"v1\\.54").unwrap();
        assert!(text.starts_with(r"no lines match `v1\\.54`, where `\\` is a literal backslash; with `v1\.54`: 1 matching lines"), "{text}");
    }

    #[test]
    fn parts_stop_at_the_limit() {
        let lines: Vec<String> = (1..=10).map(|i| format!("l{i}")).collect();
        assert_eq!(part(&lines, 4, 3), ("4\tl4\n5\tl5\n6\tl6".into(), 6));
    }

    #[test]
    fn json_comes_one_value_per_line_in_its_order() {
        let json = r#"{"z":1,"a":{"s":"x, {y}: \"q\"","e":[],"o":{}},"l":[1, 2]}"#;
        let pretty = "{\n  \"z\": 1,\n  \"a\": {\n    \"s\": \"x, {y}: \\\"q\\\"\",\n    \"e\": [],\n    \"o\": {}\n  },\n  \"l\": [\n    1,\n    2\n  ]\n}";
        assert_eq!(pretty_json(json), pretty);
    }

    #[test]
    fn a_match_in_a_long_line_shows_around_it() {
        let line = format!("{}needle{}", "a".repeat(5000), "b".repeat(5000));
        let shown = around(&line, &Regex::new("needle").unwrap());
        assert_eq!(
            shown,
            format!("…{}needle{}…", "a".repeat(WINDOW), "b".repeat(WINDOW))
        );
    }
}
