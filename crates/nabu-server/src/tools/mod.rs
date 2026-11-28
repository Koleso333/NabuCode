use std::path::{Path, PathBuf};
use std::sync::Arc;

use ncore::core::ir::{Block, Tool, ToolCall, ToolDef, ToolResult};
use ncore::rt::{BoxFuture, boxed};
use ncore::{CancelToken, Tools};
use serde_json::{Value, json};

pub use agent::Ask;
pub use browse::close_chat as close_browser_tabs;
pub mod browser_panel {
    pub use super::browse::{chrome_connected, chrome_link, clear_cache, clear_site_data, close_tab as close, go, history, open_tab as open, reload, send_events_to, stored, stream, tabs_of};
}
pub use jobs::Jobs;

use crate::sessions::{Interrupt, Sessions, Spawner};
use crate::skills::Skill;

mod agent;
mod ask;
pub mod browser;
mod browse;
mod changes;
mod edit;
mod fetch;
mod glob;
mod grep;
mod jobs;
mod list;
mod outline;
mod read;
mod search;
mod shell;
mod skill;
mod wait;
mod waits;
mod web;
mod write;

/// Lines or paths a listing shows before it says how many more there are.
const SHOWN: usize = 200;

/// How a running call is told to stop.
#[derive(Clone, Default)]
pub struct Signals {
    /// The person stopped the agent: the call's work is killed.
    pub cancel: CancelToken,
    /// The person sent a message: commands and subagents go on in the background, waits end.
    pub wrap_up: CancelToken,
}

#[derive(Clone, Copy)]
enum Run {
    Files(fn(&Path, &Value) -> Result<String, String>),
    Content(fn(&Path, &Value) -> Result<Vec<Block>, String>),
    /// Runs programs: needs the call's signals to kill them or send them to the background, and the session's background tasks.
    Programs(fn(&Path, &Value, &Signals, &Jobs) -> Result<String, String>),
    /// Talks to the network: runs on the async threads and touches no files.
    Web(fn(Value) -> BoxFuture<'static, Result<String, String>>),
    /// Works in the chat's browser tabs.
    Browser(fn(browse::Call) -> BoxFuture<'static, Result<Vec<Block>, String>>),
    /// `wait`: watches the session's background work and the chat's tabs.
    Wait,
}

/// Definition, name, run, and whether the tool must run alone: file edits do, reads and shells share.
const TOOLS: [(fn() -> ToolDef, &str, Run, bool); 19] = [
    (read::def, "read", Run::Content(read::run), false),
    (list::def, "list", Run::Files(list::run), false),
    (glob::def, "glob", Run::Files(glob::run), false),
    (grep::def, "grep", Run::Files(grep::run), false),
    (edit::def, "edit", Run::Files(edit::run), true),
    (write::def, "write", Run::Files(write::run), true),
    (
        shell::bash_def,
        "bash",
        Run::Programs(shell::run_bash),
        false,
    ),
    (
        shell::powershell_def,
        "powershell",
        Run::Programs(shell::run_powershell),
        false,
    ),
    (jobs::def, "task", Run::Programs(jobs::run), false),
    (wait::plain_def, "wait", Run::Wait, false),
    (
        search::def,
        "search",
        Run::Web(|args| boxed(search::run(args))),
        false,
    ),
    (
        fetch::def,
        "fetch",
        Run::Web(|args| boxed(fetch::run(args))),
        false,
    ),
    (browse::open_def, "browser_open", Run::Browser(|call| boxed(browse::open(call))), false),
    (browse::tabs_def, "browser_tabs", Run::Browser(|call| boxed(browse::tabs(call))), false),
    (browse::state_def, "browser_state", Run::Browser(|call| boxed(browse::page_state(call))), false),
    (browse::act_def, "browser_act", Run::Browser(|call| boxed(browse::act(call))), false),
    (browse::read_def, "browser_read", Run::Browser(|call| boxed(browse::read(call))), false),
    (browse::js_def, "browser_js", Run::Browser(|call| boxed(browse::js(call))), false),
    (browse::logs_def, "browser_logs", Run::Browser(|call| boxed(browse::logs(call))), false),
];

/// Groups of tools the model loads with `tools` when it needs them: name, what they are for, their tools.
const DEFERRED: [(&str, &str, &[&str]); 3] = [
    ("search", search::BRIEF, &["search"]),
    ("fetch", fetch::BRIEF, &["fetch"]),
    (
        "browser",
        browse::BRIEF,
        &["browser_open", "browser_tabs", "browser_state", "browser_act", "browser_read", "browser_js", "browser_logs"],
    ),
];

/// A group with its tools in brackets, when they have other names: `browser (browser_open, browser_tabs)`.
fn with_tools(group: &str) -> String {
    match DEFERRED.iter().find(|(name, _, tools)| *name == group && tools != &[group]) {
        Some((_, _, tools)) => format!("{group} ({})", tools.join(", ")),
        None => group.to_owned(),
    }
}

/// Whether the tool is loaded on demand.
fn deferred(tool: &str) -> bool {
    DEFERRED.iter().any(|(_, _, tools)| tools.contains(&tool))
}

fn is_group(name: &str) -> bool {
    DEFERRED.iter().any(|(group, _, _)| *group == name)
}

/// The group a deferred tool loads with; other tools stand for themselves.
fn group_of(tool: &str) -> &str {
    DEFERRED.iter().find(|(_, _, tools)| tools.contains(&tool)).map_or(tool, |(group, _, _)| group)
}

/// Whether the machine has what the tool needs.
fn offered(name: &str) -> bool {
    match name {
        "bash" => shell::BASH.is_some(),
        "powershell" => shell::POWERSHELL.is_some(),
        _ => true,
    }
}

/// Names of the tools this machine offers, without `agent`; a group of deferred tools by its name.
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for (_, name, _, _) in TOOLS.iter().filter(|(_, name, _, _)| offered(name)) {
        let name = group_of(name);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The tools of one session, working relative to its directory.
pub struct Toolbox {
    directory: PathBuf,
    /// Calls of one step start together: reads share it, changes take it alone.
    /// The lock queues in call order, so each call sees the changes of the calls before it.
    order: tokio::sync::RwLock<()>,
    jobs: Arc<Jobs>,
    /// The only tools a subagent may use; `None` means all.
    allowed: Option<Vec<String>>,
    /// Starts subagents; a subagent has none, so they do not nest.
    spawner: Option<Spawner>,
    /// Deferred tools loaded in this session, in load order.
    loaded: std::sync::Mutex<Vec<String>>,
    /// The session whose tool calls get the changed files as metadata.
    shown_in: Option<(Arc<Sessions>, String)>,
    /// The turn's interruption, which tells running calls to wrap up.
    interrupt: Arc<Interrupt>,
    /// Skills the model may load with `skill`.
    skills: Vec<Skill>,
    /// The chat whose browser tabs the tools see: a subagent works in its parent's.
    chat: String,
    /// The extension's id of the Chrome the chat works in.
    chrome: Option<String>,
}

impl Toolbox {
    pub fn new(directory: &Path, jobs: Arc<Jobs>) -> Self {
        Self {
            directory: directory.to_owned(),
            order: tokio::sync::RwLock::new(()),
            jobs,
            allowed: None,
            spawner: None,
            loaded: Default::default(),
            shown_in: None,
            interrupt: Default::default(),
            skills: Vec::new(),
            chat: String::new(),
            chrome: None,
        }
    }

    pub fn in_chat(mut self, chat: String, chrome: Option<String>) -> Self {
        (self.chat, self.chrome) = (chat, chrome);
        self
    }

    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }

    fn has_skills(&self) -> bool {
        !self.skills.is_empty() && self.allows("skill")
    }

    pub fn interrupted_by(mut self, interrupt: Arc<Interrupt>) -> Self {
        self.interrupt = interrupt;
        self
    }

    pub fn showing_in(mut self, sessions: Arc<Sessions>, session: &str) -> Self {
        self.shown_in = Some((sessions, session.to_owned()));
        self
    }

    pub fn limited(mut self, allowed: Option<Vec<String>>) -> Self {
        self.allowed = allowed;
        self
    }

    pub fn spawning(mut self, spawner: Spawner) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Deferred tools already loaded: by earlier `tools` calls, or given to a subagent by name.
    pub fn loading(self, names: Vec<String>) -> Self {
        let given = self
            .allowed
            .iter()
            .flatten()
            .filter(|n| is_group(n))
            .cloned();
        let mut loaded: Vec<String> = Vec::new();
        for name in names.into_iter().chain(given) {
            if is_group(&name) && self.allows(&name) && !loaded.contains(&name) {
                loaded.push(name);
            }
        }
        *self.loaded.lock().unwrap() = loaded;
        self
    }

    fn allows(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|list| list.iter().any(|n| n == name || n == group_of(name)))
    }

    fn has_browser(&self) -> bool {
        self.allows("browser") && self.is_loaded("browser")
    }

    fn is_loaded(&self, tool: &str) -> bool {
        self.loaded.lock().unwrap().iter().any(|n| n == group_of(tool))
    }

    /// Deferred tools this session may load.
    fn loadable(&self) -> Vec<(&'static str, &'static str)> {
        DEFERRED
            .into_iter()
            .filter(|(name, _, _)| offered(name) && self.allows(name))
            .map(|(name, brief, _)| (name, brief))
            .collect()
    }

    fn load(&self, args: &Value) -> Result<String, String> {
        let names = str_list(args, "load");
        if names.is_empty() {
            return Err("missing `load`: the names of the tools to load".into());
        }
        let loadable = self.loadable();
        let unknown: Vec<&String> = names
            .iter()
            .filter(|n| !loadable.iter().any(|(l, _)| l == n))
            .collect();
        if !unknown.is_empty() {
            let names: Vec<&str> = loadable.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "cannot load {unknown:?}; deferred tools here: {}",
                names.join(", ")
            ));
        }
        let mut loaded = self.loaded.lock().unwrap();
        let new: Vec<String> = names.into_iter().filter(|n| !loaded.contains(n)).collect();
        loaded.extend(new.iter().cloned());
        Ok(match new.is_empty() {
            true => format!("already loaded: {}", loaded.join(", ")),
            false => format!(
                "loaded {}; you can call them from your next step",
                new.iter().map(|n| with_tools(n)).collect::<Vec<_>>().join(", ")
            ),
        })
    }

    /// Deferred tools this session could load but has not.
    pub fn unloaded(&self) -> Vec<ToolDef> {
        let loadable = self.loadable();
        TOOLS
            .iter()
            .filter(|(_, name, _, _)| {
                deferred(name) && loadable.iter().any(|(l, _)| *l == group_of(name)) && !self.is_loaded(name)
            })
            .map(|(def, _, _, _)| def())
            .collect()
    }

    /// Runs off the async threads: tools read files and walk trees.
    async fn run(
        &self,
        call: &ToolCall,
        args: Value,
        signals: &Signals,
    ) -> Result<Vec<Block>, String> {
        let name = call.name.as_str();
        let text = |text| vec![Block::text(text)];
        if !self.allows(name) {
            return Err(format!(
                "`{name}` is not among the tools you were given: {}",
                self.allowed.as_deref().unwrap_or_default().join(", ")
            ));
        }
        if name == "tools" && !self.loadable().is_empty() {
            return self.load(&args).map(text);
        }
        if name == "skill" && self.has_skills() {
            return skill::run(&self.skills, &args).map(text);
        }
        if deferred(name) && !self.is_loaded(name) {
            return Err(format!(
                "`{name}` is not loaded yet: call `tools` with load: [\"{}\"] first",
                group_of(name)
            ));
        }
        if let (Some(spawner), "ask") = (&self.spawner, name) {
            drop(self.order.read().await);
            return ask::run(&spawner.sessions, &spawner.parent, &call.id, &args, &signals.cancel)
                .await
                .map(text);
        }
        if let (Some(spawner), "agent") = (&self.spawner, name) {
            // Waits for the changes of earlier calls in the step, then lets later ones go on.
            drop(self.order.read().await);
            return spawner
                .run(&call.id, Ask::parse(&args, &names())?, &self.jobs, signals)
                .await
                .map(text);
        }
        let Some(&(_, _, run, alone)) = TOOLS.iter().find(|(_, n, _, _)| *n == name && offered(n))
        else {
            return Err(format!("unknown tool `{name}`"));
        };
        if let Run::Web(run) = run {
            return web::until_stopped(signals, run(args)).await.map(text);
        }
        if let Run::Wait = run {
            drop(self.order.read().await);
            let call = wait::Call { args, jobs: self.jobs.clone(), signals: signals.clone(), chat: self.chat.clone(), browser: self.has_browser() };
            return wait::run(call).await.map(text);
        }
        if let Run::Browser(run) = run {
            let call = browse::Call { chat: self.chat.clone(), chrome: self.chrome.clone(), directory: self.directory.clone(), args };
            return web::until_stopped(signals, run(call)).await;
        }
        let _alone = if alone {
            Some(self.order.write().await)
        } else {
            None
        };
        let _shared = if alone {
            None
        } else {
            Some(self.order.read().await)
        };
        let directory = self.directory.clone();
        let signals = signals.clone();
        let jobs = self.jobs.clone();
        let tool = name.to_owned();
        let task = tokio::task::spawn_blocking(move || match run {
            Run::Files(run) => {
                let before = changes::Before::take(&directory, &tool, &args);
                let result = run(&directory, &args).map(text);
                (
                    result,
                    before.map(|b| b.files(&directory)).unwrap_or_default(),
                )
            }
            Run::Content(run) => (run(&directory, &args), Vec::new()),
            Run::Programs(run) => (run(&directory, &args, &signals, &jobs).map(text), Vec::new()),
            Run::Web(_) | Run::Browser(_) | Run::Wait => unreachable!("web, browser and wait tools run above"),
        });
        let (result, files) = task.await.map_err(|e| format!("the tool crashed: {e}"))?;
        if let (Some((sessions, session)), false) = (&self.shown_in, files.is_empty()) {
            sessions.link(session, &call.id, json!({ "files": files }));
        }
        result
    }
}

impl Tools for Toolbox {
    fn definitions(&self) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = TOOLS
            .iter()
            .filter(|(_, name, _, _)| offered(name) && self.allows(name) && !deferred(name))
            .map(|(def, name, _, _)| if *name == "wait" { wait::def(self.has_browser()) } else { def() })
            .collect();
        // Only the main agent talks to the user and starts subagents.
        if let Some(spawner) = &self.spawner {
            defs.push(ask::def());
            defs.push(agent::def(
                &spawner.providers(),
                &spawner.current(),
                &names(),
            ));
        }
        if self.has_skills() {
            defs.push(skill::def(&self.skills));
        }
        let loadable = self.loadable();
        if loadable.iter().any(|(group, _)| !self.is_loaded(group)) {
            defs.push(tools_def(&loadable));
        }
        // Loaded ones last, in load order: the tools before them keep their place in the cached prefix.
        for group in self.loaded.lock().unwrap().iter() {
            defs.extend(TOOLS.iter().filter(|(_, n, _, _)| group_of(n) == group).map(|(def, _, _, _)| def()));
        }
        defs
    }

    fn call<'a>(
        &'a self,
        call: &'a ToolCall,
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, ToolResult> {
        boxed(async move {
            let running = self.interrupt.call();
            let signals = Signals { cancel: cancel.clone(), wrap_up: running.wrap_up.clone() };
            let outcome = match serde_json::from_str::<Value>(&call.args) {
                Ok(args) => self.run(call, args, &signals).await,
                Err(e) => Err(format!("arguments are not valid JSON: {e}")),
            };
            let (mut content, is_error) = match outcome {
                Ok(content) => (content, false),
                Err(text) => (vec![Block::text(text)], true),
            };
            let notes = self.jobs.notices();
            if !notes.is_empty() {
                let notes = notes.join("\n\n");
                if let Some(Block::Text { text, .. }) = content.first_mut() {
                    text.push_str("\n\n");
                    text.push_str(&notes);
                } else {
                    content.push(Block::text(notes));
                }
            }
            ToolResult {
                call_id: call.id.clone(),
                name: Some(call.name.clone()),
                content,
                is_error,
                cache: None,
            }
        })
    }
}

fn tools_def(loadable: &[(&str, &str)]) -> ToolDef {
    let list: Vec<String> = loadable
        .iter()
        .map(|(name, brief)| format!("- {}: {brief}", with_tools(name)))
        .collect();
    def(
        "tools",
        &format!(
            "Loads deferred tools: they are listed here but cannot be called until loaded. \
Load the ones the task needs, several in one call if so; they stay loaded for the rest of the session \
and can be called from your next step.\nDeferred tools:\n{}",
            list.join("\n")
        ),
        json!({
            "type": "object",
            "properties": {
                "load": { "type": "array", "items": { "type": "string" }, "description": "Names of the tools to load." }
            },
            "required": ["load"]
        }),
    )
}

fn def(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef::Function(Tool {
        name: name.into(),
        description: Some(description.into()),
        schema,
        strict: None,
        cache: None,
        ext: Default::default(),
    })
}

/// `path` as given when absolute, else inside the working directory.
fn resolve(directory: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        directory.join(path)
    }
}

/// The search root from an optional `path` argument.
fn root(directory: &Path, args: &Value) -> Result<PathBuf, String> {
    let root =
        opt_str(args, "path").map_or_else(|| directory.to_owned(), |p| resolve(directory, p));
    if root.exists() {
        Ok(root)
    } else {
        Err(format!("{} does not exist", root.display()))
    }
}

/// A walk under `root` that skips what .gitignore and .ignore exclude, and the .git folder itself.
fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git");
    builder
}

/// Files under `root`.
fn walk(root: &Path) -> impl Iterator<Item = PathBuf> {
    walker(root)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(ignore::DirEntry::into_path)
}

/// A path relative to the working directory when inside it, with forward slashes.
fn shown(directory: &Path, path: &Path) -> String {
    let text = path
        .strip_prefix(directory)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if text.is_empty() { ".".into() } else { text }
}

/// `N lines`, or `binary`.
fn size(path: &Path) -> String {
    text_of(path).map_or("binary".into(), |t| format!("{} lines", t.lines().count()))
}

/// File text, or `None` for binary files.
fn text_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The first `SHOWN` lines, and a note with how many were left out and how to see them.
fn listing(lines: Vec<String>, what: &str, hint: &str) -> String {
    let total = lines.len();
    let mut out: Vec<String> = lines.into_iter().take(SHOWN).collect();
    if total > SHOWN {
        out.push(format!("({} more {what} not shown; {hint})", total - SHOWN));
    }
    out.join("\n")
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    opt_str(args, key).ok_or_else(|| format!("missing string argument `{key}`"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// A string or a list of strings.
fn str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        // Models sometimes send the list as JSON text.
        Some(Value::String(s))
            if s.starts_with('[') && serde_json::from_str::<Vec<String>>(s).is_ok() =>
        {
            serde_json::from_str(s).unwrap_or_default()
        }
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn num_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("nabucode-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    client.session\n        .list();\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/deep/lib.rs"), "pub fn list() {}\n").unwrap();
        std::fs::write(root.join("target/out.rs"), "fn list() {}\n").unwrap();
        std::fs::write(root.join("blob.bin"), b"list\0\0").unwrap();
        root
    }

    #[test]
    fn a_session_limited_to_a_group_has_its_tools_loaded() {
        let toolbox = Toolbox::new(&std::env::temp_dir(), Arc::default()).limited(Some(vec!["browser".into()])).loading(Vec::new());
        let names: Vec<String> = toolbox.definitions().into_iter().filter_map(|d| match d {
            ToolDef::Function(tool) => Some(tool.name),
            _ => None,
        }).collect();
        let browser = ["browser_open", "browser_tabs", "browser_state", "browser_act", "browser_read", "browser_js", "browser_logs"];
        assert_eq!(names, browser);
    }

    #[test]
    fn glob_by_name_and_by_path_skips_ignored() {
        let root = project("glob");
        let names = glob::run(&root, &json!({"pattern": "*.rs"})).unwrap();
        assert!(names.contains("src/main.rs") && names.contains("src/deep/lib.rs"));
        assert!(!names.contains("target"));
        let paths = glob::run(&root, &json!({"pattern": "src/*.rs"})).unwrap();
        assert_eq!(paths, "src/main.rs (4 lines)");
        let excluded = glob::run(&root, &json!({"pattern": ["*.rs", "!**/deep/**"]})).unwrap();
        assert_eq!(excluded, "src/main.rs (4 lines)");
        let none = glob::run(&root, &json!({"pattern": "*.py"})).unwrap();
        assert!(none.starts_with("no files match *.py in `.` ("), "{none}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn grep_says_what_it_searched() {
        let root = project("scope");
        let empty_glob = grep::run(
            &root,
            &json!({"pattern": "list", "glob": "**/agent*/**/*.rs"}),
        )
        .unwrap();
        assert!(
            empty_glob.contains("matched no files") && empty_glob.contains("nothing was searched")
        );
        let no_hits = grep::run(&root, &json!({"pattern": "nothing_here"})).unwrap();
        assert!(no_hits.contains("files searched"), "{no_hits}");
        let excluded = grep::run(
            &root,
            &json!({"pattern": "list", "glob": ["*.rs", "!src/deep/**"], "output": "files"}),
        )
        .unwrap();
        assert_eq!(excluded, "src/main.rs (4 lines)");
        let capped = grep::run(
            &root,
            &json!({"pattern": ".", "path": "src/main.rs", "max_per_file": 1}),
        )
        .unwrap();
        assert_eq!(
            capped,
            "src/main.rs\n1: fn main() {\n(3 more matching lines in this file)"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_outline_lists_declarations() {
        let root = project("outline");
        let source = "use x;\n\npub struct A;\n\nimpl A {\n    pub fn new() -> Self {\n        let a = 1;\n    }\n}\n\nconst B: u8 = 1;\nexport default async function c() {}\nclass D:\n    def e(self):\n";
        std::fs::write(root.join("src/mixed.txt"), source).unwrap();
        let text = read::run(&root, &json!({"path": "src/mixed.txt", "outline": true})).unwrap();
        assert_eq!(
            ncore::core::ir::plain_text(&text),
            "3\tpub struct A;\n5\timpl A {\n6\t    pub fn new() -> Self {\n11\tconst B: u8 = 1;\n\
12\texport default async function c() {}\n13\tclass D:\n14\t    def e(self):\n(outline: 7 declarations in 14 lines)"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn list_shows_a_tree() {
        let root = project("list");
        let tree = list::run(&root, &json!({})).unwrap();
        assert_eq!(
            tree,
            ".gitignore (1 lines)\nblob.bin (binary)\nsrc/\n  deep/\n  main.rs (4 lines)"
        );
        assert!(
            list::run(&root, &json!({"path": "src/main.rs"}))
                .unwrap_err()
                .contains("use read")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn read_several_ranges() {
        let root = project("read");
        let text = read::run(
            &root,
            &json!({"path": "src/main.rs", "ranges": [[4, 9], [1, 1], [2, 2]]}),
        )
        .unwrap();
        assert_eq!(
            ncore::core::ir::plain_text(&text),
            "1\tfn main() {\n2\t    client.session\n--\n4\t}\n(file has 4 lines)"
        );
        assert!(read::run(&root, &json!({"path": "src/main.rs", "ranges": [[7, 9]]})).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn read_returns_image_content() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use ncore::core::ir::MediaSource;

        let root = project("read-image");
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+b3ioAAAAASUVORK5CYII=";
        std::fs::write(root.join("shot"), STANDARD.decode(png).unwrap()).unwrap();
        let toolbox = Toolbox::new(&root, Arc::new(Jobs::default()));
        let call = ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            args: json!({"path": "shot"}).to_string(),
            sig: None,
        };
        let result = toolbox.call(&call, &CancelToken::new()).await;

        assert!(!result.is_error);
        let [Block::Text { text, .. }, Block::Media { media, .. }] = result.content.as_slice() else {
            panic!("expected an image and its filename");
        };
        assert_eq!(text, "Image: shot");
        assert_eq!(media.mime.as_deref(), Some("image/png"));
        assert!(matches!(&media.source, MediaSource::Base64(data) if data == png));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn edit_applies_all_or_nothing_and_shows_new_lines() {
        let root = project("edit");
        std::fs::write(
            root.join("a.txt"),
            "\u{feff}one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\n",
        )
        .unwrap();
        let edits = json!({"path": "a.txt", "edits": [{"old": "two\nthree", "new": "2\n3\n3.5"}, {"old": "six", "new": "6"}]});
        let text = edit::run(&root, &edits).unwrap();
        assert_eq!(
            text,
            "edited a.txt (2 edits; 7 -> 8 lines). Changed lines now:\n1\tone\n2\t2\n3\t3\n4\t3.5\n5\tfour\n6\tfive\n7\t6\n8\tseven"
        );
        let saved = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert_eq!(
            saved,
            "\u{feff}one\r\n2\r\n3\r\n3.5\r\nfour\r\nfive\r\n6\r\nseven\r\n"
        );

        let failed = edit::run(
            &root,
            &json!({"path": "a.txt", "edits": [{"old": "one", "new": "1"}, {"old": "nine", "new": "9"}]}),
        );
        assert!(
            failed
                .unwrap_err()
                .starts_with("edit 2 of 2 (a.txt): `old` was not found, nor its first line")
        );
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), saved);

        std::fs::write(
            root.join("b.rs"),
            "fn a() {\n    x(1);\n}\nfn b() {\n    x(1);\n}\n",
        )
        .unwrap();
        let twice = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "x(1);", "new": "y();"}]}),
        )
        .unwrap_err();
        assert!(
            twice.starts_with("`old` matches 2 places, at lines 2, 5;"),
            "{twice}"
        );
        let outside = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "x(1);", "new": "y();", "lines": [3, 4]}]}),
        )
        .unwrap_err();
        assert!(
            outside.starts_with("`old` is not in lines 3-4; it is at lines 2, 5"),
            "{outside}"
        );
        std::fs::write(root.join("c.rs"), "x x\nx\n").unwrap();
        let counted = edit::run(&root, &json!({"path": "c.rs", "edits": [{"old": "x", "new": "y", "all": true, "lines": [1, 1]}]})).unwrap();
        assert!(
            counted.starts_with("edited c.rs (1 edit, 2 replacements; 2 -> 2 lines)"),
            "{counted}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("c.rs")).unwrap(),
            "y y\nx\n"
        );
        let spaces = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "fn b() {\nx(1);", "new": ""}]}),
        )
        .unwrap_err();
        assert!(spaces.contains("matches lines 4-5 when spaces"), "{spaces}");
        let numbers = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "2\t    x(1);", "new": ""}]}),
        )
        .unwrap_err();
        assert!(numbers.contains("numbers from read output"), "{numbers}");
        let every = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "    x(1);\n", "new": "", "all": true}]}),
        )
        .unwrap();
        assert_eq!(
            every,
            "edited b.rs (1 edit, 2 replacements; 6 -> 4 lines). Changed lines now:\n1\tfn a() {\n2\t}\n3\tfn b() {\n4\t}"
        );
        let renamed = edit::run(&root, &json!({"path": "b.rs", "edits": [{"old": "a", "new": "c", "all": true, "word": true}]})).unwrap();
        assert!(renamed.contains("1\tfn c() {"), "{renamed}");
        let inside = edit::run(
            &root,
            &json!({"path": "b.rs", "edits": [{"old": "f", "new": "g", "word": true}]}),
        )
        .unwrap_err();
        assert!(inside.contains("only inside longer words"), "{inside}");

        let across = json!({"edits": [{"path": "a.txt", "old": "seven", "new": "7"}, {"path": "b.rs", "old": "fn b", "new": "fn d"}]});
        let both = edit::run(&root, &across).unwrap();
        assert!(
            both.starts_with("edited a.txt (1 edit; 8 -> 8 lines)")
                && both.contains("\n\nedited b.rs (1 edit;"),
            "{both}"
        );
        let half = json!({"edits": [{"path": "a.txt", "old": "7", "new": "8"}, {"path": "b.rs", "old": "fn x", "new": "fn y"}]});
        assert!(
            edit::run(&root, &half)
                .unwrap_err()
                .contains("no file was changed")
        );
        assert!(
            std::fs::read_to_string(root.join("a.txt"))
                .unwrap()
                .contains("7\r\n")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn edit_reports_changes_ending_in_multibyte_characters() {
        let root = project("edit-unicode");
        for new in ["x", "н", "й", "漢", "😀", "первая\nпоследняя", "н\n", ""]
        {
            std::fs::write(root.join("a.txt"), "before\nold\nafter\n").unwrap();
            std::fs::write(root.join("b.txt"), "old").unwrap();
            let edits = json!({"edits": [
                {"path": "a.txt", "old": "old", "new": new},
                {"path": "b.txt", "old": "old", "new": "готово😀"}
            ]});
            let output = edit::run(&root, &edits).unwrap();
            let expected = format!("before\n{new}\nafter\n");
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).unwrap(),
                expected
            );
            assert_eq!(
                std::fs::read_to_string(root.join("b.txt")).unwrap(),
                "готово😀"
            );
            let lines: Vec<_> = expected.lines().collect();
            let numbered = lines
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{}\t{line}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                output,
                format!(
                    "edited a.txt (1 edit; 3 -> {} lines). Changed lines now:\n{numbered}\n\nedited b.txt (1 edit; 1 -> 1 lines). Changed lines now:\n1\tготово😀",
                    lines.len()
                )
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn calls_of_one_step_change_files_in_call_order() {
        let root = project("order");
        std::fs::write(root.join("n.txt"), "a\n").unwrap();
        let toolbox = Toolbox::new(&root, Arc::default());
        let call = |i: usize, name: &str, args: Value| ToolCall {
            id: i.to_string(),
            name: name.into(),
            args: args.to_string(),
            sig: None,
        };
        let mut calls = Vec::new();
        for (i, (from, to)) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")]
            .into_iter()
            .enumerate()
        {
            calls.push(call(
                i,
                "edit",
                json!({"path": "n.txt", "edits": [{"old": from, "new": to}]}),
            ));
        }
        calls.push(call(9, "read", json!({"path": "n.txt"})));
        let cancel = CancelToken::new();
        let results =
            futures_util::future::join_all(calls.iter().map(|c| toolbox.call(c, &cancel))).await;
        assert!(results.iter().all(|r| !r.is_error), "{results:?}");
        assert_eq!(std::fs::read_to_string(root.join("n.txt")).unwrap(), "e\n");
        assert!(format!("{:?}", results[4].content).contains("1\\te"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_creates_folders_and_keeps_line_endings() {
        let root = project("write");
        assert_eq!(
            write::run(
                &root,
                &json!({"path": "new/dir/x.txt", "content": "a\nb\n"})
            )
            .unwrap(),
            "created new/dir/x.txt (2 lines)"
        );
        std::fs::write(root.join("crlf.txt"), "a\r\nb\r\n").unwrap();
        assert_eq!(
            write::run(&root, &json!({"path": "crlf.txt", "content": "c\n"})).unwrap(),
            "replaced crlf.txt: 2 lines before, 1 now"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("crlf.txt")).unwrap(),
            "c\r\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn grep_lines_context_multiline_and_modes() {
        let root = project("grep");
        let lines = grep::run(&root, &json!({"pattern": "fn list"})).unwrap();
        assert_eq!(lines, "src/deep/lib.rs\n1: pub fn list() {}");
        let around = grep::run(&root, &json!({"pattern": "main", "context": 1})).unwrap();
        assert_eq!(around, "src/main.rs\n1: fn main() {\n2-     client.session");
        let chain = grep::run(
            &root,
            &json!({"pattern": r"session\s*\.list", "output": "files"}),
        )
        .unwrap();
        assert_eq!(chain, "src/main.rs (4 lines)");
        let count = grep::run(
            &root,
            &json!({"pattern": "list", "glob": "*.rs", "output": "count"}),
        )
        .unwrap();
        assert_eq!(
            count,
            "src/deep/lib.rs: 1 of 1 lines\nsrc/main.rs: 1 of 4 lines"
        );
        assert!(
            grep::run(&root, &json!({"pattern": "("}))
                .unwrap_err()
                .contains("bad regular expression")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
