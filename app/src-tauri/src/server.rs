use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// A server that lived shorter than this before exiting counts as a crash in a row.
const QUICK_CRASH: Duration = Duration::from_secs(30);
const CRASHES_BEFORE_GIVING_UP: u32 = 3;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const LOG_LINES: usize = 20;
const LOG_LIMIT: u64 = 1 << 20;
/// Kept in step with nabu-server's shared.rs.
const BUSY: i32 = 3;
const START_MARK: &str = "--- nabu started";

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Starting,
    Running { port: u16, token: String },
    Failed { message: String },
}

/// What a running server wrote to `server.json`.
#[derive(Deserialize)]
struct Found {
    port: u16,
    token: String,
    protocol: u32,
}

/// The server shared by all NabuCode windows: found or started in the background, started again when it exits.
pub struct Sidecar {
    status: Arc<Mutex<Status>>,
    restart: Mutex<Sender<()>>,
}

impl Sidecar {
    pub fn start(app: AppHandle) -> Self {
        let status = Arc::new(Mutex::new(Status::Starting));
        let (restart, requests) = channel();
        let shared = status.clone();
        std::thread::spawn(move || supervise(app, data_dir(), shared, requests));
        Self { status, restart: Mutex::new(restart) }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn restart(&self) {
        let _ = self.restart.lock().unwrap().send(());
    }
}

fn supervise(app: AppHandle, data: PathBuf, status: Arc<Mutex<Status>>, restarts: Receiver<()>) {
    let publish = |next: Status| {
        *status.lock().unwrap() = next.clone();
        let _ = app.emit("server-status", next);
    };
    let mut crashes = 0;
    loop {
        publish(Status::Starting);
        let started = Instant::now();
        let message = match attach(&data) {
            Ok(Attached::Held(mut hold)) => {
                publish(Status::Running { port: hold.found.port, token: hold.found.token.clone() });
                hold.wait();
                describe("The server stopped", &data)
            }
            // The window tells about the other version; that server stays untouched until the user retries
            Ok(Attached::Foreign(found)) => {
                publish(Status::Running { port: found.port, token: found.token });
                if !wait_restart(&restarts) {
                    return;
                }
                continue;
            }
            Err(message) => message,
        };
        crashes = if started.elapsed() < QUICK_CRASH { crashes + 1 } else { 1 };
        if crashes >= CRASHES_BEFORE_GIVING_UP {
            publish(Status::Failed { message });
            if !wait_restart(&restarts) {
                return;
            }
            crashes = 0;
        }
    }
}

/// False when the window is gone.
fn wait_restart(restarts: &Receiver<()>) -> bool {
    while restarts.try_recv().is_ok() {}
    let asked = restarts.recv().is_ok();
    while restarts.try_recv().is_ok() {}
    asked
}

enum Attached {
    Held(Hold),
    Foreign(Found),
}

fn attach(data: &Path) -> Result<Attached, String> {
    if let Some(attached) = find(data) {
        return Ok(attached);
    }
    let mut child = spawn(data)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(attached) = find(data) {
            return Ok(attached);
        }
        // BUSY: another window started a server at the same moment, so its server.json is coming
        if let Ok(Some(exit)) = child.try_wait()
            && exit.code() != Some(BUSY)
        {
            return Err(describe(&format!("The server didn't start ({exit})"), data));
        }
        if Instant::now() > deadline {
            return Err(describe("The server didn't answer on start", data));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn find(data: &Path) -> Option<Attached> {
    let text = std::fs::read_to_string(data.join("server.json")).ok()?;
    let found: Found = serde_json::from_str(&text).ok()?;
    let stream = open_hold(&found)?;
    if found.protocol != nabu_protocol::PROTOCOL_VERSION {
        return Some(Attached::Foreign(found));
    }
    Some(Attached::Held(Hold { found, stream }))
}

/// Its output goes to `server.log`: the server outlives this window, so it can't write into our pipes.
fn spawn(data: &Path) -> Result<std::process::Child, String> {
    let fail = |error: std::io::Error| format!("Couldn't start the server: {error}");
    std::fs::create_dir_all(data).map_err(fail)?;
    let log = std::fs::File::options().create(true).append(true).open(data.join("server.log")).map_err(fail)?;
    if log.metadata().is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        let _ = log.set_len(0);
    }
    let mut command = Command::new(server_path()?);
    command.args(["serve", "--shared", "--data"]).arg(data);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map_err(fail)
}

/// The socket that keeps the shared server alive while this window is open.
struct Hold {
    found: Found,
    stream: TcpStream,
}

impl Hold {
    /// Returns when the server is gone.
    fn wait(&mut self) {
        let mut buf = [0u8; 256];
        while matches!(self.stream.read(&mut buf), Ok(n) if n > 0) {}
    }
}

/// A websocket handshake by hand; nothing is sent over it afterwards.
fn open_hold(found: &Found) -> Option<TcpStream> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], found.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let request = format!(
        "GET /hold?token={} HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: bmFidWNvZGUtaG9sZC0xMg==\r\n\r\n",
        found.token
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while !reply.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).ok()?;
        reply.push(byte[0]);
    }
    if !reply.starts_with(b"HTTP/1.1 101") {
        return None;
    }
    stream.set_read_timeout(None).ok()?;
    Some(stream)
}

/// The reason with the last lines of the server's latest run.
fn describe(reason: &str, data: &Path) -> String {
    let log = std::fs::read_to_string(data.join("server.log")).unwrap_or_default();
    let run = log.rfind(START_MARK).map_or(log.as_str(), |start| &log[start..]);
    let lines: Vec<&str> = run.lines().collect();
    let tail = &lines[lines.len().saturating_sub(LOG_LINES)..];
    if tail.is_empty() {
        return reason.to_owned();
    }
    format!("{reason}\n{}", tail.join("\n"))
}

/// The same rule as the server's: `NABUCODE_HOME`, else `~/.nabucode`.
fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("NABUCODE_HOME").filter(|value| !value.is_empty()) {
        return home.into();
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".nabucode")
}

#[cfg(not(feature = "embed-server"))]
fn server_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = exe.with_file_name(format!("nabu{}", std::env::consts::EXE_SUFFIX));
    if !path.exists() {
        return Err(format!("Server not found: {}", path.display()));
    }
    Ok(path)
}

/// Unpacks the embedded server once per build; the hash in the name keeps a running older copy untouched.
#[cfg(feature = "embed-server")]
fn server_path() -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};

    static SERVER: &[u8] = include_bytes!(env!("NABUCODE_SERVER_EXE"));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SERVER.hash(&mut hasher);

    let dir = std::env::temp_dir().join("NabuCode");
    let path = dir.join(format!("nabu-{:016x}{}", hasher.finish(), std::env::consts::EXE_SUFFIX));
    let unpacked = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == SERVER.len() as u64);
    if !unpacked {
        let fail = |error: std::io::Error| format!("Couldn't unpack the server: {error}");
        std::fs::create_dir_all(&dir).map_err(fail)?;
        let part = path.with_extension("part");
        std::fs::write(&part, SERVER).map_err(fail)?;
        std::fs::rename(&part, &path).map_err(fail)?;
    }
    Ok(path)
}
