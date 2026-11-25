#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dropped;
mod editors;
mod fonts;
mod server;

use std::path::PathBuf;
use server::{Sidecar, Status};
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, webview::Color};

#[tauri::command]
fn show_main(window: WebviewWindow) -> Result<(), String> {
    window.show().map_err(|error| error.to_string())
}

#[tauri::command]
fn server_status(sidecar: tauri::State<Sidecar>) -> Status {
    sidecar.status()
}

#[tauri::command]
fn server_restart(sidecar: tauri::State<Sidecar>) {
    sidecar.restart();
}

/// Only scripts/release.ps1 sets NABUCODE_RELEASE; any other build from source is marked -dev
#[tauri::command]
fn app_version(app: tauri::AppHandle) -> String {
    let version = app.package_info().version.to_string();
    if option_env!("NABUCODE_RELEASE").is_some() { version } else { format!("{version}-dev") }
}

fn project_arg() -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix("--project=").map(str::to_owned))
}

#[derive(serde::Serialize)]
struct Folder {
    path: String,
    name: String,
}

/// Where a new chat goes when no project is picked: the folder given with --project=, else the home folder
#[tauri::command]
fn start_folder() -> Folder {
    if let Some(path) = project_arg() {
        let name = std::path::Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
        return Folder { path, name };
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    Folder { path: home.to_string_lossy().into_owned(), name: "Home".into() }
}

/// The demo shows the NabuCode checkout it was started from
#[tauri::command]
fn project_root() -> String {
    if let Some(path) = project_arg() {
        return path;
    }
    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    loop {
        if path.join("crates/nabu-server/Cargo.toml").exists() {
            return path.to_string_lossy().into_owned();
        }
        if !path.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).to_string_lossy().into_owned();
        }
    }
}

#[tauri::command]
fn theme_choice() -> Option<serde_json::Value> {
    let path = std::env::var_os("LOCALAPPDATA")?;
    let text = std::fs::read_to_string(PathBuf::from(path).join("EveryEnd/theme.json")).ok()?;
    serde_json::from_str(&text).ok()
}

#[tauri::command]
fn open_project(path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("Couldn't open the folder: {path}"));
    }
    #[cfg(windows)]
    return std::process::Command::new("explorer.exe")
        .arg(path.replace('/', "\\"))
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string());
    #[cfg(not(windows))]
    return Err("Opening a folder works only on Windows for now".into());
}

/// A WebView2 update can leave its registry entry on a deleted version folder, and the loader then
/// reports no runtime; in that case the installed version is given to it by path.
#[cfg(windows)]
fn find_webview_runtime() {
    use std::path::Path;
    use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

    if std::env::var_os("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER").is_some() {
        return;
    }
    let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    let read = |kind: &str, name: &str| {
        let key = format!(r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\{kind}\{{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}}");
        machine.open_subkey(key).ok()?.get_value::<String, _>(name).ok()
    };
    if read("ClientState", "EBWebView").is_some_and(|folder| Path::new(&folder).is_dir()) {
        return;
    }
    let (Some(location), Some(version)) = (read("Clients", "location"), read("Clients", "pv")) else {
        return;
    };
    let folder = Path::new(&location).join(version);
    if folder.join("msedgewebview2.exe").exists() {
        // Safe: runs first in main, before any other thread exists
        unsafe { std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", folder) };
    }
}

fn main() {
    #[cfg(windows)]
    find_webview_runtime();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            app.manage(Sidecar::start(app.handle().clone()));
            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("/".into()))
                .title("NabuCode")
                .inner_size(970.0, 621.0)
                .position(10.0, 10.0)
                .min_inner_size(800.0, 500.0)
                .background_color(Color(18, 18, 18, 255))
                .visible(false);
            #[cfg(windows)]
            let builder = builder.decorations(false);
            let window = builder.build()?;
            if matches!(window.theme(), Ok(tauri::Theme::Light)) {
                window.set_background_color(Some(Color(248, 248, 248, 255)))?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![app_version, server_status, server_restart, start_folder, project_root, theme_choice, open_project, show_main, fonts::font_catalog, fonts::font_install, dropped::dropped_items, dropped::read_dropped, dropped::clipboard_files, editors::editors, editors::editor_info, editors::open_in_editor, editors::reveal_in_explorer])
        .run(tauri::generate_context!())
        .expect("failed to run NabuCode");
}
