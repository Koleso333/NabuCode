//! The `browser` group: tabs of a headless Chrome the server runs, and of the user's Chrome through the NabuCode extension;
//! one set of tabs per chat.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use base64::Engine;
use ncore::core::ir::{Block, Media, MediaSource, ToolDef};
use serde_json::{Value, json};

use self::cdp::Chrome;
use self::tab::Tab;
use super::web::clip;
use super::{bool_arg, def, num_arg, opt_str, str_arg};

mod act;
mod cdp;
mod extension;
mod page;
mod panel;
mod tab;

pub use extension::link as chrome_link;
pub use panel::{close as close_tab, go, history, open as open_tab, reload, send_events_to, stream, tabs_of};

pub const BRIEF: &str =
    "control browser tabs: page state with numbered elements, clicks and input, navigation, page text, JavaScript, console and network";

const LOG_SHOWN: usize = 100;
const OUTPUT_CHARS: usize = 40_000;

/// What a browser tool call needs from its session.
pub struct Call {
    /// The chat whose tabs the call sees.
    pub chat: String,
    /// The extension's id of the Chrome the chat works in: tabs open there by default.
    pub chrome: Option<String>,
    pub directory: PathBuf,
    pub args: Value,
}

struct Browser {
    chrome: Arc<Chrome>,
    /// The user's Chrome rather than the headless one.
    in_chrome: bool,
    /// The headless browser's User-Agent without "Headless", which sites turn away.
    agent: Option<String>,
    /// Attached tabs by their DevTools session.
    tabs: Mutex<HashMap<String, Arc<Tab>>>,
}

/// Tabs of each chat, by their numbers.
#[derive(Default)]
struct Chat {
    last: u32,
    tabs: Vec<(u32, Arc<Tab>)>,
}

static BROWSER: tokio::sync::Mutex<Option<Arc<Browser>>> = tokio::sync::Mutex::const_new(None);
/// The user's Chromes connected through the extension, by the extension's id of each, the latest last.
static CHROMES: Mutex<Vec<(String, Arc<Browser>)>> = Mutex::new(Vec::new());
static CHATS: LazyLock<Mutex<HashMap<String, Chat>>> = LazyLock::new(Default::default);

pub fn open_def() -> ToolDef {
    def(
        "browser_open",
        "Opens a URL in a new tab and returns the tab number and the page state. Each chat has its own tabs. \
By default the tab is in NabuCode's built-in browser: the user sees it in NabuCode's browser panel, and all chats share its sign-ins and cookies; \
a chat the user started in Chrome's NabuCode panel opens tabs in that Chrome. \
`browser: \"chrome\"` opens it in the user's own Chrome, with the user's sign-ins, when the NabuCode extension is connected there. \
`take` makes a tab the user has open in Chrome one of yours, by its id from browser_tabs; with `url` it also goes there.",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http, https or file URL; `localhost:3000` works too." },
                "browser": { "type": "string", "enum": ["headless", "chrome"] },
                "take": { "type": "integer", "description": "Id of a tab in the user's Chrome, from browser_tabs." },
                "view": view_schema()
            }
        }),
    )
}

pub fn tabs_def() -> ToolDef {
    def(
        "browser_tabs",
        "Lists your tabs: number, title, URL; when the user's Chrome is connected, also its other tabs, with ids for browser_open's `take`. \
`close` closes your tabs by number; a tab you took from the user is let go and stays open.",
        json!({
            "type": "object",
            "properties": {
                "close": { "type": "array", "items": { "type": "integer" }, "description": "Tab numbers to close." }
            }
        }),
    )
}

pub fn state_def() -> ToolDef {
    def(
        "browser_state",
        "Returns the page state of a tab: one line per element, nested by indentation, like `[2] textbox \"Email\" focused`. \
Elements you can act on are numbered `[N]`, and an element keeps its number while it stays on the page. \
When the page replaces elements (a re-render, a reload, a navigation), a new element takes a gone one's number \
if its role and name match and it stands in the same place among elements with that role and name; other new elements get new numbers. \
After the first call a tab's state comes as the changes since the last state you got (`-` and `+` lines); \
`~` marks an element whose line changed while it kept its number. The header says which element has focus (`focus [N]`); lines do not. \
A state of up to 40 lines comes whole, with its changes marked. \
`full: true` gives the whole page. The header always has the tab's title and URL, and says when the page loaded again or went to another URL. \
Long states are cut at 400 lines, and the cut is noted. The header notes new tabs the page opened and open dialogs.",
        json!({
            "type": "object",
            "properties": {
                "tab": { "type": "integer", "description": "Tab number." },
                "view": view_schema(),
                "full": { "type": "boolean", "description": "The whole state instead of the changes." },
                "find": { "type": "string", "description": "Regex, case-insensitive: only matching lines, with the lines they sit in." }
            },
            "required": ["tab"]
        }),
    )
}

pub fn act_def() -> ToolDef {
    def(
        "browser_act",
        "Runs actions in a tab in order and returns its new state, as `browser_state` gives it, once the page settles: \
up to 3 s for navigation, requests and DOM changes. Stops at the first action that fails and says which one and why; \
the actions before it stay done. A target is an element number from the state, or a point [x, y] in screenshot pixels. \
A number is looked up when its action runs: after a reload or navigation earlier in the list it points at the element \
that took it on the new page. After a reload or navigation the state comes whole.\n\
Actions, by `do`:\n\
- click: target, button (left/right/middle), count (2 for a double click), keys (modifiers held, like \"ctrl\" or \"shift\")\n\
- hover: target\n\
- type: text, target (optional; else into the focused element). Adds to what the field has.\n\
- fill: target, value. Replaces a field's value; true/false for a checkbox; an option's text for a select.\n\
- key: keys, like \"Enter\", \"ctrl+a\", \"shift+Tab\"\n\
- scroll: target and/or dy, dx in pixels; a target without dy is scrolled into view\n\
- drag: from, to\n\
- upload: target (a file field), paths\n\
- dialog: accept (true/false), text (for a prompt) — answers an open alert, confirm or prompt\n\
- goto: url\n\
- back, forward, reload\n\
- viewport: width, height in pixels, or width \"mobile\" for a 390×844 touch screen; fixes the page size. \
Otherwise a tab the user watches in their browser panel takes the panel's size; width \"auto\" gives the size back to it",
        json!({
            "type": "object",
            "properties": {
                "tab": { "type": "integer", "description": "Tab number." },
                "actions": {
                    "type": "array",
                    "description": "Actions to run in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "do": { "type": "string", "enum": ["click", "hover", "type", "fill", "key", "scroll", "drag", "upload", "dialog", "goto", "back", "forward", "reload", "viewport"] },
                            "target": { "description": "Element number, or [x, y]." },
                            "text": { "type": "string" },
                            "value": { "description": "Field value: text, true/false, or an option's text." },
                            "keys": { "description": "Key combo, like \"ctrl+a\"; for click, the modifiers held." },
                            "button": { "type": "string", "enum": ["left", "right", "middle"] },
                            "count": { "type": "integer" },
                            "dx": { "type": "number" },
                            "dy": { "type": "number" },
                            "from": { "description": "Element number, or [x, y]." },
                            "to": { "description": "Element number, or [x, y]." },
                            "paths": { "type": "array", "items": { "type": "string" } },
                            "accept": { "type": "boolean" },
                            "url": { "type": "string" },
                            "width": { "description": "Pixels, or \"mobile\"." },
                            "height": { "type": "integer" }
                        },
                        "required": ["do"]
                    }
                },
                "view": view_schema(),
                "full": { "type": "boolean", "description": "The whole state instead of the changes." }
            },
            "required": ["tab", "actions"]
        }),
    )
}

pub fn read_def() -> ToolDef {
    def(
        "browser_read",
        "Reads the tab's page as Markdown with numbered lines, as `fetch` does, but the page as it is in the tab now: \
with its sign-ins and after your actions. Like `fetch` it keeps the main text: `<main>`, else the longest `<article>`, else the body, \
without navigation, footers, forms and buttons; the state shows those. A long page comes in parts: the first call shows its outline and the first 300 lines; \
continue with `offset`, or use `find`.",
        json!({
            "type": "object",
            "properties": {
                "tab": { "type": "integer", "description": "Tab number." },
                "offset": { "type": "integer", "description": "First line to show (1-based). Default 1." },
                "limit": { "type": "integer", "description": "Lines to show, up to 2000. Default 300." },
                "find": { "type": "string", "description": "Regex, case-insensitive: only matching lines, each with 2 lines around it." }
            },
            "required": ["tab"]
        }),
    )
}

pub fn js_def() -> ToolDef {
    def(
        "browser_js",
        "Runs JavaScript in the tab's page and returns the value of its last expression as JSON; `await` works at the top level. \
Errors come back with their message and stack.",
        json!({
            "type": "object",
            "properties": {
                "tab": { "type": "integer", "description": "Tab number." },
                "code": { "type": "string", "description": "JavaScript." }
            },
            "required": ["tab", "code"]
        }),
    )
}

pub fn logs_def() -> ToolDef {
    def(
        "browser_logs",
        "Console messages or network requests of the tab since it opened, newest last, up to 100 lines. \
Requests are numbered `#N`; `body` with a request number returns that response's body.",
        json!({
            "type": "object",
            "properties": {
                "tab": { "type": "integer", "description": "Tab number." },
                "kind": { "type": "string", "enum": ["console", "network"], "description": "Default \"console\"." },
                "find": { "type": "string", "description": "Regex, case-insensitive." },
                "errors": { "type": "boolean", "description": "Only errors and failed requests." },
                "body": { "type": "integer", "description": "Request number to get the response body of." }
            },
            "required": ["tab"]
        }),
    )
}

fn view_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["text", "screenshot", "both"],
        "description": "\"text\" (default): the state as text; \"screenshot\": a JPEG of the visible area; \"both\"."
    })
}

/// Closes the tabs of a chat that ended; the tabs in the user's Chrome stay open for the user.
pub fn close_chat(chat: &str) {
    let Some(tabs) = CHATS.lock().unwrap().remove(chat) else { return };
    panel::announce(chat);
    let chat = tabs;
    tokio::spawn(async move {
        for (_, tab) in chat.tabs {
            let _ = if tab.in_chrome { tab.let_go().await } else { tab.close().await };
        }
    });
}

// Marks in the profile: a site was opened since the cache or the whole profile was cleared.
// Chrome's own files can't tell it: it makes empty cookie, history and storage databases on start.
const CACHE_MARK: &str = "nabu-cache";
const SITES_MARK: &str = "nabu-sites";

fn profile() -> Option<PathBuf> {
    Some(super::browser::home()?.join("profile"))
}

pub fn stored() -> nabu_protocol::BrowserStored {
    let marked = |mark| profile().is_some_and(|p| p.join(mark).exists());
    nabu_protocol::BrowserStored { cache: marked(CACHE_MARK), sites: marked(SITES_MARK) }
}

fn mark_stored() {
    let Some(profile) = profile() else { return };
    for mark in [CACHE_MARK, SITES_MARK] {
        if !profile.join(mark).exists() {
            let _ = std::fs::write(profile.join(mark), "");
        }
    }
}

/// Clears the browser's cache: through a page of the running browser, else from the profile on disk.
pub async fn clear_cache() -> Result<(), String> {
    if let Some(profile) = profile() {
        let _ = std::fs::remove_file(profile.join(CACHE_MARK));
    }
    let running = BROWSER.lock().await.as_ref().filter(|b| !b.chrome.is_closed()).cloned();
    let Some(browser) = running else {
        let profile = profile().ok_or("no data folder")?.join("Default");
        for folder in ["Cache", "Code Cache", "GPUCache"] {
            let _ = std::fs::remove_dir_all(profile.join(folder));
        }
        return Ok(());
    };
    // Clearing the cache takes a page session; a page without an opener is no tab of any chat
    let created = browser.chrome.call(None, "Target.createTarget", json!({ "url": "about:blank", "background": true })).await?;
    let target = created["targetId"].as_str().ok_or("the browser opened no page")?.to_owned();
    let attached = browser.chrome.call(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true })).await;
    let cleared = match attached {
        Ok(attached) => browser.chrome.call(attached["sessionId"].as_str(), "Network.clearBrowserCache", json!({})).await.map(drop),
        Err(e) => Err(e),
    };
    let _ = browser.chrome.call(None, "Target.closeTarget", json!({ "targetId": target })).await;
    cleared
}

/// Removes cookies, site storage, cache and history: closes the headless browser with every chat's tabs in it and deletes its profile.
pub async fn clear_site_data() -> Result<(), String> {
    let mut slot = BROWSER.lock().await;
    let chats: Vec<String> = CHATS.lock().unwrap().keys().cloned().collect();
    for chat in chats {
        if let Some(tabs) = CHATS.lock().unwrap().get_mut(&chat) {
            tabs.tabs.iter().filter(|(_, tab)| !tab.in_chrome).for_each(|(_, tab)| tab.state.lock().unwrap().closed = true);
            tabs.tabs.retain(|(_, tab)| tab.in_chrome);
        }
        panel::announce(&chat);
    }
    if let Some(browser) = slot.take() {
        let _ = browser.chrome.call(None, "Browser.close", json!({})).await;
        for _ in 0..50 {
            if browser.chrome.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let profile = super::browser::home().ok_or("no data folder")?.join("profile");
    // Chrome's helper processes may hold files for a moment after it exits
    for attempt in 0.. {
        match std::fs::remove_dir_all(&profile) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt == 30 => return Err(format!("removing {} failed: {e}", profile.display())),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    Ok(())
}

/// The running browser, started when the first tool needs it.
async fn browser() -> Result<Arc<Browser>, String> {
    let mut slot = BROWSER.lock().await;
    if let Some(browser) = slot.as_ref().filter(|b| !b.chrome.is_closed()) {
        return Ok(browser.clone());
    }
    let binary = match super::browser::installed() {
        Some(path) => path,
        None => super::browser::download().await?,
    };
    let profile = super::browser::home().ok_or("no data folder")?.join("profile");
    std::fs::create_dir_all(&profile).map_err(|e| format!("creating {} failed: {e}", profile.display()))?;
    let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
    let chrome = Chrome::start(&binary, &profile, events).await?;
    let browser = Browser::start(chrome, false, incoming).await?;
    *slot = Some(browser.clone());
    Ok(browser)
}

/// Whether the extension in the user's Chrome is connected now.
pub fn chrome_connected() -> bool {
    CHROMES.lock().unwrap().iter().any(|(_, b)| !b.chrome.is_closed())
}

/// The Chrome the chat works in, else the one connected last.
fn chrome(own: Option<&str>) -> Result<Arc<Browser>, String> {
    let mut chromes = CHROMES.lock().unwrap();
    chromes.retain(|(_, b)| !b.chrome.is_closed());
    let found = match own {
        Some(own) => chromes.iter().rev().find(|(id, _)| id == own),
        None => chromes.last(),
    };
    found.map(|(_, b)| b.clone()).ok_or_else(|| match own {
        Some(_) => "the Chrome this chat works in is not connected now".into(),
        None => "the user's Chrome is not connected: the NabuCode extension is not installed there, or Chrome is closed".into(),
    })
}

impl Browser {
    /// Starts watching the browser's tabs; its events come through `incoming`.
    async fn start(chrome: Arc<Chrome>, in_chrome: bool, mut incoming: cdp::Incoming) -> Result<Arc<Self>, String> {
        let version = chrome.call(None, "Browser.getVersion", json!({})).await?;
        let agent = version["userAgent"].as_str().unwrap_or_default().replace("HeadlessChrome", "Chrome");
        chrome.call(None, "Target.setDiscoverTargets", json!({ "discover": true })).await?;
        let browser = Arc::new(Browser { chrome, in_chrome, agent: (!in_chrome).then_some(agent), tabs: Mutex::default() });
        let events_of = browser.clone();
        tokio::spawn(async move {
            while let Some((session, method, params)) = incoming.recv().await {
                events_of.event(session.as_deref(), &method, &params);
            }
        });
        Ok(browser)
    }

    fn event(self: &Arc<Self>, session: Option<&str>, method: &str, params: &Value) {
        if let Some(session) = session {
            let tab = self.tabs.lock().unwrap().get(session).cloned();
            if let Some(tab) = tab
                && tab.event(method, params)
            {
                if method == "Page.frameStoppedLoading" {
                    if !self.in_chrome && tab.state.lock().unwrap().url.starts_with("http") {
                        mark_stored();
                    }
                    tokio::spawn(panel::find_icon(tab.clone()));
                }
                panel::announce(&tab.chat);
            }
            return;
        }
        let info = &params["targetInfo"];
        match method {
            "Target.targetCreated" if info["type"] == "page" => {
                let opener = info["openerId"].as_str().and_then(|id| self.tab_by_target(id));
                if let (Some(opener), Some(target)) = (opener, info["targetId"].as_str()) {
                    opener.state.lock().unwrap().opening += 1;
                    let (this, target) = (self.clone(), target.to_owned());
                    tokio::spawn(async move { this.adopt(&opener, &target).await });
                }
            }
            "Target.targetInfoChanged" => {
                if let Some(tab) = info["targetId"].as_str().and_then(|id| self.tab_by_target(id)) {
                    let mut state = tab.state.lock().unwrap();
                    if let Some(url) = info["url"].as_str().filter(|u| !u.is_empty()) {
                        state.url = url.to_owned();
                    }
                    drop(state);
                    panel::announce(&tab.chat);
                }
            }
            "Target.targetDestroyed" | "Target.detachedFromTarget" => {
                let tab = match params["sessionId"].as_str() {
                    Some(session) => self.tabs.lock().unwrap().remove(session),
                    None => params["targetId"].as_str().and_then(|id| self.tab_by_target(id)),
                };
                if let Some(tab) = tab {
                    tab.state.lock().unwrap().closed = true;
                    panel::announce(&tab.chat);
                }
            }
            _ => {}
        }
    }

    fn tab_by_target(&self, target: &str) -> Option<Arc<Tab>> {
        self.tabs.lock().unwrap().values().find(|t| t.target == target).cloned()
    }

    /// Opens an empty tab of the chat in a window of its own, so every tab draws, not only the one in front.
    async fn new_tab(&self, chat: &str) -> Result<(u32, Arc<Tab>), String> {
        let created = self.chrome.call(None, "Target.createTarget", json!({ "url": "about:blank", "newWindow": true })).await?;
        let target = created["targetId"].as_str().ok_or("the browser opened no tab")?;
        self.attach(chat, target, false).await
    }

    /// Attaches a tab and gives it the next number in its chat.
    async fn attach(&self, chat: &str, target: &str, taken: bool) -> Result<(u32, Arc<Tab>), String> {
        let tab = Tab::attach(self.chrome.clone(), chat, target, self.in_chrome, taken).await?;
        self.tabs.lock().unwrap().insert(tab.session.clone(), tab.clone());
        tab.start(self.agent.as_deref()).await?;
        let number = {
            let mut chats = CHATS.lock().unwrap();
            let chat = chats.entry(chat.to_owned()).or_default();
            chat.last += 1;
            chat.tabs.push((chat.last, tab.clone()));
            chat.last
        };
        panel::announce(chat);
        Ok((number, tab))
    }

    /// A tab the page opened joins its opener's chat, and the opener's next state says so.
    async fn adopt(&self, opener: &Tab, target: &str) {
        // A tab behind others does not answer `Page.enable`, so the new tab starts while it is in front.
        // Then the opener goes back to front: its input waits for a frame only the front tab draws.
        let attached = self.attach(&opener.chat, target, false).await;
        let _ = opener.call("Page.bringToFront", json!({})).await;
        let note = match attached {
            Ok((number, tab)) => {
                // The note names where the tab goes, once it starts going there.
                for _ in 0..10 {
                    if !matches!(tab.state.lock().unwrap().url.as_str(), "" | "about:blank") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let url = tab.state.lock().unwrap().url.clone();
                format!("the page opened tab {number}: {url}")
            }
            Err(e) => format!("the page opened a tab, but attaching to it failed: {e}"),
        };
        let mut state = opener.state.lock().unwrap();
        state.notes.push(note);
        state.opening -= 1;
    }
}

/// The chat's tab by number.
fn tab_of(chat: &str, args: &Value) -> Result<(u32, Arc<Tab>), String> {
    let number = num_arg(args, "tab").ok_or("missing `tab`: a tab number from browser_open or browser_tabs")? as u32;
    let mut chats = CHATS.lock().unwrap();
    let chat = chats.entry(chat.to_owned()).or_default();
    chat.tabs.retain(|(_, tab)| !tab.is_closed());
    if let Some((_, tab)) = chat.tabs.iter().find(|(n, _)| *n == number) {
        return Ok((number, tab.clone()));
    }
    let open: Vec<String> = chat.tabs.iter().map(|(n, _)| n.to_string()).collect();
    Err(match open.is_empty() {
        true => format!("there is no tab {number}: no tabs are open; open one with browser_open"),
        false => format!("there is no tab {number}; open tabs: {}", open.join(", ")),
    })
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Text,
    Screenshot,
    Both,
}

fn view(args: &Value) -> Result<View, String> {
    match opt_str(args, "view").unwrap_or("text") {
        "text" => Ok(View::Text),
        "screenshot" => Ok(View::Screenshot),
        "both" => Ok(View::Both),
        other => Err(format!("`view` is text, screenshot or both, not `{other}`")),
    }
}

pub async fn open(call: Call) -> Result<Vec<Block>, String> {
    let url = opt_str(&call.args, "url");
    let view = view(&call.args)?;
    let own = call.chrome.as_deref();
    let (number, tab) = match (num_arg(&call.args, "take"), url) {
        (Some(id), _) => take(&call.chat, own, &id.to_string()).await?,
        (None, None) => return Err("missing `url`".into()),
        (None, Some(_)) => match opt_str(&call.args, "browser").unwrap_or(if own.is_some() { "chrome" } else { "headless" }) {
            "headless" => browser().await?.new_tab(&call.chat).await?,
            "chrome" => chrome(own)?.new_tab(&call.chat).await?,
            other => return Err(format!("`browser` is headless or chrome, not `{other}`")),
        },
    };
    let _busy = tab.busy.lock().await;
    let opened = match url {
        Some(url) => tab.unless_dialog(act::goto(&tab, url)).await.map(drop),
        None => Ok(()),
    };
    tab.settle().await;
    let state = state(&tab, number, view, true, None).await?;
    match opened {
        Ok(()) => Ok(state),
        Err(e) => Err(format!("{e}\n\n{}", text_of(&state))),
    }
}

/// Makes a tab of the user's Chrome one of the chat's tabs, or finds it among them.
async fn take(chat: &str, own: Option<&str>, target: &str) -> Result<(u32, Arc<Tab>), String> {
    let mine = CHATS.lock().unwrap().get(chat).and_then(|c| c.tabs.iter().find(|(_, t)| t.in_chrome && t.target == target && !t.is_closed()).cloned());
    if let Some(found) = mine {
        return Ok(found);
    }
    chrome(own)?.attach(chat, target, true).await.map_err(|e| format!("taking Chrome tab {target} failed: {e}"))
}

pub async fn tabs(call: Call) -> Result<Vec<Block>, String> {
    let close: Vec<u32> = call.args["close"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|n| n as u32).collect();
    let mut closed = Vec::new();
    for number in close {
        let (_, tab) = tab_of(&call.chat, &json!({ "tab": number }))?;
        tab.close().await?;
        panel::announce(&call.chat);
        closed.push(format!("{number}{}", if tab.taken { " (let go)" } else { "" }));
    }
    let mut lines = Vec::new();
    if !closed.is_empty() {
        lines.push(format!("closed {}", closed.join(", ")));
    }
    let closed_line = lines.len();
    let mine: Vec<String> = {
        let mut chats = CHATS.lock().unwrap();
        let chat = chats.entry(call.chat.clone()).or_default();
        chat.tabs.retain(|(_, tab)| !tab.is_closed());
        for (number, tab) in &chat.tabs {
            let state = tab.state.lock().unwrap();
            let place = if tab.in_chrome { "the user's Chrome" } else { "NabuCode's built-in browser" };
            lines.push(format!("{number} · {} — {} ({place})", clip(&state.title, 100), state.url));
        }
        chat.tabs.iter().filter(|(_, t)| t.in_chrome).map(|(_, t)| t.target.clone()).collect()
    };
    match lines.len() == closed_line {
        true => lines.push("you have no tabs open".into()),
        false => lines.insert(closed_line, "Your tabs:".into()),
    }
    if let Ok(chrome) = chrome(call.chrome.as_deref()) {
        let targets = chrome.chrome.call(None, "Target.getTargets", json!({})).await?;
        let others: Vec<String> = targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page" && !mine.iter().any(|m| t["targetId"] == m.as_str()))
            .map(|t| format!("{} · {} — {}", t["targetId"].as_str().unwrap_or("?"), clip(t["title"].as_str().unwrap_or(""), 100), t["url"].as_str().unwrap_or("")))
            .collect();
        lines.push(String::new());
        match others.is_empty() {
            true => lines.push("the user's Chrome has no other tabs".into()),
            false => {
                lines.push("Tabs the user has open in their own Chrome, not yours, by id; browser_open with `take` makes one yours:".into());
                lines.extend(others);
            }
        }
    }
    Ok(vec![Block::text(lines.join("\n"))])
}

pub async fn page_state(call: Call) -> Result<Vec<Block>, String> {
    let (number, tab) = tab_of(&call.chat, &call.args)?;
    let view = view(&call.args)?;
    let _busy = tab.busy.lock().await;
    state(&tab, number, view, bool_arg(&call.args, "full"), opt_str(&call.args, "find")).await
}

pub async fn act(call: Call) -> Result<Vec<Block>, String> {
    let (number, tab) = tab_of(&call.chat, &call.args)?;
    let view = view(&call.args)?;
    let actions = match &call.args["actions"] {
        Value::Array(list) if !list.is_empty() => list.clone(),
        Value::Object(_) => vec![call.args["actions"].clone()],
        Value::String(text) => serde_json::from_str(text).map_err(|_| "`actions` must be a list of action objects")?,
        _ => return Err("missing `actions`: a list of actions like {\"do\": \"click\", \"target\": 3}".into()),
    };
    let _busy = tab.busy.lock().await;
    tab.call("Page.bringToFront", json!({})).await?;
    let mut failed = None;
    for (i, action) in actions.iter().enumerate() {
        let name = action["do"].as_str().unwrap_or("?");
        let dialog_open = tab.state.lock().unwrap().dialog.is_some();
        let done = match (name, dialog_open) {
            ("dialog", _) => act::run(&tab, action, &call.directory).await,
            (_, true) => Err("a dialog is open; answer it with the dialog action first".into()),
            _ => tab.unless_dialog(act::run(&tab, action, &call.directory)).await.map(drop),
        };
        if let Err(e) = done {
            failed = Some(format!("action {} of {} ({name}) failed: {e}", i + 1, actions.len()));
            break;
        }
        // The next action needs the new page.
        if matches!(name, "goto" | "back" | "forward" | "reload") && i + 1 < actions.len() {
            tab.settle().await;
        }
        if tab.state.lock().unwrap().dialog.is_some() && i + 1 < actions.len() {
            failed = Some(format!(
                "action {} of {} ({name}) opened a dialog; the actions after it were not run",
                i + 1,
                actions.len()
            ));
            break;
        }
    }
    tab.settle().await;
    let state = state(&tab, number, view, bool_arg(&call.args, "full"), None).await?;
    match failed {
        None => Ok(state),
        Some(e) => Err(format!("{e}\n\n{}", text_of(&state))),
    }
}

pub async fn read(call: Call) -> Result<Vec<Block>, String> {
    let (_, tab) = tab_of(&call.chat, &call.args)?;
    let _busy = tab.busy.lock().await;
    let expression = "({ html: document.documentElement ? document.documentElement.outerHTML : '', url: location.href })";
    let page = tab.unless_dialog(evaluate(&tab, expression)).await?.ok_or_else(dialog_open)?;
    let html = page["html"].as_str().unwrap_or_default().to_owned();
    let url = page["url"].as_str().unwrap_or_default().to_owned();
    let base = reqwest::Url::parse(&url).map_err(|e| format!("the tab's URL {url}: {e}"))?;
    let (title, text) = super::fetch::convert(html, base).await?;
    let lines = super::fetch::split(&text);
    super::fetch::show(&title, &url, &lines, "", &call.args).map(|text| vec![Block::text(text)])
}

pub async fn js(call: Call) -> Result<Vec<Block>, String> {
    let (_, tab) = tab_of(&call.chat, &call.args)?;
    let code = str_arg(&call.args, "code")?;
    let _busy = tab.busy.lock().await;
    let params = json!({ "expression": code, "awaitPromise": true, "returnByValue": true, "replMode": true, "userGesture": true });
    let result = tab.unless_dialog(tab.call("Runtime.evaluate", params)).await?.ok_or_else(dialog_open)?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("the script failed");
        return Err(clip(text, OUTPUT_CHARS));
    }
    let value = &result["result"];
    let text = match (value["type"].as_str(), value.get("value")) {
        (Some("undefined"), _) => "undefined".to_owned(),
        (_, Some(Value::String(s))) => serde_json::to_string(s).unwrap_or_default(),
        (_, Some(v)) => serde_json::to_string_pretty(v).unwrap_or_default(),
        _ => value["unserializableValue"].as_str().or(value["description"].as_str()).unwrap_or("undefined").to_owned(),
    };
    Ok(vec![Block::text(clip(&text, OUTPUT_CHARS))])
}

pub async fn logs(call: Call) -> Result<Vec<Block>, String> {
    let (number, tab) = tab_of(&call.chat, &call.args)?;
    let pattern = opt_str(&call.args, "find")
        .map(|p| regex::RegexBuilder::new(p).case_insensitive(true).build())
        .transpose()
        .map_err(|e| format!("bad regular expression in `find`: {e}"))?;
    let errors = bool_arg(&call.args, "errors");
    let kind = opt_str(&call.args, "kind").unwrap_or("console");
    if let Some(request) = num_arg(&call.args, "body") {
        return body(&tab, request).await.map(|text| vec![Block::text(text)]);
    }
    let lines: Vec<String> = {
        let state = tab.state.lock().unwrap();
        match kind {
            "console" => state.console.iter().filter(|l| !errors || l.starts_with("[error]")).cloned().collect(),
            "network" => state
                .network
                .iter()
                .enumerate()
                .filter(|(_, r)| !errors || r.failed.is_some() || r.status.is_some_and(|s| s >= 400))
                .map(|(i, r)| request_line(i + 1, r))
                .collect(),
            other => return Err(format!("`kind` is console or network, not `{other}`")),
        }
    };
    let lines: Vec<String> = lines.into_iter().filter(|l| pattern.as_ref().is_none_or(|p| p.is_match(l))).collect();
    let what = if kind == "console" { "console messages" } else { "requests" };
    if lines.is_empty() {
        return Ok(vec![Block::text(format!("tab {number}: no {what}{}", if errors || pattern.is_some() { " match" } else { "" }))]);
    }
    let total = lines.len();
    let mut shown: Vec<String> = lines.into_iter().skip(total.saturating_sub(LOG_SHOWN)).collect();
    if total > LOG_SHOWN {
        shown.insert(0, format!("({} earlier {what} not shown; use find)", total - LOG_SHOWN));
    }
    Ok(vec![Block::text(clip(&shown.join("\n"), OUTPUT_CHARS))])
}

fn request_line(number: usize, request: &tab::Request) -> String {
    let outcome = match (&request.failed, request.status, request.took) {
        (Some(error), ..) => format!("failed: {error}"),
        (None, Some(status), _) => status.to_string(),
        (None, None, Some(_)) => "done".into(),
        (None, None, None) => "pending".into(),
    };
    let took = request.took.map(|t| format!(", {} ms", t.as_millis())).unwrap_or_default();
    format!("#{number} {} {outcome} {} ({}{took})", request.method, clip(&request.url, 300), request.kind)
}

async fn body(tab: &Tab, number: usize) -> Result<String, String> {
    let id = {
        let state = tab.state.lock().unwrap();
        let request = state.network.get(number.wrapping_sub(1)).ok_or_else(|| format!("there is no request #{number}"))?;
        request.id.clone()
    };
    let body = tab
        .call("Network.getResponseBody", json!({ "requestId": id }))
        .await
        .map_err(|e| format!("the body of #{number} is not available: {e}"))?;
    let text = body["body"].as_str().unwrap_or_default();
    if body["base64Encoded"].as_bool().unwrap_or(false) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(text).unwrap_or_default();
        return match String::from_utf8(bytes) {
            Ok(text) => Ok(clip(&text, OUTPUT_CHARS)),
            Err(e) => Err(format!("the body of #{number} is binary, {} bytes", e.into_bytes().len())),
        };
    }
    Ok(clip(text, OUTPUT_CHARS))
}

fn dialog_open() -> String {
    "the page opened a dialog and waits for an answer: answer it with browser_act's dialog action".into()
}

async fn evaluate(tab: &Tab, expression: &str) -> Result<Value, String> {
    let result = tab.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true })).await?;
    if let Some(details) = result.get("exceptionDetails") {
        return Err(details["text"].as_str().unwrap_or("the script failed").to_owned());
    }
    Ok(result["result"]["value"].clone())
}

/// The tab's state as text and/or a screenshot, as changes unless `full`.
async fn state(tab: &Tab, number: u32, view: View, full: bool, find: Option<&str>) -> Result<Vec<Block>, String> {
    // A headless browser draws only the tab in front, and a tab the page opened takes the front.
    // The user's Chrome brings the tab to front only for a screenshot: text needs no switching of the user's tabs.
    if !tab.in_chrome || view != View::Text {
        tab.call("Page.bringToFront", json!({})).await?;
    }
    let mut blocks = Vec::new();
    if view != View::Screenshot {
        blocks.push(Block::text(state_text(tab, number, full, find).await?));
    }
    let dialog = tab.state.lock().unwrap().dialog.is_some();
    if view != View::Text && !dialog {
        let shot = tab.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 80 })).await?;
        if view == View::Screenshot {
            let state = tab.state.lock().unwrap();
            blocks.push(Block::text(format!("Tab {number} · {} — {}", state.title, state.url)));
        }
        blocks.push(Block::Media {
            media: Media {
                mime: Some("image/jpeg".into()),
                source: MediaSource::Base64(shot["data"].as_str().unwrap_or_default().to_owned()),
                filename: None,
            },
            cache: None,
            ext: Default::default(),
        });
    }
    Ok(blocks)
}

async fn state_text(tab: &Tab, number: u32, full: bool, find: Option<&str>) -> Result<String, String> {
    let (notes, dialog) = {
        let mut state = tab.state.lock().unwrap();
        (std::mem::take(&mut state.notes), state.dialog.clone())
    };
    let mut out = Vec::new();
    let note_lines = |out: &mut Vec<String>| {
        out.extend(notes.iter().cloned());
        if let Some(dialog) = &dialog {
            out.push(format!("a dialog is open: {dialog}; the page waits until the `dialog` action answers it"));
        }
    };
    if dialog.is_some() {
        let state = tab.state.lock().unwrap();
        out.push(format!("Tab {number} · {} — {}", state.title, state.url));
        note_lines(&mut out);
        return Ok(out.join("\n"));
    }
    let tree = tab.call("Accessibility.getFullAXTree", json!({})).await?;
    let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
    let mut state = tab.state.lock().unwrap();
    let page::Rendered { title, lines, focus } = page::render(&nodes, &mut state.numbers);
    if !title.is_empty() {
        state.title = title;
    }
    let mut head = format!("Tab {number} · {} — {}", state.title, state.url);
    let last = state.last.replace(lines.clone());
    let url = state.url.clone();
    let last_url = state.last_url.replace(url);
    let navigated = std::mem::take(&mut state.navigated);
    if navigated {
        head.push_str(&match last_url {
            Some(url) if url == state.url => " · the page loaded again since the last state".to_owned(),
            Some(url) => format!(" · the page went here from {url} since the last state"),
            None => String::new(),
        });
    }
    if let Some(focus) = focus {
        head.push_str(&format!(" · focus {focus}"));
    }
    if let Some(pattern) = find {
        out.push(head);
        note_lines(&mut out);
        let found = page::find(&lines, pattern)?;
        if found.is_empty() {
            out.push(format!("no lines match `{pattern}` among {} lines", lines.len()));
        }
        out.extend(page::cut(found));
        return Ok(out.join("\n"));
    }
    // A new document has new elements: marks against the old one would read as changes of the same elements.
    let full = full || navigated;
    let mostly_changed = last.is_some() && !full;
    let short = lines.len() <= page::SHORT_STATE;
    match last.filter(|_| !full).and_then(|last| page::changes(&last, &lines, short)) {
        Some((changed, _)) if changed.is_empty() => {
            out.push(format!("{head} · no changes in the state since the last one"));
            note_lines(&mut out);
        }
        Some((changed, same)) => {
            out.push(match short {
                true => format!("{head} · changes since the last state marked - + ~"),
                false => format!("{head} · changes since the last state, {same} lines unchanged"),
            });
            note_lines(&mut out);
            out.extend(page::cut(changed));
        }
        None => {
            out.push(match mostly_changed {
                true => format!("{head} · the whole state: most of it changed since the last state"),
                false => head,
            });
            note_lines(&mut out);
            if lines.is_empty() {
                out.push("(the page is empty)".into());
            }
            out.extend(page::cut(lines));
        }
    }
    Ok(out.join("\n"))
}

/// The text blocks of a result, joined.
fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Sign in</title></head><body>
<nav><a href="/home">Home</a></nav>
<h1>Welcome back</h1><p>Use your work account</p>
<form onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'Hello ' + this.email.value; console.log('sent', this.email.value); fetch('/api/login').catch(() => {});">
<label>Email <input name="email" type="email"></label>
<label>Password <input name="password" type="password"></label>
<label><input type="checkbox" name="remember"> Remember me</label>
<select name="role"><option>Viewer</option><option>Editor</option></select>
<button>Sign in</button></form>
<p id="out"></p>
<button onclick="alert('Saved!')">Alert</button>
<a href="about:blank#popup" target="_blank">Popup</a>
<script>console.warn('page loaded'); setTimeout(() => { throw new Error('late failure') }, 10);</script>
</body></html>"#;

    fn text(blocks: &Result<Vec<Block>, String>) -> String {
        match blocks {
            Ok(blocks) => text_of(blocks),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Drives a real headless Chrome: BROWSER_HOME=<dir> cargo test browser_tools -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn browser_tools() {
        let home = PathBuf::from(std::env::var("BROWSER_HOME").unwrap());
        super::super::browser::set_home(&home);
        let page = home.join("page.html");
        std::fs::write(&page, PAGE).unwrap();
        let url = format!("file:///{}", page.to_string_lossy().replace('\\', "/"));
        let call = |args: Value| Call { chat: "chat1".into(), chrome: None, directory: home.clone(), args };
        let clock = std::sync::Mutex::new(std::time::Instant::now());
        let show = |what: &str, out: &Result<Vec<Block>, String>| {
            let mut last = clock.lock().unwrap();
            println!("===== {what} ({} ms)\n{}\n", last.elapsed().as_millis(), text(out));
            *last = std::time::Instant::now();
        };

        let opened = open(call(json!({ "url": url }))).await;
        show("open", &opened);
        let state = page_state(call(json!({ "tab": 1, "full": true }))).await;
        let number = |name: &str| {
            let text = text(&state);
            let line = text.lines().find(|l| l.contains(name)).unwrap().to_owned();
            line.trim().trim_start_matches('[').split(']').next().unwrap().parse::<u32>().unwrap()
        };
        let (email, password, remember, role, sign_in) =
            (number("textbox \"Email\""), number("Password"), number("Remember"), number("combobox"), number("button \"Sign in\""));
        let acted = act(call(json!({ "tab": 1, "actions": [
            { "do": "fill", "target": email, "value": "a@b.c" },
            { "do": "type", "target": password, "text": "secret" },
            { "do": "fill", "target": remember, "value": true },
            { "do": "fill", "target": role, "value": "Editor" },
            { "do": "click", "target": sign_in }
        ] }))).await;
        show("act: fill form and sign in", &acted);
        show("state again", &page_state(call(json!({ "tab": 1 }))).await);
        show("find", &page_state(call(json!({ "tab": 1, "find": "sign|hello" }))).await);
        show("key ctrl+a and type", &act(call(json!({ "tab": 1, "actions": [
            { "do": "click", "target": email }, { "do": "key", "keys": "ctrl+a" }, { "do": "type", "text": "x@y.z" }
        ] }))).await);
        show("popup", &act(call(json!({ "tab": 1, "actions": [{ "do": "click", "target": number("link \"Popup\"") }] }))).await);
        show("state after popup", &page_state(call(json!({ "tab": 1 }))).await);
        show("tabs", &tabs(call(json!({}))).await);
        show("alert", &act(call(json!({ "tab": 1, "actions": [{ "do": "click", "target": number("button \"Alert\"") }, { "do": "key", "keys": "Tab" }] }))).await);
        show("js during dialog", &js(call(json!({ "tab": 1, "code": "1 + 1" }))).await);
        show("answer dialog", &act(call(json!({ "tab": 1, "actions": [{ "do": "dialog", "accept": true }] }))).await);
        show("js", &js(call(json!({ "tab": 1, "code": "await new Promise(r => setTimeout(r, 10)); ({ out: document.getElementById('out').textContent, n: [1, 2] })" }))).await);
        show("js error", &js(call(json!({ "tab": 1, "code": "nope.x" }))).await);
        show("console", &logs(call(json!({ "tab": 1 }))).await);
        show("network", &logs(call(json!({ "tab": 1, "kind": "network" }))).await);
        show("read", &read(call(json!({ "tab": 1 }))).await);
        show("reload then old number", &act(call(json!({ "tab": 1, "actions": [{ "do": "reload" }, { "do": "fill", "target": email, "value": "q" }] }))).await);
        show("bad target", &act(call(json!({ "tab": 1, "actions": [{ "do": "click", "target": 999 }] }))).await);
        let shot = page_state(call(json!({ "tab": 1, "view": "both" }))).await.unwrap();
        assert!(shot.iter().any(|b| matches!(b, Block::Media { .. })));
        show("close", &tabs(call(json!({ "close": [2] }))).await);
        let started = std::time::Instant::now();
        let example = open(call(json!({ "url": "https://example.com" }))).await;
        show(&format!("example.com in {} ms", started.elapsed().as_millis()), &example);
    }

    /// Drives the user's-Chrome path end to end: a Chromium that takes `--load-extension` runs the extension from `extension/`,
    /// which finds this test's server through `nabu` as the native host (build it first).
    /// CHROMIUM=<chrome.exe> cargo test chrome_extension -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn chrome_extension() {
        use axum::extract::ws::WebSocketUpgrade;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = axum::Router::new()
            .route(
                "/chrome",
                axum::routing::get(|ws: WebSocketUpgrade, query: axum::extract::Query<HashMap<String, String>>| async move {
                    ws.on_upgrade(move |socket| chrome_link(socket, query.get("chrome").cloned().unwrap_or_default()))
                }),
            )
            // Chrome lets no extension attach to an `about:` page with a fragment
            .route("/page", axum::routing::get(|| async { axum::response::Html(PAGE.replace("about:blank#popup", "/page#popup")) }));
        tokio::spawn(async move { axum::serve(listener, router).await });
        let home = std::env::temp_dir().join(format!("nabu-chrome-test-{}", std::process::id()));
        crate::chrome_host::list(port, "test", &home);
        let deps = std::env::current_exe().unwrap();
        crate::chrome_host::register(&deps.parent().unwrap().parent().unwrap().join("nabu.exe"));
        let extension = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extension");
        let _chromium = tokio::process::Command::new(std::env::var("CHROMIUM").unwrap())
            .arg("--headless=new")
            .arg(format!("--user-data-dir={}", home.join("profile").display()))
            .arg(format!("--load-extension={}", extension.canonicalize().unwrap().display()))
            .args(["--no-first-run", "--no-default-browser-check", "about:blank"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while chrome(None).is_err() {
            assert!(started.elapsed() < Duration::from_secs(60), "the extension did not connect");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        println!("===== connected in {} ms\n", started.elapsed().as_millis());
        let call = |args: Value| Call { chat: "chat1".into(), chrome: None, directory: home.clone(), args };
        let show = |what: &str, out: &Result<Vec<Block>, String>| println!("===== {what}\n{}\n", text(out));
        let listed = tabs(call(json!({}))).await;
        show("tabs at start", &listed);
        let theirs = text(&listed).lines().filter_map(|l| l.split(" · ").next()?.parse::<u64>().ok()).next().unwrap();
        let page = format!("http://127.0.0.1:{port}/page");
        let opened = open(call(json!({ "browser": "chrome", "url": page }))).await;
        show("open in chrome", &opened);
        let number = |name: &str| {
            let text = text(&opened);
            let line = text.lines().find(|l| l.contains(name)).unwrap().to_owned();
            line.trim().trim_start_matches('[').split(']').next().unwrap().parse::<u32>().unwrap()
        };
        show("fill and sign in", &act(call(json!({ "tab": 1, "actions": [
            { "do": "fill", "target": number("textbox \"Email\""), "value": "a@b.c" },
            { "do": "click", "target": number("button \"Sign in\"") }
        ] }))).await);
        let shot = page_state(call(json!({ "tab": 1, "view": "both" }))).await;
        show("screenshot", &shot);
        assert!(shot.unwrap().iter().any(|b| matches!(b, Block::Media { .. })));
        show("popup", &act(call(json!({ "tab": 1, "actions": [{ "do": "click", "target": number("link \"Popup\"") }] }))).await);
        show("js", &js(call(json!({ "tab": 1, "code": "document.getElementById('out').textContent" }))).await);
        show("console", &logs(call(json!({ "tab": 1 }))).await);
        show("take the user's tab", &open(call(json!({ "take": theirs, "url": page }))).await);
        show("tabs", &tabs(call(json!({}))).await);
        show("let the taken tab go", &tabs(call(json!({ "close": [3] }))).await);
        let own = CHROMES.lock().unwrap()[0].0.clone();
        assert!(!own.is_empty(), "the extension names its Chrome");
        let in_own = |args: Value| Call { chat: "chat2".into(), chrome: Some(own.clone()), directory: home.clone(), args };
        let opened = open(in_own(json!({ "url": page }))).await;
        show("a chat of this Chrome opens tabs in it by default", &tabs(in_own(json!({}))).await);
        assert!(opened.is_ok() && text(&tabs(in_own(json!({}))).await).contains("(the user's Chrome)"));
        let elsewhere = Call { chat: "chat3".into(), chrome: Some("gone".into()), directory: home.clone(), args: json!({ "url": page }) };
        show("a chat of a Chrome that is gone", &open(elsewhere).await);
    }
}
