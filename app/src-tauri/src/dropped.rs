//! Files and folders dropped on the window or picked come as paths; the page asks here what they are and reads previews.

use std::path::{Path, PathBuf};
use serde::Serialize;
use tauri::ipc::Response;

#[derive(Serialize)]
pub struct DroppedItem {
    path: String,
    name: String,
    folder: bool,
    mime: String,
    size: u64,
}

#[tauri::command]
pub fn dropped_items(paths: Vec<PathBuf>) -> Vec<DroppedItem> {
    paths.into_iter().map(target).map(|path| DroppedItem {
        name: path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned()),
        folder: path.is_dir(),
        mime: mime_guess::from_path(&path).first().map(|mime| mime.to_string()).unwrap_or_default(),
        size: path.metadata().map_or(0, |meta| meta.len()),
        path: path.display().to_string(),
    }).collect()
}

#[tauri::command]
pub fn read_dropped(path: PathBuf) -> Result<Response, String> {
    std::fs::read(&path).map(Response::new).map_err(|e| format!("{}: {e}", path.display()))
}

/// Files copied in Explorer are paths on the Windows clipboard; the page sees only their bytes, so it asks here.
#[tauri::command]
pub fn clipboard_files() -> Vec<PathBuf> {
    copied_files()
}

#[cfg(windows)]
fn copied_files() -> Vec<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() || OpenClipboard(None).is_err() {
            return Vec::new();
        }
        let paths = GetClipboardData(CF_HDROP.0 as u32).map_or_else(|_| Vec::new(), |data| {
            let drop = HDROP(data.0);
            (0..DragQueryFileW(drop, u32::MAX, None)).filter_map(|index| {
                let mut buffer = vec![0u16; DragQueryFileW(drop, index, None) as usize + 1];
                let len = DragQueryFileW(drop, index, Some(&mut buffer)) as usize;
                (len > 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..len])))
            }).collect()
        });
        let _ = CloseClipboard();
        paths
    }
}

#[cfg(not(windows))]
fn copied_files() -> Vec<PathBuf> {
    Vec::new()
}

/// A shortcut stands for the file or folder it points at; a broken one stays itself.
fn target(path: PathBuf) -> PathBuf {
    let shortcut = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"));
    if !shortcut {
        return path;
    }
    shortcut_target(&path).filter(|target| target.exists()).unwrap_or(path)
}

#[cfg(windows)]
fn shortcut_target(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let read = || -> Option<PathBuf> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            link.cast::<IPersistFile>().ok()?.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;
            let mut buffer = vec![0u16; 32768];
            link.GetPath(&mut buffer, std::ptr::null_mut(), 0).ok()?;
            let len = buffer.iter().position(|&c| c == 0)?;
            (len > 0).then(|| PathBuf::from(OsString::from_wide(&buffer[..len])))
        };
        let target = read();
        if com.is_ok() {
            CoUninitialize();
        }
        target
    }
}

#[cfg(not(windows))]
fn shortcut_target(_: &Path) -> Option<PathBuf> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    fn wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    /// Touches the real clipboard, so only by hand: copy files in Explorer, then run with --ignored.
    #[test]
    #[ignore]
    fn copied_files_come_as_paths() {
        println!("{:?}", clipboard_files());
    }

    #[test]
    fn a_dropped_shortcut_becomes_its_target() {
        let dir = std::env::temp_dir().join(format!("everyend-lnk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("app.exe");
        std::fs::write(&app, b"MZ").unwrap();
        let shortcut = dir.join("App.lnk");
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(PCWSTR(wide(&app).as_ptr())).unwrap();
            link.cast::<IPersistFile>().unwrap().Save(PCWSTR(wide(&shortcut).as_ptr()), true).unwrap();
        }
        let items = dropped_items(vec![shortcut.clone()]);
        assert_eq!((items[0].name.as_str(), items[0].path.as_str()), ("app.exe", app.display().to_string().as_str()));
        std::fs::remove_file(&app).unwrap();
        assert_eq!(dropped_items(vec![shortcut])[0].name, "App.lnk", "a broken shortcut stays itself");
        std::fs::remove_dir_all(dir).ok();
    }
}
