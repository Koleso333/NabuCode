use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// Where events of the browser and its tabs go: (CDP session id, method, params).
pub type Events = mpsc::UnboundedSender<(Option<String>, String, Value)>;
pub type Incoming = mpsc::UnboundedReceiver<(Option<String>, String, Value)>;

/// A DevTools connection: to the headless Chrome the server runs, or to the user's Chrome through the NabuCode extension.
pub struct Chrome {
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    waiting: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    closed: AtomicBool,
    _process: Option<Child>,
}

impl Chrome {
    /// Starts the browser with the profile in `profile` and connects to it.
    pub async fn start(binary: &Path, profile: &Path, events: Events) -> Result<Arc<Self>, String> {
        let port_file = profile.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);
        let mut command = Command::new(binary);
        command
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--mute-audio",
                "--window-size=1280,800",
                "--disable-features=Translate,MediaRouter",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let process = command.spawn().map_err(|e| format!("starting {} failed: {e}", binary.display()))?;
        #[cfg(windows)]
        if let Some(handle) = process.raw_handle() {
            super::super::shell::contain_handle(handle);
        }
        let address = tokio::time::timeout(START_TIMEOUT, async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&port_file)
                    && let Some((port, path)) = text.split_once('\n')
                    && !path.trim().is_empty()
                {
                    return format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| format!("the browser did not start in {} s", START_TIMEOUT.as_secs()))?;
        let (socket, _) = tokio_tungstenite::connect_async(&address)
            .await
            .map_err(|e| format!("connecting to the browser failed: {e}"))?;
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(text) = outgoing.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
        });
        let chrome = Self::new(out, Some(process));
        let reader = chrome.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    reader.receive(&text, &events);
                }
            }
            reader.close();
        });
        Ok(chrome)
    }

    /// A connection whose messages go out through `out`; the owner passes what comes back to `receive`.
    pub fn new(out: mpsc::UnboundedSender<String>, process: Option<Child>) -> Arc<Self> {
        Arc::new(Self { out, next: AtomicU64::new(1), waiting: Mutex::default(), closed: AtomicBool::new(false), _process: process })
    }

    /// Ends the connection: calls waiting for an answer fail.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for (_, waiting) in self.waiting.lock().unwrap().drain() {
            let _ = waiting.send(Err("the browser closed".into()));
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn receive(&self, text: &str, events: &Events) {
        let Ok(value) = serde_json::from_str::<Value>(text) else { return };
        if let Some(id) = value["id"].as_u64() {
            let Some(waiting) = self.waiting.lock().unwrap().remove(&id) else { return };
            let result = match value.get("error") {
                Some(error) => Err(error["message"].as_str().unwrap_or("unknown error").to_owned()),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiting.send(result);
            return;
        }
        if let Some(method) = value["method"].as_str() {
            let session = value["sessionId"].as_str().map(str::to_owned);
            let _ = events.send((session, method.to_owned(), value.get("params").cloned().unwrap_or(Value::Null)));
        }
    }

    /// Calls a DevTools method of the browser (`session` None) or of an attached tab.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value, String> {
        if self.is_closed() {
            return Err("the browser closed".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (tell, answer) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tell);
        if self.out.send(message.to_string()).is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err("the browser closed".into());
        }
        match tokio::time::timeout(CALL_TIMEOUT, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the browser closed".into()),
            Err(_) => {
                self.waiting.lock().unwrap().remove(&id);
                Err(format!("the browser did not answer {method} in {} s", CALL_TIMEOUT.as_secs()))
            }
        }
    }
}
