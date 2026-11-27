//! The GUI's browser panel: tabs of a chat, their screencast and the user's input.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use nabu_protocol::{BrowserTab, Event};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use super::tab::Tab;
use super::{CHATS, act, browser};

/// Close code of a tab that is gone: reconnecting is useless.
const GONE: u16 = 4404;
const ICON_BYTES: usize = 64 * 1024;

static EVENTS: OnceLock<broadcast::Sender<Event>> = OnceLock::new();

/// Where `BrowserTabsChanged` goes.
pub fn send_events_to(events: broadcast::Sender<Event>) {
    let _ = EVENTS.set(events);
}

/// Tells the GUI that the chat's tabs changed.
pub(super) fn announce(chat: &str) {
    if let Some(events) = EVENTS.get().filter(|e| e.receiver_count() > 0) {
        let _ = events.send(Event::BrowserTabsChanged { session_id: chat.to_owned(), tabs: tabs_of(chat) });
    }
}

pub fn tabs_of(chat: &str) -> Vec<BrowserTab> {
    let mut chats = CHATS.lock().unwrap();
    let Some(chat) = chats.get_mut(chat) else { return Vec::new() };
    chat.tabs.retain(|(_, tab)| !tab.is_closed());
    chat.tabs.iter().filter(|(_, tab)| !tab.in_chrome).map(|(number, tab)| tab.info(*number)).collect()
}

fn tab_by_id(id: &str) -> Result<Arc<Tab>, String> {
    let chats = CHATS.lock().unwrap();
    let tab = chats.values().flat_map(|c| &c.tabs).find(|(_, tab)| tab.target == id && !tab.is_closed());
    tab.map(|(_, tab)| tab.clone()).ok_or_else(|| "the tab is closed".to_owned())
}

/// Opens a tab of the chat; it starts loading `url` after the reply.
pub async fn open(chat: &str, url: &str) -> Result<BrowserTab, String> {
    let (number, tab) = browser().await?.new_tab(chat).await?;
    let info = tab.info(number);
    if !url.trim().is_empty() {
        let url = url.to_owned();
        tokio::spawn(async move { act::goto(&tab, &url).await });
    }
    Ok(info)
}

/// What the user typed in the address row: an address, or else a web search. The panel shows the loading,
/// and a page that fails to open shows the browser's own error page, so the reply does not wait for either.
/// `search` is the search's address with `%s` for the words.
pub async fn go(id: &str, input: &str, search: Option<&str>) -> Result<(), String> {
    let url = if is_address(input.trim()) { act::full_url(input) } else { search_url(input.trim(), search) };
    let tab = tab_by_id(id)?;
    tokio::spawn(async move { tab.call("Page.navigate", json!({ "url": url })).await });
    Ok(())
}

/// An address has a scheme, a local host, a port, or a dotted host without spaces, like the omnibox takes it.
fn is_address(input: &str) -> bool {
    if input.contains("://") || ["about:", "data:", "file:"].iter().any(|s| input.starts_with(s)) {
        return true;
    }
    if input.is_empty() || input.contains(char::is_whitespace) {
        return false;
    }
    let host = input.split(['/', '?', '#']).next().unwrap_or_default();
    let (name, port) = host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).unwrap_or((host, ""));
    let top = name.rsplit_once('.').map(|(rest, top)| !rest.is_empty() && top.chars().count() >= 2 && top.chars().all(char::is_alphanumeric));
    name == "localhost" || name.starts_with('[') || name.parse::<std::net::Ipv4Addr>().is_ok() || !port.is_empty() || top == Some(true)
}

// Google answers a headless browser with a captcha now and then, DuckDuckGo does not
fn search_url(query: &str, search: Option<&str>) -> String {
    let query = percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC).to_string();
    search.filter(|s| s.contains("%s")).unwrap_or("https://duckduckgo.com/?q=%s").replacen("%s", &query, 1)
}

pub async fn history(id: &str, step: i64) -> Result<(), String> {
    act::history(&*tab_by_id(id)?, step).await
}

pub async fn reload(id: &str) -> Result<(), String> {
    tab_by_id(id)?.call("Page.reload", json!({})).await.map(drop)
}

pub async fn close(id: &str) -> Result<(), String> {
    let tab = tab_by_id(id)?;
    tab.browser_call("Target.closeTarget", json!({ "targetId": tab.target })).await?;
    tab.state.lock().unwrap().closed = true;
    announce(&tab.chat);
    Ok(())
}

/// Looks up the page's icon after a load and keeps it as a `data:` URL.
pub(super) async fn find_icon(tab: Arc<Tab>) {
    let expression = "(() => { const link = [...document.querySelectorAll('link[rel~=\"icon\" i]')].pop(); \
if (link?.href) return link.href; return /^https?:$/.test(location.protocol) ? location.origin + '/favicon.ico' : ''; })()";
    let found = tab.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true })).await;
    let source = found.ok().and_then(|v| v["result"]["value"].as_str().map(str::to_owned)).unwrap_or_default();
    if tab.state.lock().unwrap().icon_source.as_deref() == Some(source.as_str()) {
        return;
    }
    let favicon = match source.as_str() {
        "" => None,
        data if data.starts_with("data:image/") => Some(data.to_owned()).filter(|d| d.len() <= ICON_BYTES * 2),
        url => download_icon(url).await,
    };
    {
        let mut state = tab.state.lock().unwrap();
        state.icon_source = Some(source);
        state.favicon = favicon;
    }
    announce(&tab.chat);
}

async fn download_icon(url: &str) -> Option<String> {
    let response = crate::tools::web::get(url).timeout(Duration::from_secs(10)).send().await.ok()?.error_for_status().ok()?;
    let kind = response.headers().get("content-type")?.to_str().ok()?.split(';').next()?.trim().to_owned();
    let bytes = response.bytes().await.ok()?;
    let kind = match kind.as_str() {
        "image/x-icon" | "image/vnd.microsoft.icon" | "application/octet-stream" if bytes.starts_with(&[0, 0, 1, 0]) => "image/x-icon".to_owned(),
        image if image.starts_with("image/") => kind,
        _ => return None,
    };
    (bytes.len() <= ICON_BYTES).then(|| format!("data:{kind};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes)))
}

/// The panel's socket of one tab. Out: `{"width","height"}` text when the page area in a frame changes, then PNG frames.
/// In: `{"do":"size","width","height","scale"}` (CSS pixels and device pixel ratio: the page takes this size), `{"do":"mouse"|"key", ...}` with the params of
/// `Input.dispatchMouseEvent` / `Input.dispatchKeyEvent`, `{"do":"text","text"}` and `{"do":"front"}`.
pub async fn stream(socket: WebSocket, id: String) {
    let (mut sink, mut incoming) = socket.split();
    let Ok(tab) = tab_by_id(&id) else {
        let _ = sink.send(close_message(GONE)).await;
        return;
    };
    let mut frames = tab.frames.subscribe();
    *tab.watchers.lock().unwrap() += 1;
    let _ = tab.call("Page.bringToFront", json!({})).await;
    // Input runs apart from frames: Chrome answers it only after the page handled it.
    let (input, mut queue) = mpsc::unbounded_channel::<Value>();
    let worker = tab.clone();
    let inputs = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            let _ = take_input(&worker, message).await;
        }
    });
    let mut area = (0.0, 0.0);
    // Frame size asked for, and whether the screencast runs at it
    let (mut size, mut casting) = (None, false);
    let mut check = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if area != (frame.width, frame.height) {
                        area = (frame.width, frame.height);
                        let size = json!({ "width": frame.width, "height": frame.height }).to_string();
                        if sink.send(Message::Text(size.into())).await.is_err() { break }
                    }
                    if sink.send(Message::Binary(frame.image.clone().into())).await.is_err() { break }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            message = incoming.next() => match message {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(message) if message["do"] == "size" => {
                        let (width, height) = (message["width"].as_f64().unwrap_or(1280.0), message["height"].as_f64().unwrap_or(800.0));
                        let scale = message["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
                        tab.state.lock().unwrap().panel_size = Some((width, height, scale));
                        let _ = tab.fit_panel().await;
                        size = Some(((width * scale).ceil() as u64, (height * scale).ceil() as u64));
                        casting = cast(&tab, size).await;
                    }
                    Ok(message) => { let _ = input.send(message); }
                    Err(_) => {}
                },
                Some(Ok(_)) => {}
                _ => break,
            },
            _ = check.tick() => {
                if tab.is_closed() {
                    let _ = sink.send(close_message(GONE)).await;
                    break;
                }
                if !casting {
                    casting = cast(&tab, size).await;
                }
            }
        }
    }
    inputs.abort();
    let last = {
        let mut watchers = tab.watchers.lock().unwrap();
        *watchers -= 1;
        *watchers == 0
    };
    if last && !tab.is_closed() {
        let _ = tab.call("Page.stopScreencast", json!({})).await;
    }
}

/// (Re)starts the screencast at `size`, which also sends a frame at once. A tab still opening its first page refuses it.
async fn cast(tab: &Tab, size: Option<(u64, u64)>) -> bool {
    let Some((width, height)) = size else { return false };
    let _ = tab.call("Page.stopScreencast", json!({})).await;
    let params = json!({ "format": "png", "maxWidth": width, "maxHeight": height });
    tab.call("Page.startScreencast", params).await.is_ok()
}

async fn take_input(tab: &Tab, mut message: Value) -> Result<(), String> {
    let action = message["do"].as_str().unwrap_or_default().to_owned();
    if let Some(fields) = message.as_object_mut() {
        fields.remove("do");
    }
    match action.as_str() {
        "mouse" => tab.call("Input.dispatchMouseEvent", message).await.map(drop),
        "key" => tab.call("Input.dispatchKeyEvent", message).await.map(drop),
        "text" => tab.call("Input.insertText", message).await.map(drop),
        "front" => tab.call("Page.bringToFront", json!({})).await.map(drop),
        _ => Ok(()),
    }
}

fn close_message(code: u16) -> Message {
    Message::Close(Some(CloseFrame { code, reason: "".into() }))
}

#[cfg(test)]
mod tests {
    use super::is_address;

    #[test]
    fn address_or_search() {
        for address in ["example.com", "ку.рф/путь", "localhost:3000", "127.0.0.1", "my-pc:8080", "https://ку", "about:blank", "[::1]:5173"] {
            assert!(is_address(address), "{address}");
        }
        for query in ["ку", "как дела", "rust traits", "a.b c", "file", "v1.2"] {
            assert!(!is_address(query), "{query}");
        }
    }
}
