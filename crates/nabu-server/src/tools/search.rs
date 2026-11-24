use std::collections::HashMap;

use futures_util::future::join_all;
use ncore::core::ir::ToolDef;
use scraper::{ElementRef, Html, Selector};
use serde_json::{Value, json};

use super::web::{clip, get, squeeze};
use super::{num_arg, opt_str, str_list};

pub const BRIEF: &str =
    "search the web (Brave and Yahoo): titles, URLs, snippets and dates of the results";

const LIMIT: usize = 8;
const LIMIT_MAX: usize = 20;
const QUERIES_MAX: usize = 5;
const SNIPPET: usize = 300;

pub fn def() -> ToolDef {
    super::def(
        "search",
        "Searches the web with Brave and Yahoo at once and merges their results: pages both engines found come first. \
Each result has a title, the URL, a snippet and, when the engine shows one, a date. \
Several queries in one call run in parallel, so phrase a question a few different ways or ask several questions at once. \
Results are only snippets: read a page with `fetch` before relying on details from it. \
Engines are web search, not an index of code: name the library, version or error message as text you expect on the page.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Up to 5 queries; they run in parallel."
                },
                "limit": { "type": "integer", "description": "Results per query, 1 to 20. Default 8." },
                "site": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Only these domains, like [\"docs.rs\"] or [\"github.com\", \"stackoverflow.com\"]."
                },
                "exclude": {
                    "type": "array", "items": { "type": "string" },
                    "description": "Leave out these domains."
                },
                "fresh": {
                    "type": "string", "enum": ["day", "week", "month", "year"],
                    "description": "Only pages from the last day, week, month or year."
                }
            },
            "required": ["query"]
        }),
    )
}

#[derive(Clone)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
    date: Option<String>,
}

/// What one engine returned for one query.
type Answer = Result<Vec<Hit>, String>;

pub async fn run(args: Value) -> Result<String, String> {
    let queries = str_list(&args, "query");
    if queries.is_empty() {
        return Err("missing `query`".into());
    }
    if queries.len() > QUERIES_MAX {
        return Err(format!(
            "{} queries given; at most {QUERIES_MAX} per call",
            queries.len()
        ));
    }
    let limit = num_arg(&args, "limit").unwrap_or(LIMIT).clamp(1, LIMIT_MAX);
    let fresh = opt_str(&args, "fresh");
    if let Some(f) = fresh.filter(|f| !["day", "week", "month", "year"].contains(f)) {
        return Err(format!("`fresh` is \"{f}\"; use day, week, month or year"));
    }
    let filters = filters(&str_list(&args, "site"), &str_list(&args, "exclude"));
    let filters = &filters;
    let blocks = join_all(queries.iter().map(|query| async move {
        let full = format!("{query}{filters}");
        let (brave, yahoo) = tokio::join!(brave(&full, fresh), yahoo(&full, fresh));
        block(query, &[("Brave", brave), ("Yahoo", yahoo)], limit)
    }))
    .await;
    Ok(blocks.join("\n\n"))
}

/// Query operators both engines understand.
fn filters(sites: &[String], excluded: &[String]) -> String {
    let mut out = String::new();
    let sites: Vec<String> = sites.iter().map(|s| format!("site:{s}")).collect();
    match sites.len() {
        0 => {}
        1 => out.push_str(&format!(" {}", sites[0])),
        _ => out.push_str(&format!(" ({})", sites.join(" OR "))),
    }
    for site in excluded {
        out.push_str(&format!(" -site:{site}"));
    }
    out
}

/// One query's merged results, or why there are none.
fn block(query: &str, answers: &[(&str, Answer)], limit: usize) -> String {
    let counts: Vec<String> = answers
        .iter()
        .map(|(engine, answer)| match answer {
            Ok(hits) => format!("{engine} {}", hits.len()),
            Err(e) => format!("{engine} failed: {e}"),
        })
        .collect();
    let lists: Vec<&[Hit]> = answers
        .iter()
        .filter_map(|(_, a)| a.as_deref().ok())
        .collect();
    let merged = merge(&lists);
    let head = format!("\"{query}\" ({})", counts.join("; "));
    if merged.is_empty() {
        return format!("{head}\nno results");
    }
    let shown: Vec<String> = merged
        .iter()
        .take(limit)
        .enumerate()
        .map(|(i, hit)| line(i + 1, hit))
        .collect();
    format!("{head}\n{}", shown.join("\n"))
}

fn line(n: usize, hit: &Hit) -> String {
    let date = hit
        .date
        .as_deref()
        .map(|d| format!("{d} — "))
        .unwrap_or_default();
    format!(
        "{n}. {}\n   {}\n   {date}{}",
        hit.title,
        hit.url,
        clip(&hit.snippet, SNIPPET)
    )
}

/// Reciprocal rank fusion: a page high in both lists beats one high in a single list.
fn merge(lists: &[&[Hit]]) -> Vec<Hit> {
    // Score, best rank, order first seen (Brave before Yahoo) and the page.
    let mut scores: HashMap<String, (f64, usize, usize, Hit)> = HashMap::new();
    for list in lists {
        for (rank, hit) in list.iter().enumerate() {
            let seen = scores.len();
            let entry = scores
                .entry(key(&hit.url))
                .or_insert((0.0, rank, seen, hit.clone()));
            entry.0 += 1.0 / (60 + rank) as f64;
            entry.1 = entry.1.min(rank);
            if entry.3.date.is_none() {
                entry.3.date = hit.date.clone();
            }
        }
    }
    let mut ranked: Vec<(f64, usize, usize, Hit)> = scores.into_values().collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    ranked.into_iter().map(|(_, _, _, hit)| hit).collect()
}

/// The same page under small URL differences.
fn key(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    url.trim_start_matches("www.")
        .trim_end_matches('/')
        .to_lowercase()
}

async fn brave(query: &str, fresh: Option<&str>) -> Answer {
    let mut request =
        get("https://search.brave.com/search").query(&[("q", query), ("source", "web")]);
    if let Some(fresh) = fresh {
        let age = match fresh {
            "day" => "pd",
            "week" => "pw",
            "month" => "pm",
            _ => "py",
        };
        request = request.query(&[("tf", age)]);
    }
    let html = page(request).await?;
    let hits = parse_brave(&html);
    if hits.is_empty() && html.contains("captcha") {
        return Err("asked for a captcha".into());
    }
    Ok(hits)
}

/// Yahoo serves Bing's index without Bing's bot traps; it shows 7 results a page.
async fn yahoo(query: &str, fresh: Option<&str>) -> Answer {
    let mut request = get("https://search.yahoo.com/search").query(&[("p", query)]);
    // Yahoo has no year filter.
    if let Some(age) = fresh.and_then(|f| f.get(..1)).filter(|a| *a != "y") {
        request = request.query(&[("btf", age)]);
    }
    let html = page(request).await?;
    Ok(parse_yahoo(&html))
}

async fn page(request: reqwest::RequestBuilder) -> Result<String, String> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status();
    if status.as_u16() == 429 {
        return Err("rate limited (429)".into());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    response
        .text()
        .await
        .map_err(|e| format!("reading the page failed: {e}"))
}

fn parse_brave(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let item = selector("div.snippet[data-type=\"web\"]");
    let link = selector("a");
    let title = selector(".title");
    let content = selector(".generic-snippet .content, .snippet-description");
    let date = selector("span.t-secondary");
    let hits = doc.select(&item).filter_map(|div| {
        let url = div.select(&link).next()?.value().attr("href")?.to_owned();
        let title = div.select(&title).next().map(text)?;
        let body = div.select(&content).next();
        let date = body
            .and_then(|b| b.select(&date).next())
            .map(|d| text(d).trim_end_matches(['-', ' ']).to_owned());
        let mut snippet = body.map(text).unwrap_or_default();
        if let Some(d) = &date {
            snippet = snippet
                .trim_start_matches(d.as_str())
                .trim_start_matches([' ', '-'])
                .to_owned();
        }
        Some(Hit {
            title,
            url,
            snippet,
            date,
        })
    });
    distinct(hits)
}

fn parse_yahoo(html: &str) -> Vec<Hit> {
    let doc = Html::parse_document(html);
    let item = selector("div.relsrch");
    let link = selector("div.compTitle a");
    let title = selector("h3");
    let content = selector("div.compText");
    let hits = doc.select(&item).filter_map(|div| {
        let a = div.select(&link).next()?;
        let url = unwrap_yahoo(a.value().attr("href")?);
        let title = a.select(&title).next().map(text)?;
        let snippet = div.select(&content).next().map(text).unwrap_or_default();
        Some(Hit {
            title,
            url,
            snippet,
            date: None,
        })
    });
    distinct(hits)
}

/// Yahoo links go through its redirect; the target is percent-encoded in `/RU=`.
fn unwrap_yahoo(href: &str) -> String {
    let target = href
        .split_once("/RU=")
        .and_then(|(_, rest)| rest.split(['/']).next());
    target
        .and_then(|t| percent_encoding::percent_decode_str(t).decode_utf8().ok())
        .map_or_else(|| href.to_owned(), |t| t.into_owned())
}

/// Web results only, each page once.
fn distinct(hits: impl Iterator<Item = Hit>) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    hits.filter(|hit| hit.url.starts_with("http") && seen.insert(key(&hit.url)))
        .collect()
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

fn text(element: ElementRef) -> String {
    squeeze(&element.text().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(url: &str) -> Hit {
        Hit {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
            date: None,
        }
    }

    #[test]
    fn pages_both_engines_found_come_first() {
        let brave = [
            hit("https://a.com/"),
            hit("https://b.com"),
            hit("https://c.com"),
        ];
        let yahoo = [hit("https://www.c.com"), hit("https://d.com")];
        let urls: Vec<String> = merge(&[&brave, &yahoo])
            .into_iter()
            .map(|h| h.url)
            .collect();
        assert_eq!(
            urls,
            [
                "https://c.com",
                "https://a.com/",
                "https://b.com",
                "https://d.com"
            ]
        );
    }

    #[test]
    fn yahoo_links_are_unwrapped() {
        let href = "https://r.search.yahoo.com/_ylt=A2R;_ylu=Y2/RV=2/RE=17/RO=10/RU=https%3a%2f%2fdocs.rs%2ftokio%2fmacro.select.html/RK=2/RS=OZ-";
        assert_eq!(
            unwrap_yahoo(href),
            "https://docs.rs/tokio/macro.select.html"
        );
    }

    #[tokio::test]
    #[ignore = "talks to Bing and Brave"]
    async fn live() {
        let args = serde_json::json!({"query": ["rust serde flatten enum", "как настроить wireguard на ubuntu"], "limit": 5});
        println!("{}", run(args).await.unwrap());
        let args = serde_json::json!({"query": "tokio select", "site": "docs.rs", "fresh": "year", "limit": 3});
        println!(
            "
{}",
            run(args).await.unwrap()
        );
    }

    #[test]
    fn filters_join_sites() {
        assert_eq!(
            filters(&["a.com".into(), "b.org".into()], &["c.net".into()]),
            " (site:a.com OR site:b.org) -site:c.net"
        );
    }
}
