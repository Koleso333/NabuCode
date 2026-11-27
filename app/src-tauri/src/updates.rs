use std::sync::Mutex;

use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// One manifest per channel, written by scripts/release.ps1
const MANIFESTS: &str = "https://raw.githubusercontent.com/Koleso333/NabuCode/updates";

#[derive(Default)]
pub struct Updates {
    found: Mutex<Option<Update>>,
    download: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Serialize)]
pub struct Found {
    version: String,
    notes: Option<String>,
}

#[derive(Clone, Serialize)]
struct Progress {
    done: u64,
    total: Option<u64>,
}

/// The one-file build carries its server inside and has no installer to update it with
#[tauri::command]
pub fn updates_supported() -> bool {
    !cfg!(feature = "embed-server")
}

/// A release newer than this build in the channel; an older channel finds nothing until it overtakes it
#[tauri::command]
pub async fn update_check(app: AppHandle, updates: State<'_, Updates>, channel: String) -> Result<Option<Found>, String> {
    if !matches!(channel.as_str(), "stable" | "beta" | "nightly") {
        return Err(format!("Unknown channel: {channel}"));
    }
    let url = format!("{MANIFESTS}/{channel}.json").parse().map_err(|error| format!("{error}"))?;
    let updater = app.updater_builder().endpoints(vec![url]).and_then(|builder| builder.build()).map_err(|error| error.to_string())?;
    let update = match updater.check().await {
        Ok(update) => update,
        // GitHub answered without the manifest: no release in this channel yet
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => None,
        Err(error) => return Err(error.to_string()),
    };
    let found = update.as_ref().map(|update| Found { version: update.version.clone(), notes: update.body.clone() });
    *updates.found.lock().unwrap() = update;
    Ok(found)
}

/// Downloads the found update with "update-progress" events and starts its installer; NabuCode exits then
#[tauri::command]
pub fn update_install(app: AppHandle, updates: State<Updates>) -> Result<(), String> {
    let update = updates.found.lock().unwrap().clone().ok_or("No update to install")?;
    let task = tauri::async_runtime::spawn(async move {
        let mut done = 0;
        let report = |chunk: usize, total: Option<u64>| {
            done += chunk as u64;
            let _ = app.emit("update-progress", Progress { done, total });
        };
        let result = update.download(report, || {}).await.and_then(|bytes| update.install(bytes));
        if let Err(error) = result {
            let _ = app.emit("update-failed", error.to_string());
        }
    });
    if let Some(previous) = updates.download.lock().unwrap().replace(task) {
        previous.abort();
    }
    Ok(())
}

/// The downloaded part lives only in memory and goes with the task
#[tauri::command]
pub fn update_cancel(updates: State<Updates>) {
    if let Some(task) = updates.download.lock().unwrap().take() {
        task.abort();
    }
}
