use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use nabu_protocol::{ClientMessage, Event, Listening, ProtocolError, ServerMessage};
use nabu_server::Server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

struct App {
    server: Server,
    token: String,
}

const USAGE: &str = "Usage: nabu serve [--data <folder>]\n\n  serve   runs the server for a NabuCode app; it prints its port and token and stops with the app";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() != Some("serve") {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let app = Arc::new(App {
        server: Server::new(&data_dir()),
        token: new_token(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;

    let listening = Listening {
        port: listener.local_addr()?.port(),
        token: app.token.clone(),
    };
    println!(
        "{}",
        serde_json::to_string(&listening).expect("serializable")
    );

    tokio::spawn(exit_with_parent());
    let starter = app.clone();
    tokio::spawn(async move { starter.server.start().await });

    let router = axum::Router::new()
        .route("/", get(connect))
        .route("/terminal", get(connect_terminal))
        .route("/file", get(send_file))
        .route("/upload", post(receive_file).layer(DefaultBodyLimit::disable()))
        .with_state(app);
    axum::serve(listener, router).await
}

/// The app holds our stdin; EOF means it is gone.
async fn exit_with_parent() {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 64];
    while matches!(stdin.read(&mut buf).await, Ok(n) if n > 0) {}
    std::process::exit(0);
}

async fn connect(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws.on_upgrade(move |socket| serve_client(socket, app)))
}

async fn connect_terminal(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let terminal = query.get("id").and_then(|id| app.server.terminals.get(id));
    let cursor = query
        .get("cursor")
        .and_then(|cursor| cursor.parse().ok())
        .unwrap_or(0);
    Ok(ws.on_upgrade(move |socket| nabu_server::terminal::stream(socket, terminal, cursor)))
}

/// An attachment by link, so chats with pictures don't carry them inline; it never changes, so it is cached for good.
async fn send_file(
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (Some(session), Some(name)) = (query.get("session"), query.get("name")) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let path = app.server.attachment(session, name).ok_or(StatusCode::BAD_REQUEST)?;
    let bytes = tokio::fs::read(&path).await.map_err(|_| StatusCode::NOT_FOUND)?;
    Response::builder()
        .header(header::CONTENT_TYPE, mime_of(name))
        .header(header::CACHE_CONTROL, "private, max-age=31536000, immutable")
        .body(bytes.into())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// A file without a path on this computer (pasted, or from a remote client): its bytes are streamed to the session's folder,
/// and the part then goes with the reference this returns.
async fn receive_file(
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
    body: Body,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (Some(session), Some(part), Some(mime)) = (query.get("session"), query.get("part"), query.get("mime")) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let (path, url) = app.server.upload(session, part, mime, query.get("filename").cloned()).map_err(|_| StatusCode::NOT_FOUND)?;
    let tmp = path.with_extension("upload");
    let written = write_stream(&tmp, body).await;
    if let Err(e) = written.and(tokio::fs::rename(&tmp, &path).await) {
        eprintln!("[upload] {}: {e}", path.display());
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(serde_json::json!({ "url": url }).to_string().into())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn write_stream(path: &std::path::Path, body: Body) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        file.write_all(&chunk.map_err(std::io::Error::other)?).await?;
    }
    file.flush().await
}

fn mime_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map_or("", |(_, ext)| ext) {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn serve_client(socket: WebSocket, app: Arc<App>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = serde_json::to_string(&msg).expect("serializable");
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let _ = tx.send(ServerMessage::Event {
        event: Event::Connected,
    });
    let mut events = app.server.subscribe();
    let event_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if event_tx.send(ServerMessage::Event { event }).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let msg: ClientMessage = match serde_json::from_str(&text) {
            Ok(msg) => msg,
            Err(err) => {
                let id = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v["id"].as_u64());
                let error = ProtocolError {
                    message: format!("bad request: {err}"),
                };
                let _ = tx.send(ServerMessage::Error { id, error });
                continue;
            }
        };
        let (app, tx) = (app.clone(), tx.clone());
        tokio::spawn(async move {
            let reply = match app.server.handle(msg.request).await {
                Ok(result) => ServerMessage::Reply { id: msg.id, result },
                Err(error) => ServerMessage::Error {
                    id: Some(msg.id),
                    error,
                },
            };
            let _ = tx.send(reply);
        });
    }

    forwarder.abort();
    drop(tx);
    let _ = writer.await;
}

/// `--data <dir>`, else `NABUCODE_HOME`, else `~/.nabucode`.
fn data_dir() -> std::path::PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data"
            && let Some(dir) = args.next()
        {
            return dir.into();
        }
    }
    if let Some(home) = std::env::var_os("NABUCODE_HOME").filter(|v| !v.is_empty()) {
        return home.into();
    }
    dirs::home_dir().expect("home folder").join(".nabucode")
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("os random");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
