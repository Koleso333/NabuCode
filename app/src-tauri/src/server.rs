use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// A server that lived shorter than this before exiting counts as a crash in a row.
const QUICK_CRASH: Duration = Duration::from_secs(30);
const CRASHES_BEFORE_GIVING_UP: u32 = 3;
const STDERR_LINES: usize = 20;

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Starting,
    Running { port: u16, token: String },
    Failed { message: String },
}

#[derive(Deserialize)]
struct Listening {
    port: u16,
    token: String,
}

/// The shell's own server: started in the background, restarted when it exits.
pub struct Sidecar {
    status: Arc<Mutex<Status>>,
    restart: Mutex<Sender<()>>,
}

impl Sidecar {
    pub fn start(app: AppHandle) -> Self {
        let status = Arc::new(Mutex::new(Status::Starting));
        let (restart, requests) = channel();
        let shared = status.clone();
        std::thread::spawn(move || supervise(app, shared, requests));
        Self { status, restart: Mutex::new(restart) }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn restart(&self) {
        let _ = self.restart.lock().unwrap().send(());
    }
}

fn supervise(app: AppHandle, status: Arc<Mutex<Status>>, restarts: Receiver<()>) {
    let publish = |next: Status| {
        *status.lock().unwrap() = next.clone();
        let _ = app.emit("server-status", next);
    };
    let mut crashes = 0;
    loop {
        publish(Status::Starting);
        let started = Instant::now();
        let message = match Process::spawn() {
            Ok(mut process) => {
                publish(Status::Running { port: process.listening.port, token: process.listening.token.clone() });
                process.wait()
            }
            Err(message) => message,
        };
        crashes = if started.elapsed() < QUICK_CRASH { crashes + 1 } else { 1 };
        if crashes >= CRASHES_BEFORE_GIVING_UP {
            publish(Status::Failed { message });
            if restarts.recv().is_err() {
                return;
            }
            while restarts.try_recv().is_ok() {}
            crashes = 0;
        }
    }
}

/// Holding stdin keeps the server alive: it exits when the pipe closes, so also when the shell dies.
struct Process {
    listening: Listening,
    child: Child,
    _stdin: ChildStdin,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Process {
    fn spawn() -> Result<Self, String> {
        let mut command = Command::new(server_path()?);
        command.arg("serve");
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|error| format!("Couldn't start the server: {error}"))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = keep_tail(child.stderr.take().expect("piped"));

        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line);
        match serde_json::from_str(&line) {
            Ok(listening) => Ok(Self { listening, child, _stdin: stdin, stderr }),
            Err(_) => {
                let _ = child.kill();
                let exit = child.wait().map(|code| code.to_string()).unwrap_or_default();
                let reason = read.err().map(|error| error.to_string()).unwrap_or(exit);
                Err(describe(&format!("The server didn't answer on start ({reason})"), &stderr))
            }
        }
    }

    fn wait(&mut self) -> String {
        let exit = self.child.wait().map(|code| code.to_string()).unwrap_or_else(|error| error.to_string());
        describe(&format!("The server stopped ({exit})"), &self.stderr)
    }
}

fn keep_tail(stderr: impl std::io::Read + Send + 'static) -> Arc<Mutex<VecDeque<String>>> {
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let lines = tail.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut lines = lines.lock().unwrap();
            if lines.len() == STDERR_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    });
    tail
}

fn describe(reason: &str, stderr: &Mutex<VecDeque<String>>) -> String {
    // The reader thread may still be catching the last lines of a process that just exited
    std::thread::sleep(Duration::from_millis(100));
    let lines = stderr.lock().unwrap();
    if lines.is_empty() {
        return reason.to_owned();
    }
    format!("{reason}\n{}", lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n"))
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
