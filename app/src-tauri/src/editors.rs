//! Editors installed on this machine, and opening a project file in one of them or in Explorer.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::fonts::blocking;

/// Known editors by their exe; the first name wins when several are found.
const KNOWN: &[&str] = &[
    "Code.exe", "Code - Insiders.exe", "Cursor.exe", "Windsurf.exe", "Zed.exe", "sublime_text.exe", "notepad++.exe",
    "idea64.exe", "webstorm64.exe", "pycharm64.exe", "rider64.exe", "clion64.exe", "goland64.exe", "rustrover64.exe",
    "phpstorm64.exe", "devenv.exe",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Editor {
    pub path: String,
    pub name: String,
    /// A PNG data URL of the program's icon.
    pub icon: String,
}

static FOUND: OnceLock<Vec<Editor>> = OnceLock::new();

#[tauri::command]
pub async fn editors() -> Result<Vec<Editor>, String> {
    if let Some(found) = FOUND.get() {
        return Ok(found.clone());
    }
    let found = blocking(|| describe(&find())).await?;
    Ok(FOUND.get_or_init(|| found).clone())
}

/// Name and icon of a program the user picked by hand.
#[tauri::command]
pub async fn editor_info(path: String) -> Result<Editor, String> {
    blocking(move || describe(&[PathBuf::from(path)])?.pop().ok_or_else(|| "Couldn't read the program".into())).await
}

/// Opens `file` in `editor`, or in the program Windows has for it when `editor` is empty.
#[tauri::command]
pub fn open_in_editor(file: String, editor: String) -> Result<(), String> {
    if editor.is_empty() {
        return tauri_plugin_opener::open_path(&file, None::<&str>).map_err(|error| error.to_string());
    }
    Command::new(&editor).arg(&file).spawn().map(|_| ()).map_err(|error| format!("{editor}: {error}"))
}

#[tauri::command]
pub fn reveal_in_explorer(path: String) -> Result<(), String> {
    let path = path.replace('/', "\\");
    let mut explorer = Command::new("explorer.exe");
    // A folder opens itself, a file opens its folder with it selected.
    // Explorer reads only `/select,"path"`: the default quoting of the whole argument sends it elsewhere
    let arg = if Path::new(&path).is_dir() { format!("\"{path}\"") } else { format!("/select,\"{path}\"") };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        explorer.raw_arg(arg);
    }
    #[cfg(not(windows))]
    explorer.arg(arg);
    // explorer.exe returns 1 even when it worked, so only a failed start is an error
    explorer.spawn().map(|_| ()).map_err(|error| error.to_string())
}

fn find() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(Path::new(&local).join("Programs"));
    }
    let mut folders = Vec::new();
    for root in &roots {
        for app in subfolders(root) {
            folders.push(app.join("bin"));
            folders.extend(subfolders(&app).into_iter().map(|inner| inner.join("bin"))); // JetBrains\<IDE>\bin
            folders.extend(subfolders(&app).into_iter().map(|inner| inner.join(r"Common7\IDE"))); // Visual Studio\<year>\<edition>
            folders.push(app);
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for exe in KNOWN {
        let hit = folders.iter().map(|folder| folder.join(exe)).find(|path| path.is_file());
        found.extend(hit);
    }
    if let Some(windows) = std::env::var_os("WINDIR") {
        found.push(Path::new(&windows).join(r"System32\notepad.exe"));
    }
    found
}

fn subfolders(folder: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect()
}

// .NET reads the name and icon from the exe in one PowerShell run, a second or so, once per launch
fn describe(paths: &[PathBuf]) -> Result<Vec<Editor>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let list = paths.iter().map(|path| format!("'{}'", path.display().to_string().replace('\'', "''"))).collect::<Vec<_>>().join(",");
    let script = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Add-Type -AssemblyName System.Drawing; \
         $items=@({list}) | ForEach-Object {{ $info=(Get-Item -LiteralPath $_).VersionInfo; \
         $name=if($info.FileDescription){{$info.FileDescription}}else{{[IO.Path]::GetFileNameWithoutExtension($_)}}; \
         $stream=New-Object IO.MemoryStream; [Drawing.Icon]::ExtractAssociatedIcon($_).ToBitmap().Save($stream,[Drawing.Imaging.ImageFormat]::Png); \
         [pscustomobject]@{{path=$_; name=$name; icon='data:image/png;base64,'+[Convert]::ToBase64String($stream.ToArray())}} }}; ConvertTo-Json -InputObject @($items) -Compress"
    );
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}
