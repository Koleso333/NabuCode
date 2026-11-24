use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nabu_protocol::{Event, Project};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::prompt::git;

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    path: String,
    name: String,
}

/// The user's project folders in `projects.json`, in their order, with the branch each is on.
pub struct Projects {
    file: PathBuf,
    saved: Mutex<Vec<Saved>>,
    /// What clients were last told, so a branch switched outside NabuCode is noticed.
    shown: Mutex<Vec<Project>>,
    events: broadcast::Sender<Event>,
}

impl Projects {
    pub fn load(dir: &Path, events: broadcast::Sender<Event>) -> Arc<Self> {
        let file = dir.join("projects.json");
        let saved: Vec<Saved> = std::fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        let projects = Arc::new(Self {
            file,
            saved: Mutex::new(saved),
            shown: Mutex::default(),
            events,
        });
        *projects.shown.lock().unwrap() = projects.list();
        projects.watch_branches();
        projects
    }

    pub fn list(&self) -> Vec<Project> {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .map(|saved| Project {
                path: saved.path.clone(),
                name: saved.name.clone(),
                branch: git::branch(Path::new(&saved.path)),
            })
            .collect()
    }

    pub fn add(&self, path: String, name: String) -> Result<Vec<Project>, String> {
        self.change(|saved| {
            if !saved.iter().any(|item| same_folder(&item.path, &path)) {
                saved.push(Saved { path, name });
            }
        })
    }

    pub fn rename(&self, path: &str, name: String) -> Result<Vec<Project>, String> {
        self.change(|saved| {
            if let Some(item) = saved.iter_mut().find(|item| same_folder(&item.path, path)) {
                item.name = name;
            }
        })
    }

    pub fn remove(&self, path: &str) -> Result<Vec<Project>, String> {
        self.change(|saved| saved.retain(|item| !same_folder(&item.path, path)))
    }

    pub fn reorder(&self, paths: &[String]) -> Result<Vec<Project>, String> {
        self.change(|saved| {
            let rank = |item: &Saved| paths.iter().position(|path| same_folder(path, &item.path)).unwrap_or(paths.len());
            saved.sort_by_key(rank);
        })
    }

    fn change(&self, edit: impl FnOnce(&mut Vec<Saved>)) -> Result<Vec<Project>, String> {
        {
            let mut saved = self.saved.lock().unwrap();
            let mut next = saved.clone();
            edit(&mut next);
            write(&self.file, &next).map_err(|err| format!("Could not save the projects: {err}"))?;
            *saved = next;
        }
        Ok(self.announce())
    }

    /// Sends the list to clients; also right after a switch, without waiting for the watcher.
    pub fn announce(&self) -> Vec<Project> {
        let projects = self.list();
        *self.shown.lock().unwrap() = projects.clone();
        let _ = self.events.send(Event::ProjectsChanged { projects: projects.clone() });
        projects
    }

    // HEAD is a tiny file, so reading it for every project each second costs nothing
    fn watch_branches(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(projects) = this.upgrade() else {
                    return;
                };
                let changed = projects.list() != *projects.shown.lock().unwrap();
                if changed {
                    projects.announce();
                }
            }
        });
    }
}

pub fn branches(directory: &str) -> Result<Vec<String>, String> {
    let output = run_git(directory, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)", "refs/heads"])?;
    Ok(output.lines().map(str::to_owned).collect())
}

pub fn switch(directory: &str, branch: &str) -> Result<(), String> {
    run_git(directory, &["switch", branch]).map(|_| ())
}

fn run_git(directory: &str, args: &[&str]) -> Result<String, String> {
    git_output(directory, args, &[0])
}

/// Stdout of git when it exits with one of `codes`; some commands use 1 for "nothing found".
pub(crate) fn git_output(directory: &str, args: &[&str], codes: &[i32]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command.output().map_err(|err| format!("Could not run git: {err}"))?;
    if !output.status.code().is_some_and(|code| codes.contains(&code)) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn same_folder(a: &str, b: &str) -> bool {
    let key = |path: &str| path.replace('\\', "/").trim_end_matches('/').to_lowercase();
    key(a) == key(b)
}

fn write(file: &Path, saved: &[Saved]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved)?)?;
    std::fs::rename(tmp, file)
}
