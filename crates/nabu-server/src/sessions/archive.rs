use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

const INDEX: &str = "index.json";
const META: &str = "meta.json";

/// An archived session as the index lists it; its zip carries the same as `meta.json`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub updated: u64,
    /// The zip's name in the archive folder.
    pub zip: String,
}

/// Archived sessions: a zip per session with its subagents' sessions and attachments, and an index next to them.
pub struct Archive {
    dir: PathBuf,
    index: Mutex<Vec<Entry>>,
}

impl Archive {
    pub fn load(dir: PathBuf) -> Self {
        let index = read_index(&dir).unwrap_or_else(|| {
            let index = rebuild(&dir);
            if !index.is_empty()
                && let Err(e) = write_index(&dir, &index)
            {
                eprintln!("[archive] could not write the index: {e}");
            }
            index
        });
        Self { dir, index: Mutex::new(index) }
    }

    /// Packs the files, named as in the sessions folder, into the entry's zip and lists it.
    pub fn pack(&self, mut entry: Entry, files: &[(String, PathBuf)]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        entry.zip = format!("{}.zip", entry.id);
        let path = self.dir.join(&entry.zip);
        let tmp = path.with_extension("zip.tmp");
        write_zip(&tmp, &entry, files)?;
        std::fs::rename(&tmp, &path)?;
        let mut index = self.index.lock().unwrap();
        index.retain(|e| e.id != entry.id);
        index.push(entry);
        write_index(&self.dir, &index)
    }

    /// Unpacks the archived session into the sessions folder; returns the ids of the sessions it held.
    pub fn unpack(&self, id: &str, into: &Path) -> io::Result<Vec<String>> {
        let zip = self.zip_of(id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no archived session {id}")))?;
        let mut zip = zip::ZipArchive::new(File::open(self.dir.join(zip))?).map_err(io::Error::other)?;
        let mut ids = Vec::new();
        for i in 0..zip.len() {
            let mut item = zip.by_index(i).map_err(io::Error::other)?;
            let Some(name) = item.enclosed_name().filter(|name| name != Path::new(META)) else {
                continue;
            };
            if item.is_dir() {
                continue;
            }
            let path = into.join(&name);
            std::fs::create_dir_all(path.parent().unwrap_or(into))?;
            io::copy(&mut item, &mut File::create(&path)?)?;
            if name.parent() == Some(Path::new("")) && name.extension().is_some_and(|e| e == "json") {
                ids.extend(name.file_stem().map(|s| s.to_string_lossy().into_owned()));
            }
        }
        Ok(ids)
    }

    /// Drops the zip and its entry once the session is back among the others.
    pub fn forget(&self, id: &str) {
        let Some(zip) = self.zip_of(id) else {
            return;
        };
        let mut index = self.index.lock().unwrap();
        index.retain(|e| e.id != id);
        if let Err(e) = write_index(&self.dir, &index).and_then(|_| std::fs::remove_file(self.dir.join(zip))) {
            eprintln!("[archive] could not drop {id}: {e}");
        }
    }

    fn zip_of(&self, id: &str) -> Option<String> {
        self.index.lock().unwrap().iter().find(|e| e.id == id).map(|e| e.zip.clone())
    }
}

fn write_zip(path: &Path, entry: &Entry, files: &[(String, PathBuf)]) -> io::Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(path)?);
    let options = SimpleFileOptions::default();
    zip.start_file(META, options).map_err(io::Error::other)?;
    zip.write_all(&serde_json::to_vec_pretty(entry).expect("serializable"))?;
    for (name, file) in files {
        zip.start_file(name.as_str(), options).map_err(io::Error::other)?;
        io::copy(&mut File::open(file)?, &mut zip)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

fn read_index(dir: &Path) -> Option<Vec<Entry>> {
    serde_json::from_slice(&std::fs::read(dir.join(INDEX)).ok()?).ok()
}

fn write_index(dir: &Path, index: &[Entry]) -> io::Result<()> {
    let tmp = dir.join(format!("{INDEX}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(index).expect("serializable"))?;
    std::fs::rename(tmp, dir.join(INDEX))
}

/// The index read back from the zips' own metadata, when it is lost or broken.
fn rebuild(dir: &Path) -> Vec<Entry> {
    let Ok(items) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    items
        .flatten()
        .map(|item| item.path())
        .filter(|path| path.extension().is_some_and(|e| e == "zip"))
        .filter_map(|path| match meta(&path) {
            Ok(entry) => Some(entry),
            Err(e) => {
                eprintln!("[archive] skipping {}: {e}", path.display());
                None
            }
        })
        .collect()
}

fn meta(path: &Path) -> Result<Entry, String> {
    let mut zip = zip::ZipArchive::new(File::open(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let mut entry: Entry = serde_json::from_reader(zip.by_name(META).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    entry.zip = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    Ok(entry)
}
