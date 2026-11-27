//! Skills: folders with a SKILL.md from NabuCode, Claude Code and Codex, and Claude Code's old command files.

use std::path::{Path, PathBuf};

use nabu_protocol::{SkillHarness, SkillInfo, SkillScope, SkillSource};

use crate::prompt;

/// Skill marks one message may hold.
pub const MAX_MARKS: usize = 10;

/// How long a description may be in the list the model gets.
const LISTED: usize = 250;

/// How much of the text the settings show.
const PREVIEW: usize = 300;

/// Claude desktop's skills inside Claude Code's global folder.
const SYNCED: &str = "synced";

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub described: bool,
    pub preview: String,
    pub enabled: bool,
    /// The SKILL.md, or the command file.
    pub path: PathBuf,
    pub harness: SkillHarness,
    pub scope: SkillScope,
}

impl Skill {
    /// The skill's folder; its files are named relative to it.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }

    /// The instructions, without the header.
    pub fn body(&self) -> Result<String, String> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| format!("cannot read {}: {e}", self.path.display()))?;
        Ok(header(&text).1.trim().to_owned())
    }

    pub fn info(&self) -> SkillInfo {
        SkillInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            described: self.described,
            preview: self.preview.clone(),
            enabled: self.enabled,
            path: self.path.display().to_string(),
            harness: self.harness,
            scope: self.scope,
        }
    }

    /// One line of the list the model gets.
    pub fn listed(&self) -> String {
        let description = self.description.split_whitespace().collect::<Vec<_>>().join(" ");
        match description.char_indices().nth(LISTED) {
            Some((cut, _)) => format!("- {}: {}…", self.name, description[..cut].trim_end()),
            None => format!("- {}: {description}", self.name),
        }
    }
}

/// A skill's instructions as the model gets them, from a mark or from the `skill` tool.
pub fn content(name: &str, dir: &str, body: &str) -> String {
    format!("<skill-content name=\"{name}\" dir=\"{dir}\">\n{body}\n</skill-content>")
}

/// NabuCode's first, a project's before the global ones of the same tool; Codex's and Claude desktop's are off until turned on.
pub fn default_sources() -> Vec<SkillSource> {
    let harnesses = [SkillHarness::Nabucode, SkillHarness::Claude, SkillHarness::Codex];
    let mut sources: Vec<SkillSource> = harnesses
        .into_iter()
        .flat_map(|harness| {
            let enabled = harness != SkillHarness::Codex;
            [SkillScope::Project, SkillScope::Global].map(|scope| SkillSource { harness, scope, enabled })
        })
        .collect();
    let codex = sources.iter().position(|s| s.harness == SkillHarness::Codex).unwrap_or(sources.len());
    sources.insert(codex, SkillSource { harness: SkillHarness::Claude, scope: SkillScope::Synced, enabled: false });
    sources
}

/// Skills of a session in `directory`; of two with one name the earlier source wins, unless it is turned off.
pub fn find(sources: &[SkillSource], directory: &Path, disabled: &[String]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for source in sources.iter().filter(|s| s.enabled) {
        for (folder, kind) in folders(source, directory) {
            // Claude desktop's skills come only from their own source
            let skip = (source.harness == SkillHarness::Claude && source.scope == SkillScope::Global).then(|| folder.join(SYNCED));
            let found = match kind {
                Kind::Skills => skill_files(&folder, skip.as_deref()),
                Kind::Commands => command_files(&folder),
            };
            for (name, path) in found {
                if skills.iter().any(|s| s.enabled && s.name == name) {
                    continue;
                }
                if let Some(mut skill) = read(name, path, source) {
                    skill.enabled = !disabled.contains(&skill.path.display().to_string());
                    skills.push(skill);
                }
            }
        }
    }
    skills
}

#[derive(Clone, Copy)]
enum Kind {
    /// A folder per skill, each with a SKILL.md.
    Skills,
    /// Claude Code's commands: a .md file per command.
    Commands,
}

/// Where a source keeps its skills; project folders from `directory` up to the project root.
fn folders(source: &SkillSource, directory: &Path) -> Vec<(PathBuf, Kind)> {
    let places: &[(&str, Kind)] = match (source.harness, source.scope) {
        (SkillHarness::Nabucode, SkillScope::Project | SkillScope::Global) => &[(".nabucode/skills", Kind::Skills)],
        (SkillHarness::Claude, SkillScope::Project | SkillScope::Global) => &[(".claude/skills", Kind::Skills), (".claude/commands", Kind::Commands)],
        (SkillHarness::Claude, SkillScope::Synced) => &[(".claude/skills/synced", Kind::Skills)],
        (SkillHarness::Codex, SkillScope::Global) => &[(".codex/skills", Kind::Skills), (".agents/skills", Kind::Skills)],
        (SkillHarness::Codex, SkillScope::Project) => &[(".agents/skills", Kind::Skills)],
        (_, SkillScope::Synced) => &[],
    };
    let bases: Vec<PathBuf> = match source.scope {
        SkillScope::Global | SkillScope::Synced => dirs::home_dir().into_iter().collect(),
        SkillScope::Project => {
            let root = prompt::git::root(directory).unwrap_or_else(|| directory.to_owned());
            prompt::project_dirs(&root, directory).into_iter().rev().collect()
        }
    };
    bases
        .iter()
        .flat_map(|base| places.iter().map(move |(place, kind)| (place.split('/').fold(base.clone(), |path, part| path.join(part)), *kind)))
        .filter(|(folder, _)| folder.is_dir())
        .collect()
}

/// SKILL.md files under `folder`, a few levels deep, named by their folder; a skill's own subfolders are not searched.
fn skill_files(folder: &Path, skip: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let mut pending = vec![(folder.to_owned(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let file = dir.join("SKILL.md");
        if depth > 0 && file.is_file() {
            found.push((dir.file_name().unwrap_or_default().to_string_lossy().into_owned(), file));
            continue;
        }
        if depth < 3 {
            pending.extend(subfolders(&dir).into_iter().rev().filter(|d| Some(d.as_path()) != skip).map(|d| (d, depth + 1)));
        }
    }
    found.sort();
    found
}

/// Command files under `folder`; a file in a subfolder is named `sub:name`, as Claude Code does.
fn command_files(folder: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let mut pending = vec![(folder.to_owned(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            if path.is_dir() {
                pending.push((path, format!("{prefix}{stem}:")));
            } else if path.extension().is_some_and(|e| e == "md") {
                found.push((format!("{prefix}{stem}"), path));
            }
        }
    }
    found
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    // Hidden folders hold a tool's own skills, like Codex's `.system`
    let hidden = |p: &PathBuf| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'));
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !hidden(p)).collect();
    dirs.sort();
    dirs
}

/// The header's name wins over the folder's; without a description the first line of the text is one.
fn read(name: String, path: PathBuf, source: &SkillSource) -> Option<Skill> {
    let text = std::fs::read_to_string(&path).ok()?;
    let (fields, body) = header(&text);
    let field = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim().to_owned()).filter(|v| !v.is_empty());
    let first_line = || body.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).map(str::to_owned);
    let described = field("description");
    let preview = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = match preview.char_indices().nth(PREVIEW) {
        Some((cut, _)) => format!("{}…", preview[..cut].trim_end()),
        None => preview,
    };
    Some(Skill {
        name: field("name").unwrap_or(name),
        described: described.is_some(),
        description: described.or_else(first_line).unwrap_or_default(),
        preview,
        enabled: true,
        path,
        harness: source.harness,
        scope: source.scope,
    })
}

/// The `---` header's fields and the text after it. Reads the YAML skills use: `key: value`,
/// quoted values, `|` and `>` blocks, and plain values going on in indented lines.
fn header(text: &str) -> (Vec<(String, String)>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---").filter(|r| r.starts_with('\n') || r.starts_with("\r\n")) else {
        return (Vec::new(), text);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), text);
    };
    let body = rest[end + 4..].split_once('\n').map_or("", |(_, b)| b);
    let mut fields: Vec<(String, String, char)> = Vec::new();
    for line in rest[..end].lines() {
        let indented = line.starts_with([' ', '\t']);
        match (indented, fields.last_mut()) {
            (true, Some((_, value, style))) => {
                let separator = if *style == '|' { "\n" } else { " " };
                if !value.is_empty() {
                    value.push_str(separator);
                }
                value.push_str(line.trim());
            }
            _ => {
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim();
                    let style = value.chars().next().filter(|c| matches!(c, '|' | '>')).unwrap_or(' ');
                    let value = if style == ' ' { unquote(value) } else { String::new() };
                    fields.push((key.trim().to_owned(), value, style));
                }
            }
        }
    }
    (fields.into_iter().map(|(k, v, _)| (k, v)).collect(), body)
}

fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
            return if quote == '"' { inner.replace("\\\"", "\"").replace("\\n", "\n") } else { inner.replace("''", "'") };
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nabucode-skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn header_reads_quoted_and_block_values() {
        let text = "---\nname: \"review\"\ndescription: >\n  Reviews the diff\n  for bugs.\nnotes: |\n  a\n  b\n---\n# Body\ntext\n";
        let (fields, body) = header(text);
        assert_eq!(fields[0], ("name".into(), "review".into()));
        assert_eq!(fields[1], ("description".into(), "Reviews the diff for bugs.".into()));
        assert_eq!(fields[2], ("notes".into(), "a\nb".into()));
        assert_eq!(body, "# Body\ntext\n");
        assert_eq!(header("no header").1, "no header");
    }

    #[test]
    fn project_skills_win_and_commands_are_named_by_folder() {
        let root = temp("find");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let skill = |base: &Path, name: &str, description: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\ndescription: {description}\n---\nbody of {name}")).unwrap();
        };
        skill(&root.join(".nabucode/skills"), "review", "nabucode review");
        skill(&root.join(".claude/skills"), "review", "claude review");
        skill(&root.join(".claude/skills/group"), "deploy", "deploy it");
        std::fs::create_dir_all(root.join(".claude/commands/git")).unwrap();
        std::fs::write(root.join(".claude/commands/git/commit.md"), "# Commits the work\n\nSteps.").unwrap();

        let project = |harness| SkillSource { harness, scope: SkillScope::Project, enabled: true };
        let sources = [project(SkillHarness::Nabucode), project(SkillHarness::Claude)];
        let skills = find(&sources, &root, &[]);
        let names: Vec<(&str, &str)> = skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect();
        assert_eq!(names, [("review", "nabucode review"), ("deploy", "deploy it"), ("git:commit", "Commits the work")]);
        assert_eq!(skills[0].body().unwrap(), "body of review");
        assert!(skills[0].described && !skills[2].described);
        assert_eq!(skills[2].preview, "# Commits the work Steps.");

        let claude_only = [SkillSource { enabled: false, ..sources[0].clone() }, sources[1].clone()];
        let review = find(&claude_only, &root, &[]).into_iter().find(|s| s.name == "review").unwrap();
        assert_eq!(review.description, "claude review");

        // A turned off skill stays in the list and lets the next one of its name take its place
        let off = [skills[0].path.display().to_string()];
        let reviews: Vec<(String, bool)> = find(&sources, &root, &off).into_iter().filter(|s| s.name == "review").map(|s| (s.description, s.enabled)).collect();
        assert_eq!(reviews, [("nabucode review".into(), false), ("claude review".into(), true)]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn claude_desktop_skills_are_left_out_of_the_global_folder() {
        let root = temp("synced");
        for dir in ["review", "synced/1a2b/chrome-browser"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("SKILL.md"), "text").unwrap();
        }
        let names = |skip: Option<&Path>| skill_files(&root, skip).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        assert_eq!(names(None), ["chrome-browser", "review"]);
        assert_eq!(names(Some(&root.join(SYNCED))), ["review"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
