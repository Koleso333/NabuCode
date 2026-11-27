use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use nabu_protocol::{ClientMessage, Event, Listening, ProtocolError, Reply, Request, ServerMessage};
use nabu_server::Server;
use shared::Holds;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

mod shared;

struct App {
    server: Server,
    token: String,
    holds: Holds,
}

const USAGE: &str = "Usage: nabu serve [--shared] [--data <folder>]\n\n  serve           runs the server for a NabuCode app; it prints its port and token and stops with the app\n  serve --shared  one server for all NabuCode windows: found through server.json, stops after the last window";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if nabu_server::chrome_host::is_host_call() {
        nabu_server::chrome_host::serve_host();
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() != Some("serve") {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let data = data_dir();
    let shared = std::env::args().any(|arg| arg == "--shared");
    let _lock = if shared { Some(lock_or_exit(&data)?) } else { None };
    let app = Arc::new(App {
        server: Server::new(&data),
        token: new_token(),
        holds: Holds::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    nabu_server::chrome_host::announce(port, &app.token, &data);

    if shared {
        shared::announce(&data, port, &app.token)?;
        tokio::spawn(shared::exit_when_unheld(app.holds.clone(), data));
    } else {
        let listening = Listening { port, token: app.token.clone() };
        println!("{}", serde_json::to_string(&listening).expect("serializable"));
        tokio::spawn(exit_with_parent());
    }
    let starter = app.clone();
    tokio::spawn(async move { starter.server.start().await });

    let router = axum::Router::new()
        .route("/", get(connect))
        .route("/hold", get(hold))
        .route("/terminal", get(connect_terminal))
        .route("/browser", get(connect_browser))
        .route("/chrome", get(connect_chrome))
        .route("/file", get(send_file))
        .route("/upload", post(receive_file).layer(DefaultBodyLimit::disable()))
        .with_state(app);
    axum::serve(listener, router).await
}

fn lock_or_exit(data: &std::path::Path) -> std::io::Result<std::fs::File> {
    let Some(lock) = shared::lock(data)? else {
        eprintln!("another server already serves {}", data.display());
        std::process::exit(shared::BUSY);
    };
    eprintln!("{} (version {})", shared::START_MARK, env!("CARGO_PKG_VERSION"));
    Ok(lock)
}

/// The app holds our stdin; EOF means it is gone.
async fn exit_with_parent() {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 64];
    while matches!(stdin.read(&mut buf).await, Ok(n) if n > 0) {}
    nabu_server::chrome_host::withdraw();
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

/// A window keeps the shared server alive while this socket is open.
async fn hold(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let hold = app.holds.take();
    Ok(ws.on_upgrade(move |mut socket| async move {
        let _hold = hold;
        while let Some(Ok(_)) = socket.recv().await {}
    }))
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

async fn connect_browser(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let id = query.get("id").cloned().unwrap_or_default();
    Ok(ws.on_upgrade(move |socket| nabu_server::browser_panel::stream(socket, id)))
}

/// The NabuCode extension in the user's Chrome.
async fn connect_chrome(
    ws: WebSocketUpgrade,
    Query(query): Query<HashMap<String, String>>,
    State(app): State<Arc<App>>,
) -> Result<Response, StatusCode> {
    if query.get("token") != Some(&app.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let instance = query.get("chrome").cloned().unwrap_or_default();
    Ok(ws.on_upgrade(move |socket| nabu_server::browser_panel::chrome_link(socket, instance)))
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

    // Temporary sessions this connection made: they end with it
    let temporary = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
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
        let (app, tx, temporary) = (app.clone(), tx.clone(), temporary.clone());
        let makes_temporary = matches!(msg.request, Request::SessionCreate { temporary: true, .. });
        tokio::spawn(async move {
            let reply = match app.server.handle(msg.request).await {
                Ok(result) => {
                    if let (true, Reply::Session(session)) = (makes_temporary, &result) {
                        temporary.lock().unwrap().push(session.id.clone());
                    }
                    ServerMessage::Reply { id: msg.id, result }
                }
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
    let ended: Vec<String> = std::mem::take(&mut *temporary.lock().unwrap());
    for session_id in ended {
        let _ = app.server.handle(Request::SessionDelete { session_id }).await;
    }
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
