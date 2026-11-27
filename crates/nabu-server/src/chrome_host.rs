//! How the NabuCode extension in Chrome finds running servers. An extension can't listen for connections, so each server
//! leaves its port and token in a shared folder, and Chrome asks `nabu` itself, run as a native messaging host, for them.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

const HOST: &str = "com.nabucode.chrome";
const EXTENSION_ID: &str = "hggchochnjbapkhjfejeaokeojaedkgd";

/// Chrome runs a native host with the extension's origin as its first argument.
pub fn is_host_call() -> bool {
    std::env::args().nth(1).is_some_and(|arg| arg.starts_with("chrome-extension://"))
}

pub(crate) fn folder() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("NabuCode").join("chrome"))
}

fn server_file() -> Option<PathBuf> {
    Some(folder()?.join("servers").join(format!("{}.json", std::process::id())))
}

/// Lists this server for the extension, puts the extension it carries in place and registers this `nabu` as the native host.
pub fn announce(port: u16, token: &str, data: &std::path::Path) {
    list(port, token, data);
    let exe = std::env::current_exe();
    std::thread::spawn(move || {
        crate::chrome_extension::install();
        if let Ok(exe) = exe {
            register(&exe);
        }
    });
}

pub(crate) fn list(port: u16, token: &str, data: &std::path::Path) {
    let Some(file) = server_file() else { return };
    let info = json!({ "port": port, "token": token, "data": data, "version": env!("CARGO_PKG_VERSION") });
    let written = std::fs::create_dir_all(file.parent().unwrap()).and_then(|()| std::fs::write(&file, info.to_string()));
    if let Err(e) = written {
        eprintln!("[chrome] listing the server in {} failed: {e}", file.display());
    }
}

/// Takes the server off the list when it exits.
pub fn withdraw() {
    if let Some(file) = server_file() {
        let _ = std::fs::remove_file(file);
    }
}

/// Makes `exe` the host Chrome runs for the extension.
pub(crate) fn register(exe: &std::path::Path) {
    let Some(folder) = folder() else { return };
    let manifest = folder.join(format!("{HOST}.json"));
    let info = json!({
        "name": HOST,
        "description": "NabuCode: running servers for the NabuCode extension",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")]
    });
    if let Err(e) = std::fs::write(&manifest, serde_json::to_string_pretty(&info).unwrap_or_default()) {
        eprintln!("[chrome] writing {} failed: {e}", manifest.display());
        return;
    }
    point_browsers_at(&manifest);
}

#[cfg(windows)]
fn point_browsers_at(manifest: &std::path::Path) {
    use std::os::windows::process::CommandExt;
    for browser in ["Google\\Chrome", "Chromium", "Microsoft\\Edge"] {
        let key = format!("HKCU\\Software\\{browser}\\NativeMessagingHosts\\{HOST}");
        let _ = std::process::Command::new("reg")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
            .arg(manifest)
            .arg("/f")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .status();
    }
}

/// Chrome on Linux and macOS finds hosts by their manifest's place.
#[cfg(not(windows))]
fn point_browsers_at(manifest: &std::path::Path) {
    let Some(home) = dirs::home_dir() else { return };
    let places = match cfg!(target_os = "macos") {
        true => ["Library/Application Support/Google/Chrome", "Library/Application Support/Chromium", "Library/Application Support/Microsoft Edge"],
        false => [".config/google-chrome", ".config/chromium", ".config/microsoft-edge"],
    };
    for place in places {
        let hosts = home.join(place).join("NativeMessagingHosts");
        if home.join(place).exists() && std::fs::create_dir_all(&hosts).is_ok() {
            let _ = std::fs::copy(manifest, hosts.join(format!("{HOST}.json")));
        }
    }
}

/// Answers the extension's one message with the running servers, then exits.
pub fn serve_host() {
    let mut stdin = std::io::stdin().lock();
    let mut size = [0u8; 4];
    if stdin.read_exact(&mut size).is_ok() {
        let mut message = vec![0u8; u32::from_le_bytes(size) as usize];
        let _ = stdin.read_exact(&mut message);
    }
    // The extension reloads itself when the folder holds another build than the one it runs
    let reply = json!({ "servers": running(), "extension": crate::chrome_extension::installed_build() }).to_string();
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&(reply.len() as u32).to_le_bytes());
    let _ = stdout.write_all(reply.as_bytes());
    let _ = stdout.flush();
}

/// Listed servers that still take connections; the lists of the gone ones are removed.
fn running() -> Vec<Value> {
    let Some(entries) = folder().and_then(|f| std::fs::read_dir(f.join("servers")).ok()) else { return Vec::new() };
    let mut servers = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let info = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let port = info.as_ref().and_then(|i| i["port"].as_u64()).unwrap_or(0) as u16;
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        match info {
            Some(info) if port != 0 && TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() => servers.push(info),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    servers
}
