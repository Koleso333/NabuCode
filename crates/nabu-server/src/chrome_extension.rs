//! The Chrome extension this server carries, unpacked into a folder the user loads in Chrome once.
//! A newer NabuCode replaces it there; files changed by hand go back to the carried ones.

use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};

const PACKED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/extension.zip"));

/// The folder Chrome loads the extension from.
pub fn folder() -> Option<PathBuf> {
    Some(crate::chrome_host::folder()?.join("extension"))
}

/// The build of the extension in the folder: its `version_name`, which tells files apart.
pub fn installed_build() -> Option<String> {
    installed("version_name")
}

pub fn installed_version() -> Option<String> {
    installed("version")
}

fn installed(field: &str) -> Option<String> {
    let manifest = std::fs::read(folder()?.join("manifest.json")).ok()?;
    Some(serde_json::from_slice::<serde_json::Value>(&manifest).ok()?[field].as_str()?.to_owned())
}

/// Puts the carried extension into the folder unless the folder holds a newer one or the same files.
pub fn install() {
    let Some(target) = folder() else { return };
    let files = match carried() {
        Ok(files) if !files.is_empty() => files,
        Ok(_) => return,
        Err(e) => return eprintln!("[chrome] the carried extension is unreadable: {e}"),
    };
    if !should_replace(&target, &files) {
        return;
    }
    if let Err(e) = replace(&target, &files) {
        eprintln!("[chrome] putting the extension into {} failed: {e}", target.display());
    }
}

fn carried() -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(PACKED)).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    for index in 0..zip.len() {
        let mut file = zip.by_index(index).map_err(|e| e.to_string())?;
        let name = file.name().map_err(|e| e.to_string())?.into_owned();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        files.push((name, bytes));
    }
    Ok(files)
}

fn should_replace(target: &Path, files: &[(String, Vec<u8>)]) -> bool {
    let carried = files.iter().find(|(name, _)| name == "manifest.json").and_then(|(_, bytes)| version_of(bytes));
    let installed = std::fs::read(target.join("manifest.json")).ok().and_then(|bytes| version_of(&bytes));
    match (installed, carried) {
        (Some(installed), Some(carried)) if compare_versions(&installed, &carried) == Ordering::Greater => false,
        _ => !same_files(target, files),
    }
}

fn version_of(manifest: &[u8]) -> Option<String> {
    Some(serde_json::from_slice::<serde_json::Value>(manifest).ok()?["version"].as_str()?.to_owned())
}

/// Compares Chrome extension versions: up to four numbers joined by dots.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |v: &str| v.split('.').map(|part| part.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len())).map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0))).find(|o| o.is_ne()).unwrap_or(Ordering::Equal)
}

fn same_files(target: &Path, files: &[(String, Vec<u8>)]) -> bool {
    let mut on_disk = Vec::new();
    list(target, target, &mut on_disk);
    on_disk.len() == files.len() && files.iter().all(|(name, bytes)| std::fs::read(target.join(name)).is_ok_and(|disk| &disk == bytes))
}

fn list(root: &Path, dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        match path.is_dir() {
            true => list(root, &path, found),
            false => found.push(path.strip_prefix(root).unwrap_or(&path).to_owned()),
        }
    }
}

/// Writes the files beside the folder and swaps the folders, so Chrome never sees half of an update.
fn replace(target: &Path, files: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    let id = std::process::id();
    let fresh = target.with_file_name(format!("extension.new-{id}"));
    let old = target.with_file_name(format!("extension.old-{id}"));
    let _ = std::fs::remove_dir_all(&fresh);
    for (name, bytes) in files {
        let path = fresh.join(name);
        std::fs::create_dir_all(path.parent().unwrap_or(&fresh))?;
        std::fs::write(path, bytes)?;
    }
    if target.exists() {
        std::fs::rename(target, &old)?;
    }
    if let Err(e) = std::fs::rename(&fresh, target) {
        let _ = std::fs::rename(&old, target);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nabucode-extension-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("extension")
    }

    fn manifest(version: &str) -> (String, Vec<u8>) {
        ("manifest.json".into(), format!("{{\"version\": \"{version}\"}}").into_bytes())
    }

    #[test]
    fn versions_compare_by_numbers() {
        assert_eq!(compare_versions("0.1.10", "0.1.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.1", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2", "1.0"), Ordering::Less);
    }

    #[test]
    fn a_newer_extension_stays_and_a_changed_one_goes_back() {
        let target = temp("replace");
        let files = vec![manifest("0.2.0"), ("panel/index.html".into(), b"panel".to_vec())];
        assert!(should_replace(&target, &files));
        replace(&target, &files).unwrap();
        assert!(!should_replace(&target, &files));

        std::fs::write(target.join("panel/index.html"), b"edited").unwrap();
        assert!(should_replace(&target, &files));
        replace(&target, &files).unwrap();
        assert_eq!(std::fs::read(target.join("panel/index.html")).unwrap(), b"panel");

        std::fs::write(target.join("extra.js"), b"").unwrap();
        assert!(should_replace(&target, &files));

        std::fs::write(target.join("manifest.json"), manifest("0.3.0").1).unwrap();
        assert!(!should_replace(&target, &files));
        std::fs::remove_dir_all(target.parent().unwrap()).unwrap();
    }
}
