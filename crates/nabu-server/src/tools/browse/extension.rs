//! The user's Chrome: the NabuCode extension connects to the server and passes DevTools messages to and from its tabs.

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use super::cdp::Chrome;
use super::{Browser, CHROMES};

/// Serves one connection of the extension until it closes. `instance` is the extension's id of its Chrome:
/// chats started there work in it; other chats use the Chrome connected last.
pub async fn link(socket: WebSocket, instance: String) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = outgoing.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let chrome = Chrome::new(out, None);
    let (events, incoming) = mpsc::unbounded_channel();
    let reader = chrome.clone();
    let reading = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            if let Message::Text(text) = message {
                reader.receive(&text, &events);
            }
        }
        reader.close();
    });
    match Browser::start(chrome.clone(), true, incoming).await {
        Ok(browser) => CHROMES.lock().unwrap().push((instance, browser)),
        Err(e) => eprintln!("[chrome] the extension's connection failed: {e}"),
    }
    let _ = reading.await;
    writer.abort();
    CHROMES.lock().unwrap().retain(|(_, b)| !std::sync::Arc::ptr_eq(&b.chrome, &chrome));
}
