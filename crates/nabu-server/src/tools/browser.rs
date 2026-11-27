use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::web::{AGENT, get};

const VERSIONS: &str = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
/// Virtual time the page's scripts get before the DOM is taken.
const SCRIPTS_MS: u32 = 8000;
const RENDER_TIMEOUT: Duration = Duration::from_secs(40);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

static HOME: OnceLock<PathBuf> = OnceLock::new();
static DOWNLOADING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Where a downloaded browser goes: `<data>/browser`.
pub fn set_home(data_dir: &Path) {
    let _ = HOME.set(data_dir.join("browser"));
}

/// The folder of the downloaded browser and the tools' browser profile.
pub(super) fn home() -> Option<&'static Path> {
    HOME.get().map(PathBuf::as_path)
}

/// The page's HTML after its scripts ran, and whether a browser had to be downloaded for it.
pub async fn render(url: &str) -> Result<(String, bool), String> {
    let (browser, downloaded) = match installed() {
        Some(path) => (path, false),
        None => (download().await?, true),
    };
    let profile = std::env::temp_dir().join(format!("nabucode-render-{}", super::shell::unique()));
    let mut command = Command::new(&browser);
    command
        .args([
            "--headless",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--mute-audio",
        ])
        .arg(format!("--user-agent={AGENT}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--virtual-time-budget={SCRIPTS_MS}"))
        .arg("--dump-dom")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the headless shell is a console program
    let out = tokio::time::timeout(RENDER_TIMEOUT, command.output()).await;
    let _ = tokio::fs::remove_dir_all(&profile).await;
    let out = out.map_err(|_| {
        format!(
            "the browser did not finish in {} s",
            RENDER_TIMEOUT.as_secs()
        )
    })?;
    let out = out.map_err(|e| format!("starting {} failed: {e}", browser.display()))?;
    let html = String::from_utf8_lossy(&out.stdout).into_owned();
    if html.trim().is_empty() {
        return Err(format!("the browser gave no page ({})", out.status));
    }
    Ok((html, downloaded))
}

/// The headless Chrome downloaded before (it starts fastest), else a Chromium-based browser of the user.
pub(super) fn installed() -> Option<PathBuf> {
    shell_path()
        .into_iter()
        .chain(system_browsers())
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn system_browsers() -> Vec<PathBuf> {
    let roots = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].map(std::env::var_os);
    let apps = [
        "Google/Chrome/Application/chrome.exe",
        "Microsoft/Edge/Application/msedge.exe",
        "BraveSoftware/Brave-Browser/Application/brave.exe",
        "Chromium/Application/chrome.exe",
    ];
    apps.iter()
        .flat_map(|app| {
            roots
                .iter()
                .flatten()
                .map(move |root| Path::new(root).join(app))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn system_browsers() -> Vec<PathBuf> {
    let roots = [
        Some(PathBuf::from("/Applications")),
        dirs::home_dir().map(|h| h.join("Applications")),
    ];
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "Brave Browser.app/Contents/MacOS/Brave Browser",
        "Chromium.app/Contents/MacOS/Chromium",
    ];
    apps.iter()
        .flat_map(|app| roots.iter().flatten().map(move |root| root.join(app)))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_browsers() -> Vec<PathBuf> {
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
        "brave-browser",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    names
        .iter()
        .flat_map(|name| std::env::split_paths(&path).map(move |dir| dir.join(name)))
        .collect()
}

/// Chrome for Testing's name for this system.
fn platform() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64" | "aarch64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => Err(format!(
            "no Chrome-based browser found, and there is no headless Chrome to download for {os} {arch}"
        )),
    }
}

fn shell_path() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    };
    Some(
        HOME.get()?
            .join(format!("chrome-headless-shell-{}", platform().ok()?))
            .join(name),
    )
}

/// Downloads chrome-headless-shell from Chrome for Testing, once.
pub(super) async fn download() -> Result<PathBuf, String> {
    let _one = DOWNLOADING.lock().await;
    let target = shell_path().ok_or("no Chrome-based browser found")?;
    if target.is_file() {
        return Ok(target);
    }
    let platform = platform()?;
    let home = HOME.get().ok_or("no data folder")?;
    let versions = get(VERSIONS)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(fail)?
        .text()
        .await
        .map_err(fail)?;
    let versions: Value = serde_json::from_str(&versions).map_err(fail)?;
    let url = versions["channels"]["Stable"]["downloads"]["chrome-headless-shell"]
        .as_array()
        .and_then(|all| all.iter().find(|d| d["platform"] == platform))
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| format!("Chrome for Testing has no headless Chrome for {platform}"))?;
    tokio::fs::create_dir_all(home).await.map_err(fail)?;
    let archive = home.join("download.zip");
    save(url, &archive).await?;
    let unpacked = home.join("unpacking");
    let (from, to) = (archive.clone(), unpacked.clone());
    let done = tokio::task::spawn_blocking(move || unpack(&from, &to))
        .await
        .map_err(fail)?;
    let _ = tokio::fs::remove_file(&archive).await;
    done?;
    let folder = format!("chrome-headless-shell-{platform}");
    let _ = tokio::fs::remove_dir_all(home.join(&folder)).await;
    tokio::fs::rename(unpacked.join(&folder), home.join(&folder))
        .await
        .map_err(fail)?;
    let _ = tokio::fs::remove_dir_all(&unpacked).await;
    target
        .is_file()
        .then_some(target)
        .ok_or_else(|| "the downloaded archive has no chrome-headless-shell".into())
}

async fn save(url: &str, path: &Path) -> Result<(), String> {
    let mut response = get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(fail)?;
    let mut file = tokio::fs::File::create(path).await.map_err(fail)?;
    while let Some(chunk) = response.chunk().await.map_err(fail)? {
        file.write_all(&chunk).await.map_err(fail)?;
    }
    file.flush().await.map_err(fail)
}

fn unpack(archive: &Path, dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    let file = std::fs::File::open(archive).map_err(fail)?;
    zip::ZipArchive::new(file)
        .and_then(|mut zip| zip.extract(dir))
        .map_err(fail)
}

fn fail(e: impl std::fmt::Display) -> String {
    format!("downloading a headless Chrome failed: {e}")
}

#[cfg(test)]
mod tests {
    /// BROWSER_HOME=<dir>: downloads chrome-headless-shell there and renders a page with it.
    #[tokio::test]
    #[ignore]
    async fn downloads_and_renders() {
        super::set_home(std::path::Path::new(
            &std::env::var("BROWSER_HOME").unwrap(),
        ));
        let shell = super::download().await.unwrap();
        println!("{}", shell.display());
        let out = tokio::process::Command::new(&shell)
            .args(["--headless", "--dump-dom", "https://example.com"])
            .output()
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("Example Domain"));
    }
}
