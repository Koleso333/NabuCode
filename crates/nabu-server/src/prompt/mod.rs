use std::path::{Path, PathBuf};

use ncore::core::ir::Block;

pub(crate) mod git;

const SYSTEM: &str = include_str!("system.md");

const SUBAGENT: &str = "In this session you are a subagent: another agent working with the person gave you the task in the first message. \
It cannot answer questions while you work, so decide yourself and note your assumptions. Your final message goes back to that agent \
as your report, and only it: make it self-contained, with what you did, what you found (paths, line numbers, command results) \
and anything left undone. The person can open this session and watch.";

const AUTHORITY: &str = "These are the person's own instructions and take precedence over this prompt. \
Where files conflict, the more specific one wins; the person's messages win over all files.";

/// What the model is told about the session it works in.
pub struct Context<'a> {
    pub model_name: &'a str,
    pub model_id: &'a str,
    pub directory: &'a Path,
    /// Started by another agent with the `agent` tool.
    pub subagent: bool,
}

/// System blocks: stable text first, the per-request environment last so the cached prefix survives.
pub fn system(context: &Context) -> Vec<Block> {
    let mut stable = SYSTEM.trim_end().to_owned();
    if context.subagent {
        stable.push_str("\n\n");
        stable.push_str(SUBAGENT);
    }
    let files = instructions(context.directory);
    if !files.is_empty() {
        stable.push_str("\n\n");
        stable.push_str(&files);
        stable.push_str("\n\n");
        stable.push_str(AUTHORITY);
    }
    vec![Block::text(stable), Block::text(env(context))]
}

/// Instruction files from general to specific, each tagged with its path and scope.
fn instructions(directory: &Path) -> String {
    let global = dirs::home_dir().map(|home| home.join(".nabucode"));
    let project = git::root(directory).unwrap_or_else(|| directory.to_owned());
    let mut blocks: Vec<String> = global
        .iter()
        .filter_map(|dir| file_block(dir, "global"))
        .collect();
    blocks.extend(
        project_dirs(&project, directory)
            .iter()
            .filter_map(|dir| file_block(dir, "project")),
    );
    blocks.join("\n\n")
}

/// The project root and every folder between it and `directory`.
pub(crate) fn project_dirs(root: &Path, directory: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = directory
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// The first of NABU.md, AGENTS.md, CLAUDE.md found in a folder.
fn file_block(dir: &Path, scope: &str) -> Option<String> {
    let (path, text) = ["NABU.md", "AGENTS.md", "CLAUDE.md"]
        .iter()
        .find_map(|name| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).ok().map(|text| (path, text))
        })?;
    let text = text.trim();
    (!text.is_empty()).then(|| {
        format!(
            "<instructions source=\"{}\" scope=\"{scope}\">\n{text}\n</instructions>",
            path.display()
        )
    })
}

fn env(context: &Context) -> String {
    let git = git::branch(context.directory).unwrap_or_else(|| "not a repository".into());
    let lines = [
        format!("Model: {} ({})", context.model_name, context.model_id),
        format!("Working directory: {}", context.directory.display()),
        format!("Git: {git}"),
        format!("Platform: {}", platform()),
        format!("Date: {}", chrono::Local::now().format("%Y-%m-%d")),
    ];
    format!("<env>\n{}\n</env>", lines.join("\n"))
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_files_from_root_down() {
        let root = std::env::temp_dir().join(format!("nabucode-prompt-{}", std::process::id()));
        let sub = root.join("app");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("NABU.md"), "root rules").unwrap();
        std::fs::write(root.join("AGENTS.md"), "ignored").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "app rules").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "ignored").unwrap();

        let text = instructions(&sub);
        let first = text.find("root rules").unwrap();
        assert!(first < text.find("app rules").unwrap());
        assert!(!text.contains("ignored"));
        assert_eq!(git::branch(&sub).as_deref(), Some("main"));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
