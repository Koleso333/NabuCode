use std::sync::LazyLock;
use std::time::Duration;

use super::Signals;

/// A desktop browser's name: search engines turn away unknown clients.
pub const AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// The system TLS (schannel on Windows): Brave answers rustls with 429.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
});

/// A GET with browser-like headers.
pub fn get(url: &str) -> reqwest::RequestBuilder {
    CLIENT
        .get(url)
        .header(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("accept-language", "en-US,en;q=0.9")
}

/// Runs `work` until it ends, the call is cancelled or the person sends a message.
pub async fn until_stopped<T>(
    signals: &Signals,
    work: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        out = work => out,
        _ = signals.cancel.cancelled() => Err("cancelled".into()),
        _ = signals.wrap_up.cancelled() => Err("stopped: the person sent you a message meanwhile".into()),
    }
}

/// `text` cut to `max` characters, with `…` when cut.
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// Whitespace runs as single spaces.
pub fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
