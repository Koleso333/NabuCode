use std::path::{Path, PathBuf};

/// The folder holding `.git` at or above `directory`.
pub fn root(directory: &Path) -> Option<PathBuf> {
    directory
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The checked-out branch, or `detached at <hash>`.
pub fn branch(directory: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir(&root(directory)?)?.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_owned()),
        None => Some(format!("detached at {}", head.get(..7)?)),
    }
}

/// `.git` itself, or where a worktree's `.git` file points.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let link = std::fs::read_to_string(&dot).ok()?;
    let target = PathBuf::from(link.trim().strip_prefix("gitdir:")?.trim());
    Some(if target.is_absolute() {
        target
    } else {
        root.join(target)
    })
}
