use base64::Engine;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const BROWSER: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const CATALOG_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SUBSETS: [&str; 4] = ["latin", "latin-ext", "cyrillic", "cyrillic-ext"];

// Raw Google Fonts metadata; parsed on the UI side
#[tauri::command]
pub async fn font_catalog() -> Result<String, String> {
    blocking(read_catalog).await
}

// Returns CSS with the font files inlined, so the font works offline once installed
#[tauri::command]
pub async fn font_install(family: String, weights: Vec<u16>) -> Result<String, String> {
    blocking(move || install(&family, &weights)).await
}

pub(crate) async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| error.to_string())?
}

fn fonts_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    let dir = PathBuf::from(base).join("EveryEnd/fonts");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir)
}

fn get(url: &str) -> Result<ureq::Response, String> {
    ureq::get(url).set("User-Agent", BROWSER).timeout(Duration::from_secs(20)).call().map_err(|error| error.to_string())
}

fn read_catalog() -> Result<String, String> {
    let path = fonts_dir()?.join("catalog.json");
    let age = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok().and_then(|time| SystemTime::now().duration_since(time).ok());
    if age.is_some_and(|age| age < CATALOG_AGE) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Ok(text);
        }
    }
    match get("https://fonts.google.com/metadata/fonts").and_then(|response| response.into_string().map_err(|error| error.to_string())) {
        Ok(text) => {
            let _ = std::fs::write(&path, &text);
            Ok(text)
        }
        Err(error) => std::fs::read_to_string(&path).map_err(|_| error),
    }
}

fn install(family: &str, weights: &[u16]) -> Result<String, String> {
    let slug: String = family.chars().map(|char| if char.is_ascii_alphanumeric() { char } else { '-' }).collect();
    let path = fonts_dir()?.join(format!("{slug}.css"));
    if let Ok(css) = std::fs::read_to_string(&path) {
        return Ok(css);
    }
    let mut url = format!("https://fonts.googleapis.com/css2?family={}", family.replace(' ', "+"));
    let used: Vec<String> = weights.iter().filter(|weight| (300..=700).contains(*weight)).map(u16::to_string).collect();
    if !used.is_empty() {
        url += &format!(":wght@{}", used.join(";"));
    }
    let css = get(&url)?.into_string().map_err(|error| error.to_string())?;
    let css = inline_files(&keep_subsets(&css))?;
    std::fs::write(&path, &css).map_err(|error| error.to_string())?;
    Ok(css)
}

// Google splits a font into one block per script; only latin and cyrillic are needed
fn keep_subsets(css: &str) -> String {
    if !css.contains("/* ") {
        return css.to_owned();
    }
    css.split("/* ")
        .filter_map(|block| {
            let (subset, rule) = block.split_once(" */")?;
            SUBSETS.contains(&subset).then_some(rule)
        })
        .collect()
}

fn inline_files(css: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut rest = css;
    while let Some(start) = rest.find("url(") {
        let end = rest[start..].find(')').ok_or("Bad font CSS")? + start;
        let url = &rest[start + 4..end];
        let mut bytes = Vec::new();
        get(url)?.into_reader().read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        let kind = if url.ends_with(".woff2") { "woff2" } else { "ttf" };
        result += &rest[..start];
        result += &format!("url(data:font/{kind};base64,{})", base64::engine::general_purpose::STANDARD.encode(bytes));
        rest = &rest[end + 1..];
    }
    Ok(result + rest)
}
