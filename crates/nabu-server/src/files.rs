//! The project's files for the file panel: folder entries, git changes, patches, and a watcher that tells clients when disk changes.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use nabu_protocol::{Event, FileEntry, GitChange};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use similar::TextDiff;
use tokio::sync::broadcast;

use crate::projects::{git_output, same_folder};

/// Paths per `git check-ignore` call, to stay under the command line limit.
const IGNORE_BATCH: usize = 200;
/// Untracked files bigger than this count no lines and show no patch.
const TEXT_LIMIT: u64 = 2 * 1024 * 1024;

pub fn list(directory: &str, path: &str) -> Result<Vec<FileEntry>, String> {
    let folder = inside(directory, path)?;
    let mut entries: Vec<FileEntry> = std::fs::read_dir(&folder)
        .map_err(|err| format!("Could not read {}: {err}", folder.display()))?
        .flatten()
        .filter(|entry| entry.file_name() != ".git")
        .map(|entry| FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: entry.file_type().is_ok_and(|kind| kind.is_dir()),
            ignored: false,
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    mark_ignored(directory, path, &mut entries);
    Ok(entries)
}

// Outside git nothing is ignored, so errors just leave the marks off
fn mark_ignored(directory: &str, path: &str, entries: &mut [FileEntry]) {
    let relative = |name: &str| if path.is_empty() { name.to_owned() } else { format!("{path}/{name}") };
    for batch in entries.chunks_mut(IGNORE_BATCH) {
        let paths: Vec<String> = batch.iter().map(|entry| relative(&entry.name)).collect();
        let mut args = vec!["check-ignore", "--"];
        args.extend(paths.iter().map(String::as_str));
        let Ok(output) = git_output(directory, &args, &[0, 1]) else { return };
        let ignored: Vec<&str> = output.lines().map(|line| line.trim_end_matches('/')).collect();
        for (entry, path) in batch.iter_mut().zip(&paths) {
            entry.ignored = ignored.contains(&path.as_str());
        }
    }
}

pub fn changes(directory: &str) -> Result<Vec<GitChange>, String> {
    let prefix = git_output(directory, &["rev-parse", "--show-prefix"], &[0])?.trim().to_owned();
    let status = git_output(directory, &["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all", "--", "."], &[0])?;
    let counts = line_counts(directory, &prefix);
    let mut changes = Vec::new();
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        let (code, path) = field.split_at(3.min(field.len()));
        let code = code.trim_end();
        if code.contains('R') || code.contains('C') {
            fields.next(); // the old name
        }
        let path = path.strip_prefix(&prefix).unwrap_or(path).to_owned();
        let status = match code {
            "??" => "U",
            code if code.contains('D') => "D",
            code if code.contains('R') => "R",
            code if code.starts_with('A') => "A",
            _ => "M",
        };
        let (additions, deletions) = if status == "U" { (untracked_lines(directory, &path), 0) } else { counts.get(&path).copied().unwrap_or_default() };
        changes.push(GitChange { path, status: status.into(), additions, deletions });
    }
    Ok(changes)
}

// Without HEAD (no commits yet) the counts are just left at zero
fn line_counts(directory: &str, prefix: &str) -> HashMap<String, (usize, usize)> {
    let Ok(output) = git_output(directory, &["-c", "core.quotePath=false", "--no-optional-locks", "diff", "HEAD", "--numstat", "--no-renames", "--", "."], &[0]) else {
        return HashMap::new();
    };
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?.parse().unwrap_or(0);
            let deleted = parts.next()?.parse().unwrap_or(0);
            let path = parts.next()?;
            Some((path.strip_prefix(prefix).unwrap_or(path).to_owned(), (added, deleted)))
        })
        .collect()
}

/// Line counts of untracked files by size and change time, so a refresh only reads files that changed.
static LINE_COUNTS: LazyLock<Mutex<HashMap<PathBuf, (u64, Option<SystemTime>, usize)>>> = LazyLock::new(Mutex::default);

fn untracked_lines(directory: &str, path: &str) -> usize {
    let file = Path::new(directory).join(path);
    let Ok(meta) = std::fs::metadata(&file) else { return 0 };
    if meta.len() > TEXT_LIMIT {
        return 0;
    }
    let stamp = (meta.len(), meta.modified().ok());
    if let Some(&(size, modified, lines)) = LINE_COUNTS.lock().unwrap().get(&file)
        && (size, modified) == stamp
    {
        return lines;
    }
    let lines = std::fs::read(&file).ok().filter(|bytes| std::str::from_utf8(bytes).is_ok()).map_or(0, |bytes| count_lines(&bytes));
    LINE_COUNTS.lock().unwrap().insert(file, (stamp.0, stamp.1, lines));
    lines
}

fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count() + usize::from(bytes.last().is_some_and(|&byte| byte != b'\n'))
}

pub fn diff(directory: &str, path: &str) -> Result<String, String> {
    let file = inside(directory, path)?;
    let tracked = git_output(directory, &["ls-files", "--error-unmatch", "--", path], &[0]).is_ok();
    if tracked {
        return git_output(directory, &["--no-optional-locks", "diff", "HEAD", "--no-color", "--", path], &[0]);
    }
    let text = small_text(&file).unwrap_or_default();
    Ok(TextDiff::from_lines("", &text).unified_diff().header(path, path).to_string())
}

fn small_text(file: &Path) -> Option<String> {
    let size = std::fs::metadata(file).ok()?.len();
    if size > TEXT_LIMIT {
        return None;
    }
    std::fs::read_to_string(file).ok().map(|text| text.replace("\r\n", "\n"))
}

// Keeps requests inside the project folder
fn inside(directory: &str, path: &str) -> Result<PathBuf, String> {
    if Path::new(path).components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
        return Err(format!("Path must stay inside the project: {path}"));
    }
    Ok(Path::new(directory).join(path))
}

/// Folder watchers; each folder is watched once and sends `FilesChanged` after the disk calms down.
pub struct FileWatchers {
    watched: Mutex<Vec<(String, RecommendedWatcher)>>,
    events: broadcast::Sender<Event>,
}

/// Quiet time that ends a burst of changes.
const SETTLE: Duration = Duration::from_millis(300);
/// A burst that never calms down (an install, a build) still reports this often.
const LONGEST_WAIT: Duration = Duration::from_secs(2);

impl FileWatchers {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self { watched: Mutex::default(), events }
    }

    pub fn watch(&self, directory: &str) -> Result<(), String> {
        let mut watched = self.watched.lock().unwrap();
        if watched.iter().any(|(folder, _)| same_folder(folder, directory)) {
            return Ok(());
        }
        let (changed, calls) = mpsc::channel();
        let root = PathBuf::from(directory);
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            if result.is_ok_and(|event| event.paths.iter().any(|path| matters(&root, path))) {
                let _ = changed.send(());
            }
        })
        .map_err(|err| format!("Could not watch {directory}: {err}"))?;
        watcher
            .watch(Path::new(directory), RecursiveMode::Recursive)
            .map_err(|err| format!("Could not watch {directory}: {err}"))?;
        let events = self.events.clone();
        let folder = directory.to_owned();
        std::thread::spawn(move || {
            while calls.recv().is_ok() {
                let started = Instant::now();
                while started.elapsed() < LONGEST_WAIT && calls.recv_timeout(SETTLE).is_ok() {}
                let _ = events.send(Event::FilesChanged { directory: folder.clone() });
            }
        });
        watched.push((directory.to_owned(), watcher));
        Ok(())
    }
}

// Inside .git only a moved HEAD or ref changes what the panel shows; the rest is git's own bookkeeping
fn matters(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return true };
    let mut parts = relative.components().map(|part| part.as_os_str().to_string_lossy());
    if parts.next().as_deref() != Some(".git") {
        return true;
    }
    matches!(parts.next().as_deref(), Some("HEAD" | "refs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_project() {
        assert!(inside("C:/project", "src/main.rs").is_ok());
        assert!(inside("C:/project", "../secret").is_err());
        assert!(inside("C:/project", "C:/Windows").is_err());
    }

    #[test]
    fn only_head_and_refs_matter_inside_git() {
        let root = Path::new("C:/project");
        assert!(matters(root, Path::new("C:/project/src/main.rs")));
        assert!(matters(root, Path::new("C:/project/.git/HEAD")));
        assert!(matters(root, Path::new("C:/project/.git/refs/heads/main")));
        assert!(!matters(root, Path::new("C:/project/.git/index")));
        assert!(!matters(root, Path::new("C:/project/.git/objects/ab/cdef")));
    }
}
