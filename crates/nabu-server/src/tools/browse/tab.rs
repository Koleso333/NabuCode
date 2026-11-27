use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use nabu_protocol::BrowserTab;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::cdp::Chrome;
use super::page::Numbers;
use crate::tools::web::{clip, squeeze};

/// Longest wait for a page to settle after actions.
const SETTLE: Duration = Duration::from_secs(3);
/// No requests, loads or DOM changes for this long counts as settled.
const QUIET: Duration = Duration::from_millis(300);
const LOG_LINES: usize = 1000;

/// Marks the time of the page's last DOM change, in every document the tab loads, and reports the page title
/// through the `TITLE` binding: headless Chrome does not say when it changes.
const WATCH_DOM: &str = "(() => { const k = Symbol.for('nabu.changed'); if (window[k] !== undefined) return; window[k] = performance.now(); let title; \
const seen = () => { window[k] = performance.now(); if (window === top && document.title !== title) { title = document.title; window.nabuTitle?.(title); } }; \
new MutationObserver(seen).observe(document, { subtree: true, childList: true, attributes: true, characterData: true }); seen(); \
addEventListener('pageshow', e => { if (e.persisted) { title = undefined; seen(); } }); })()";
const TITLE: &str = "nabuTitle";

pub struct Tab {
    pub chat: String,
    pub target: String,
    pub session: String,
    /// The tab is in the user's Chrome, not in the server's headless browser.
    pub in_chrome: bool,
    /// The user opened the tab and the model took it: letting it go detaches it, it stays open.
    pub taken: bool,
    chrome: Arc<Chrome>,
    /// Calls on one tab run one after another.
    pub busy: tokio::sync::Mutex<()>,
    pub state: Mutex<State>,
    /// Screencast frames for the GUI's browser panel.
    pub frames: broadcast::Sender<Arc<Frame>>,
    /// Panel sockets that watch the tab now.
    pub watchers: Mutex<usize>,
}

/// One screencast frame: a PNG and the page area it shows, in CSS pixels.
pub struct Frame {
    pub image: Vec<u8>,
    pub width: f64,
    pub height: f64,
}

#[derive(Default)]
pub struct State {
    pub url: String,
    pub title: String,
    main_frame: String,
    pub loading: bool,
    /// The site's icon as a `data:` URL, and where it came from.
    pub favicon: Option<String>,
    pub icon_source: Option<String>,
    /// Requests in flight, with when they started.
    inflight: HashMap<String, Instant>,
    activity: Option<Instant>,
    pub dialog: Option<String>,
    /// Tabs this page opened that are still being attached.
    pub opening: usize,
    /// What happened since the model last saw the state: new tabs, dialogs, crashes.
    pub notes: Vec<String>,
    pub numbers: Numbers,
    /// The state lines the model saw last; `None` sends the whole state next.
    pub last: Option<Vec<String>>,
    /// URL of the state the model saw last.
    pub last_url: Option<String>,
    /// The main frame loaded a document since the last state.
    pub navigated: bool,
    pub console: VecDeque<String>,
    pub network: Vec<Request>,
    requests: HashMap<String, usize>,
    pub closed: bool,
    /// The panel's last size: CSS width, height and device pixel ratio. The page takes it unless the model fixed one.
    pub panel_size: Option<(f64, f64, f64)>,
    /// The model set the page size with the `viewport` action.
    pub fixed: bool,
}

pub struct Request {
    pub id: String,
    pub method: String,
    pub url: String,
    pub kind: String,
    pub status: Option<i64>,
    pub failed: Option<String>,
    pub started: Instant,
    pub took: Option<Duration>,
}

impl Tab {
    /// Attaches to a page target.
    pub async fn attach(chrome: Arc<Chrome>, chat: &str, target: &str, in_chrome: bool, taken: bool) -> Result<Arc<Self>, String> {
        let attached = chrome.call(None, "Target.attachToTarget", json!({ "targetId": target, "flatten": true })).await?;
        let session = attached["sessionId"].as_str().ok_or("attaching to the tab failed")?.to_owned();
        let tab = Arc::new(Self {
            chat: chat.to_owned(),
            target: target.to_owned(),
            session,
            in_chrome,
            taken,
            chrome,
            busy: Default::default(),
            state: Mutex::new(State { url: "about:blank".into(), ..Default::default() }),
            frames: broadcast::channel(2).0,
            watchers: Mutex::new(0),
        });
        Ok(tab)
    }

    /// Turns on what the tools read; the tab must already get its events.
    pub async fn start(&self, agent: Option<&str>) -> Result<(), String> {
        for method in ["Page.enable", "Runtime.enable", "Network.enable", "DOM.enable", "Accessibility.enable", "Log.enable"] {
            self.call(method, json!({})).await?;
        }
        if let Some(agent) = agent {
            self.call("Network.setUserAgentOverride", json!({ "userAgent": agent })).await?;
        }
        self.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true })).await?;
        self.call("Runtime.addBinding", json!({ "name": TITLE })).await?;
        self.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": WATCH_DOM })).await?;
        let _ = self.call("Runtime.evaluate", json!({ "expression": WATCH_DOM })).await;
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let mut state = self.state.lock().unwrap();
        state.main_frame = tree["frameTree"]["frame"]["id"].as_str().unwrap_or_default().to_owned();
        state.url = url_of(&tree["frameTree"]["frame"]);
        Ok(())
    }

    /// Gives the elements on the page now their numbers, without a state for the model.
    pub async fn renumber(&self) -> Result<(), String> {
        let tree = self.call("Accessibility.getFullAXTree", json!({})).await?;
        let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
        super::page::render(&nodes, &mut self.state.lock().unwrap().numbers);
        Ok(())
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.chrome.call(Some(&self.session), method, params).await
    }

    pub async fn browser_call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.chrome.call(None, method, params).await
    }

    /// Closes a tab the model opened; a tab it took from the user is only let go.
    pub async fn close(&self) -> Result<(), String> {
        if self.taken {
            return self.let_go().await;
        }
        let closed = self.browser_call("Target.closeTarget", json!({ "targetId": self.target })).await;
        self.state.lock().unwrap().closed = true;
        closed.map(drop)
    }

    /// Leaves the tab open and stops driving it.
    pub async fn let_go(&self) -> Result<(), String> {
        let detached = self.browser_call("Target.detachFromTarget", json!({ "sessionId": self.session })).await;
        self.state.lock().unwrap().closed = true;
        detached.map(drop)
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed || self.chrome.is_closed()
    }

    /// Gives the page the panel's size, unless the model fixed its own.
    pub async fn fit_panel(&self) -> Result<(), String> {
        let size = {
            let state = self.state.lock().unwrap();
            state.panel_size.filter(|_| !state.fixed)
        };
        let Some((width, height, scale)) = size else { return Ok(()) };
        let metrics = json!({ "width": width.round(), "height": height.round(), "deviceScaleFactor": scale, "mobile": false });
        self.call("Emulation.setDeviceMetricsOverride", metrics).await.map(drop)
    }

    /// The tab as the GUI's browser panel lists it.
    pub fn info(&self, number: u32) -> BrowserTab {
        let state = self.state.lock().unwrap();
        BrowserTab {
            id: self.target.clone(),
            number,
            title: state.title.clone(),
            url: state.url.clone(),
            favicon: state.favicon.clone(),
            loading: state.loading,
        }
    }

    /// Updates the tab from one of its DevTools events; true when something the panel lists changed.
    pub fn event(&self, method: &str, params: &Value) -> bool {
        if method == "Page.screencastFrame" {
            self.frame(params);
            return false;
        }
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        let now = Instant::now();
        let main = params["frameId"] == state.main_frame.as_str();
        match method {
            "Page.frameStartedLoading" if main => {
                state.loading = true;
                state.activity = Some(now);
                return true;
            }
            "Page.frameStoppedLoading" if main => {
                state.loading = false;
                state.activity = Some(now);
                return true;
            }
            "Page.frameNavigated" if params["frame"]["parentId"].is_null() => {
                let frame = &params["frame"];
                state.main_frame = frame["id"].as_str().unwrap_or_default().to_owned();
                state.url = url_of(frame);
                state.numbers.forget_nodes();
                state.navigated = true;
                state.activity = Some(now);
                return true;
            }
            "Page.navigatedWithinDocument" if main => {
                state.url = params["url"].as_str().unwrap_or_default().to_owned();
                return true;
            }
            "Page.javascriptDialogOpening" => {
                let kind = params["type"].as_str().unwrap_or("alert");
                let message = clip(&squeeze(params["message"].as_str().unwrap_or("")), 300);
                state.dialog = Some(format!("{kind} \"{message}\""));
            }
            "Page.javascriptDialogClosed" => state.dialog = None,
            "Runtime.bindingCalled" if params["name"] == TITLE => {
                state.title = params["payload"].as_str().unwrap_or_default().to_owned();
                return true;
            }
            "Inspector.targetCrashed" => state.notes.push("the page crashed; reload it".into()),
            "Network.requestWillBeSent" => request_sent(state, params, now),
            "Network.responseReceived" => {
                if let Some(request) = state.request(params) {
                    request.status = params["response"]["status"].as_i64();
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                let failed = params["errorText"].as_str().map(str::to_owned);
                let canceled = params["canceled"].as_bool().unwrap_or(false);
                if let Some(request) = state.request(params) {
                    request.took = Some(request.started.elapsed());
                    if let Some(error) = failed {
                        request.failed = Some(if canceled { "canceled".into() } else { error });
                    }
                }
                if let Some(id) = params["requestId"].as_str() {
                    state.inflight.remove(id);
                }
                state.activity = Some(now);
            }
            "Runtime.consoleAPICalled" => {
                let text = params["args"].as_array().into_iter().flatten().map(remote_text).collect::<Vec<_>>().join(" ");
                let level = match params["type"].as_str().unwrap_or("log") {
                    "warning" => "warn",
                    other => other,
                };
                let place = frame_place(&params["stackTrace"]["callFrames"][0]);
                state.log(format!("[{level}] {}{place}", clip(&text, 2000)));
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("exception");
                let place = frame_place(&details["stackTrace"]["callFrames"][0]);
                state.log(format!("[error] {}{place}", clip(text, 2000)));
            }
            "Log.entryAdded" => {
                let entry = &params["entry"];
                let level = match entry["level"].as_str().unwrap_or("info") {
                    "warning" => "warn",
                    "verbose" => "debug",
                    other => other,
                };
                let url = entry["url"].as_str().map(|u| format!(" ({u})")).unwrap_or_default();
                state.log(format!("[{level}] {}{url}", clip(entry["text"].as_str().unwrap_or(""), 2000)));
            }
            _ => {}
        }
        false
    }

    /// Passes a screencast frame to the panel and asks Chrome for the next one.
    fn frame(&self, params: &Value) {
        let (chrome, session, frame) = (self.chrome.clone(), self.session.clone(), params["sessionId"].clone());
        tokio::spawn(async move { chrome.call(Some(&session), "Page.screencastFrameAck", json!({ "sessionId": frame })).await });
        let Ok(image) = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str().unwrap_or_default()) else { return };
        let meta = &params["metadata"];
        let (width, height) = (meta["deviceWidth"].as_f64().unwrap_or(0.0), meta["deviceHeight"].as_f64().unwrap_or(0.0));
        let _ = self.frames.send(Arc::new(Frame { image, width, height }));
    }

    /// Runs `work` unless the page opens a dialog meanwhile: then `None`, since the page waits for an answer.
    pub async fn unless_dialog<T>(&self, work: impl Future<Output = Result<T, String>>) -> Result<Option<T>, String> {
        let dialog = async {
            while self.state.lock().unwrap().dialog.is_none() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::select! {
            out = work => out.map(Some),
            () = dialog => Ok(None),
        }
    }

    /// Waits until the page has no new requests, loads or DOM changes, at most `SETTLE`.
    pub async fn settle(&self) {
        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        while start.elapsed() < SETTLE {
            let quiet = {
                let state = self.state.lock().unwrap();
                if state.dialog.is_some() || state.closed {
                    return;
                }
                let busy = state.loading || state.opening > 0 || state.inflight.values().any(|at| *at >= start);
                !busy && state.activity.is_none_or(|at| at.elapsed() >= QUIET)
            };
            if quiet && self.dom_quiet().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn dom_quiet(&self) -> bool {
        let expression = "performance.now() - (window[Symbol.for('nabu.changed')] ?? 0)";
        let since = self.call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true })).await;
        since.map_or(true, |v| v["result"]["value"].as_f64().unwrap_or(f64::MAX) >= QUIET.as_millis() as f64)
    }
}

/// A frame's URL with its `#fragment`, which CDP keeps apart.
fn url_of(frame: &Value) -> String {
    let url = frame["url"].as_str().unwrap_or_default();
    format!("{url}{}", frame["urlFragment"].as_str().unwrap_or_default())
}

impl State {
    fn log(&mut self, line: String) {
        if self.console.len() >= LOG_LINES {
            self.console.pop_front();
        }
        self.console.push_back(line);
    }

    fn request(&mut self, params: &Value) -> Option<&mut Request> {
        let index = *self.requests.get(params["requestId"].as_str()?)?;
        self.network.get_mut(index)
    }
}

fn request_sent(state: &mut State, params: &Value, now: Instant) {
    let id = params["requestId"].as_str().unwrap_or_default().to_owned();
    let kind = params["type"].as_str().unwrap_or("Other").to_owned();
    let url = params["request"]["url"].as_str().unwrap_or_default();
    if url.starts_with("data:") {
        return;
    }
    // A redirect keeps the request id: the earlier hop ends with its status.
    if let Some(request) = state.request(params) {
        request.status = params["redirectResponse"]["status"].as_i64();
        request.took = Some(request.started.elapsed());
    }
    if kind != "EventSource" {
        state.inflight.insert(id.clone(), now);
    }
    state.activity = Some(now);
    if state.network.len() >= LOG_LINES {
        state.network.remove(0);
        state.requests.retain(|_, i| *i > 0);
        state.requests.values_mut().for_each(|i| *i -= 1);
    }
    state.requests.insert(id.clone(), state.network.len());
    state.network.push(Request {
        id,
        method: params["request"]["method"].as_str().unwrap_or("GET").to_owned(),
        url: url.to_owned(),
        kind,
        status: None,
        failed: None,
        started: now,
        took: None,
    });
}

/// A console argument as text: its value, else its description.
fn remote_text(arg: &Value) -> String {
    match &arg["value"] {
        Value::String(s) => s.clone(),
        Value::Null => arg["description"].as_str().or(arg["unserializableValue"].as_str()).unwrap_or("undefined").to_owned(),
        other => other.to_string(),
    }
}

/// ` (url:line)` of a stack frame, or nothing.
fn frame_place(frame: &Value) -> String {
    match frame["url"].as_str().filter(|u| !u.is_empty()) {
        Some(url) => format!(" ({url}:{})", frame["lineNumber"].as_i64().unwrap_or(0) + 1),
        None => String::new(),
    }
}
