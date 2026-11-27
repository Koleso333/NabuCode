use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ncore::core::ir::ToolDef;
use regex::Regex;
use serde_json::{Value, json};

use super::jobs::Jobs;
use super::{Signals, bool_arg, num_arg, opt_str, resolve, str_arg};

const TIMEOUT: u64 = 120;
const TIMEOUT_MAX: u64 = 600;
/// A stream longer than this many lines shows only its head and tail.
const LINES_MAX: usize = 200;
const BYTES_MAX: usize = 30_000;
const EDGE: usize = 80;
const LINE_MAX: usize = 1000;
/// How long output may keep coming after the process ends, from children it left running.
const DRAIN: Duration = Duration::from_secs(2);

/// Environment for every command: UTF-8 everywhere, no pagers, prompts or colors.
const ENV: [(&str, &str); 11] = [
    ("PYTHONUTF8", "1"),
    // Python buffers stdout when it is not a terminal, so a server's ready line would never reach the log.
    ("PYTHONUNBUFFERED", "1"),
    ("PYTHONIOENCODING", "utf-8"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("NABUCODE", "1"),
];

/// Lines PowerShell runs before the script: UTF-8 output and no progress records on stderr.
const PS_PROLOGUE: &str = "$ProgressPreference = 'SilentlyContinue'\n\
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
$OutputEncoding = [System.Text.Encoding]::UTF8\n";
/// After the script: the exit code of the last program it ran.
const PS_EPILOGUE: &str = "\nif ($global:LASTEXITCODE) { exit $global:LASTEXITCODE }\n";

pub static BASH: LazyLock<Option<PathBuf>> = LazyLock::new(find_bash);
pub static POWERSHELL: LazyLock<Option<PathBuf>> = LazyLock::new(find_powershell);

#[derive(Clone, Copy)]
enum Shell {
    Bash,
    PowerShell,
}

pub fn bash_def() -> ToolDef {
    let place = match BASH.as_deref() {
        Some(path) if cfg!(windows) => format!("Git Bash (`{}`)", path.display()),
        _ => "bash".to_owned(),
    };
    let windows = if cfg!(windows) {
        " Windows programs (cargo, node, python, git) run as usual; paths may be written `C:/src/x` or `/c/src/x`. \
Git's GNU tools come first on PATH, so `find`, `sort` and `grep` are the GNU ones, not Windows'. \
Prefer this tool over powershell for general commands."
    } else {
        ""
    };
    def(
        "bash",
        &format!(
            "Runs a bash script in {place} and returns its exit code, stdout and stderr.{windows} \
`pipefail` is on: a pipeline fails when any command in it fails, so `cargo test | tail` keeps cargo's exit code \
(a writer cut off by `head` may end with 141). {COMMON}"
        ),
    )
}

pub fn powershell_def() -> ToolDef {
    let path = POWERSHELL
        .as_deref()
        .map_or("powershell".into(), |p| p.display().to_string());
    let modern = path.to_lowercase().contains("pwsh");
    let version = if modern {
        "PowerShell 7"
    } else {
        "Windows PowerShell 5.1"
    };
    let quirks = if modern {
        ""
    } else {
        " In 5.1 `&&`, `||`, `?:` and `??` do not exist: use `; if ($?) { ... }`. \
Do not add `2>&1` to native programs: their stderr already comes back, and in 5.1 `2>&1` turns each stderr line into an error record."
    };
    let role = if BASH.is_some() {
        " Use it for Windows-specific work (processes, services, registry, .NET, Windows paths); for the rest prefer bash."
    } else {
        ""
    };
    def(
        "powershell",
        &format!(
            "Runs a PowerShell script in {version} (`{path}`) and returns its exit code, stdout and stderr.{role}{quirks} \
The exit code is the last program's exit code, or the one given to `exit`; errors of cmdlets show on stderr. {COMMON}"
        ),
    )
}

/// The part of both descriptions about how a call runs.
const COMMON: &str = "Each call starts fresh in the working directory or `cwd`: `cd`, variables and functions do not carry over. \
The script runs from a file as written, so quotes, backslashes and non-ASCII text need no extra escaping and several lines are fine. \
Create and change files with write and edit, not `cat > file <<EOF`, `echo >` or `sed -i`: those tools keep line endings and encoding and show what changed. \
stdin is closed, so commands that wait for input fail instead of hanging; pagers and colors are off; output is UTF-8. \
`timeout` is in seconds, 120 by default, up to 600. On timeout, or when the person stops you, \
the command and all its child processes are killed and the output so far comes back. \
stdout and stderr come back as one text in the order they were written; `split: true` gives them as separate blocks. \
Output longer than 200 lines or 30 KB shows its first and last 80 lines and the path of a file with all of it, for read. \
So do not cut output with `| tail` or `| grep`: a filter can hide the error you need. When only the end matters, \
set `tail: N` instead: it shows the last N lines and still saves all of them to a file. \
Shell calls of one step run in parallel with each other, after the file edits before them in the step. \
`background: true` starts the command as a background task (`task1`, `task2`, …) and returns at once, with no timeout: \
use it for servers, watchers and builds you do not need to wait on now, then check, wait for or stop it with the `task` tool. \
To start a server and wait for it in one call, add `wait` and `until` (its ready line). \
Do not add `&` or `nohup` for that: a process left behind by a normal call is not tracked.";

fn def(name: &str, description: &str) -> ToolDef {
    super::def(
        name,
        description,
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The script to run; may have several lines." },
                "timeout": { "type": "integer", "description": "Seconds before the command is killed. Default 120, at most 600." },
                "cwd": { "type": "string", "description": "Folder to run in. Default: the working directory." },
                "split": { "type": "boolean", "description": "Give stdout and stderr as separate blocks. Default false: one text in order." },
                "tail": { "type": "integer", "description": "Show only the last N lines (at most 200); the full output is still saved to a file." },
                "background": { "type": "boolean", "description": "Run in the background and return its task id at once; check it with the `task` tool. For servers, watchers and long builds." },
                "wait": { "type": "integer", "description": "With `background`: seconds to wait before returning, like `task`'s `wait`." },
                "until": { "type": "string", "description": "With `background` and `wait`: return as soon as an output line matches this regular expression, e.g. a server's ready line." }
            },
            "required": ["command"]
        }),
    )
}

pub fn run_bash(
    directory: &Path,
    args: &Value,
    signals: &Signals,
    jobs: &Jobs,
) -> Result<String, String> {
    run(Shell::Bash, directory, args, signals, jobs)
}

pub fn run_powershell(
    directory: &Path,
    args: &Value,
    signals: &Signals,
    jobs: &Jobs,
) -> Result<String, String> {
    run(Shell::PowerShell, directory, args, signals, jobs)
}

/// How the command ended.
enum End {
    Exit(Option<i32>),
    TimedOut(u64),
    Stopped,
    /// The person sent a message meanwhile; it goes on as this background task.
    Background(String),
}

fn run(
    shell: Shell,
    directory: &Path,
    args: &Value,
    signals: &Signals,
    jobs: &Jobs,
) -> Result<String, String> {
    let command = str_arg(args, "command")?.replace("\r\n", "\n");
    let timeout = num_arg(args, "timeout")
        .map_or(TIMEOUT, |t| t as u64)
        .clamp(1, TIMEOUT_MAX);
    let cwd = opt_str(args, "cwd").map_or_else(|| directory.to_owned(), |p| resolve(directory, p));
    if !cwd.is_dir() {
        return Err(format!("cwd {} is not a folder", cwd.display()));
    }
    let script = Script::new(shell, &command)?;
    let mut process = script.command(shell)?;
    process.current_dir(&cwd).stdin(Stdio::null()).envs(ENV);
    detach(&mut process);
    if bool_arg(args, "background") {
        return jobs.start(process, script, &command, args, signals);
    }
    let split = bool_arg(args, "split");
    // One pipe for both streams keeps their lines in the order they were written.
    let merged = if split {
        process.stdout(Stdio::piped()).stderr(Stdio::piped());
        None
    } else {
        let (reader, writer) = std::io::pipe().map_err(|e| format!("cannot create a pipe: {e}"))?;
        process
            .stdout(
                writer
                    .try_clone()
                    .map_err(|e| format!("cannot create a pipe: {e}"))?,
            )
            .stderr(writer);
        Some(reader)
    };
    let started = Instant::now();
    let mut child = process
        .spawn()
        .map_err(|e| format!("cannot start the shell: {e}"))?;
    contain(&child);
    // Our copies of the pipe's write end must close, or the reader never sees its end.
    drop(process);
    let streams = match merged {
        Some(reader) => vec![("output", Stream::read(Some(reader)))],
        None => vec![
            ("stdout", Stream::read(child.stdout.take())),
            ("stderr", Stream::read(child.stderr.take())),
        ],
    };
    let (end, texts): (End, Vec<(&str, String)>) = match wait(&mut child, timeout, signals) {
        Some(end) => (end, streams.into_iter().map(|(name, stream)| (name, stream.finish())).collect()),
        None => {
            // What it printed so far is shown now and starts the task's log, which gets the rest
            let mut shown = Vec::new();
            let id = jobs.adopt(child, script, &command, started, |log| {
                shown = streams.iter().map(|(name, stream)| (*name, stream.into_log(log))).collect::<Vec<_>>();
                shown.iter().map(|(_, bytes)| bytes.len() as u64).sum()
            })?;
            let texts = shown.into_iter().map(|(name, bytes)| (name, clean(&String::from_utf8_lossy(&bytes)))).collect();
            (End::Background(id), texts)
        }
    };
    let place = opt_str(args, "cwd")
        .map(|_| format!(" in {}", cwd.display()))
        .unwrap_or_default();
    Ok(report(
        &end,
        started.elapsed(),
        &place,
        &texts,
        num_arg(args, "tail"),
    ))
}

/// How the command ended; `None` when the person sent a message meanwhile and it keeps running.
fn wait(child: &mut Child, timeout: u64, signals: &Signals) -> Option<End> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(End::Exit(status.code())),
            Ok(None) => {}
            Err(_) => return Some(End::Exit(None)),
        }
        let end = if signals.cancel.is_cancelled() {
            End::Stopped
        } else if signals.wrap_up.is_cancelled() {
            return None;
        } else if Instant::now() >= deadline {
            End::TimedOut(timeout)
        } else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        kill_tree(child);
        return Some(end);
    }
}

/// A script in a temp file, deleted when dropped.
pub struct Script(PathBuf);

impl Script {
    fn new(shell: Shell, command: &str) -> Result<Self, String> {
        let (ext, text) = match shell {
            Shell::Bash => ("sh", format!("set -o pipefail\n{command}")),
            // A BOM makes 5.1 read the file as UTF-8.
            Shell::PowerShell => (
                "ps1",
                format!("\u{feff}{PS_PROLOGUE}{command}{PS_EPILOGUE}"),
            ),
        };
        let path = scratch().join(format!("{}.{ext}", unique()));
        std::fs::write(&path, text)
            .map_err(|e| format!("cannot write the script to {}: {e}", path.display()))?;
        Ok(Self(path))
    }

    fn command(&self, shell: Shell) -> Result<Command, String> {
        match shell {
            Shell::Bash => {
                let bash = BASH
                    .as_deref()
                    .ok_or("bash is not available on this machine")?;
                let mut command = Command::new(bash);
                command.arg(&self.0);
                Ok(command)
            }
            Shell::PowerShell => {
                let powershell = POWERSHELL
                    .as_deref()
                    .ok_or("PowerShell is not available on this machine")?;
                let mut command = Command::new(powershell);
                command
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                    ])
                    .arg(&self.0);
                Ok(command)
            }
        }
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One output stream, read on its own thread.
struct Stream {
    sink: Arc<Mutex<Sink>>,
    reader: Option<JoinHandle<()>>,
}

/// Where a stream's output goes: memory for the call's result, or the log once it is a background task.
enum Sink {
    Memory(Vec<u8>),
    Log(File),
}

impl Stream {
    fn read(pipe: Option<impl Read + Send + 'static>) -> Self {
        let sink = Arc::new(Mutex::new(Sink::Memory(Vec::new())));
        let writer = sink.clone();
        let reader = pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    match &mut *writer.lock().unwrap() {
                        Sink::Memory(bytes) => bytes.extend_from_slice(&chunk[..n]),
                        Sink::Log(log) => {
                            let _ = log.write_all(&chunk[..n]);
                        }
                    }
                }
            })
        });
        Self { sink, reader }
    }

    /// Moves the output so far into `log`, where the rest goes too; returns the output so far.
    fn into_log(&self, log: &File) -> Vec<u8> {
        let mut sink = self.sink.lock().unwrap();
        let (Sink::Memory(bytes), Ok(mut log)) = (&mut *sink, log.try_clone()) else { return Vec::new() };
        let bytes = std::mem::take(bytes);
        let _ = log.write_all(&bytes);
        *sink = Sink::Log(log);
        bytes
    }

    /// The text so far; waits a little for the rest when children still hold the pipe.
    fn finish(self) -> String {
        if let Some(reader) = self.reader {
            let until = Instant::now() + DRAIN;
            while !reader.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        match &*self.sink.lock().unwrap() {
            Sink::Memory(bytes) => clean(&String::from_utf8_lossy(bytes)),
            Sink::Log(_) => String::new(),
        }
    }
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9A-Za-z]")
        .expect("valid regex")
});

/// Without color codes, and with only the last state of lines a progress bar rewrote with `\r`.
pub fn clean(text: &str) -> String {
    let text = ANSI.replace_all(text, "");
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').rsplit('\r').next().unwrap_or(""))
        .collect();
    lines.join("\n").trim_end().to_owned()
}

/// The header and the streams; a merged stream goes without a label.
fn report(
    end: &End,
    took: Duration,
    place: &str,
    streams: &[(&str, String)],
    tail: Option<usize>,
) -> String {
    let seconds = format!("{:.1} s{place}", took.as_secs_f64());
    let head = match end {
        End::Exit(Some(code)) => format!("exit {code} ({seconds})"),
        End::Exit(None) => format!("ended without an exit code ({seconds})"),
        End::TimedOut(limit) => {
            format!(
                "timed out after {limit} s; the command and its child processes were killed. Output so far:"
            )
        }
        End::Stopped => {
            format!("stopped by the person after {seconds}; the command was killed. Output so far:")
        }
        End::Background(id) => format!(
            "the person sent you a message after {seconds}, so the command goes on in the background as {id}: \
`task` with id \"{id}\" shows its new output, `wait`s for its end or `stop`s it. Output so far:"
        ),
    };
    let mut parts = vec![head];
    for (name, text) in streams.iter().filter(|(_, text)| !text.is_empty()) {
        match *name {
            "output" => parts.push(shorten(name, text, tail)),
            _ => parts.push(format!("{name}:\n{}", shorten(name, text, tail))),
        }
    }
    if streams.iter().all(|(_, text)| text.is_empty()) {
        parts.push("(no output)".into());
    }
    parts.join("\n")
}

/// The whole text when short, else its head and tail, or only the asked `tail`, and a file with all of it.
pub fn shorten(name: &str, text: &str, tail: Option<usize>) -> String {
    let lines: Vec<String> = text.lines().map(cut).collect();
    let tail = tail
        .map(|n| n.clamp(1, LINES_MAX))
        .filter(|&n| n < lines.len());
    if tail.is_none() && lines.len() <= LINES_MAX && text.len() <= BYTES_MAX {
        return lines.join("\n");
    }
    let full = scratch().join(format!("{}-{name}.txt", unique()));
    let saved = match std::fs::write(&full, text) {
        Ok(()) => format!("all {} lines are in {}", lines.len(), full.display()),
        Err(e) => format!("could not save the full output: {e}"),
    };
    if let Some(n) = tail {
        let shown = lines[lines.len() - n..].join("\n");
        return format!(
            "… ({} earlier lines not shown; {saved})\n{shown}",
            lines.len() - n
        );
    }
    let edge = EDGE.min(lines.len() / 2);
    let skipped = lines.len() - 2 * edge;
    let mut kept = lines[..edge].to_vec();
    kept.push(format!("… ({skipped} lines skipped; {saved})"));
    kept.extend_from_slice(&lines[lines.len() - edge..]);
    kept.join("\n")
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(LINE_MAX) {
        Some((at, _)) => format!("{}… (line cut at {LINE_MAX} characters)", &line[..at]),
        None => line.to_owned(),
    }
}

/// Folder for scripts and full outputs.
pub fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join("nabucode-shell");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn unique() -> String {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "{}-{millis}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Puts the process into a job object that kills it, and what it starts, when NabuCode exits.
#[cfg(windows)]
pub fn contain(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    contain_handle(child.as_raw_handle());
}

/// The same for a process known by its handle.
#[cfg(windows)]
pub fn contain_handle(handle: std::os::windows::io::RawHandle) {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    static JOB: LazyLock<usize> = LazyLock::new(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size,
        );
        job as usize
    });
    unsafe { AssignProcessToJobObject(*JOB as _, handle as _) };
}

#[cfg(unix)]
pub fn contain(_child: &Child) {}

#[cfg(windows)]
pub fn kill_tree(child: &mut Child) {
    let mut taskkill = Command::new("taskkill");
    taskkill
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut taskkill);
    let _ = taskkill.status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
pub fn kill_tree(child: &mut Child) {
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// A program on PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn find_bash() -> Option<PathBuf> {
    if !cfg!(windows) {
        return which("bash").or_else(|| Some(PathBuf::from("/bin/bash")).filter(|p| p.is_file()));
    }
    // Git for Windows: git.exe sits in cmd/, bin/ or mingw64/bin/; its bash is bin/bash.exe at the root.
    // System32's bash.exe is WSL, not this.
    let mut places: Vec<PathBuf> = which("git.exe")
        .map(|git| {
            git.ancestors()
                .map(|dir| dir.join("bin/bash.exe"))
                .collect()
        })
        .unwrap_or_default();
    for (root, rest) in [
        ("ProgramFiles", "Git/bin/bash.exe"),
        ("ProgramW6432", "Git/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
    ] {
        if let Some(root) = std::env::var_os(root) {
            places.push(PathBuf::from(root).join(rest));
        }
    }
    places.into_iter().find(|p| p.is_file())
}

fn find_powershell() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    which("pwsh.exe")
        .or_else(|| which("powershell.exe"))
        .or_else(|| {
            let root = std::env::var_os("SystemRoot")?;
            Some(PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|p| p.is_file())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str, timeout: u64) -> String {
        let dir = std::env::temp_dir();
        run_bash(
            &dir,
            &json!({"command": command, "timeout": timeout}),
            &Signals::default(),
            &Jobs::default(),
        )
        .unwrap()
    }

    #[test]
    fn bash_keeps_text_exact_and_splits_streams() {
        if BASH.is_none() {
            return;
        }
        let script = "printf '%s\\n' 'привет \\n C:\\src \"q\"'\necho oops >&2\necho after\nexit 3";
        let text = bash(script, 30);
        assert!(text.starts_with("exit 3 ("), "{text}");
        assert!(
            text.ends_with(")\nпривет \\n C:\\src \"q\"\noops\nafter"),
            "{text}"
        );
        let split = run_bash(
            &std::env::temp_dir(),
            &json!({"command": script, "split": true}),
            &Signals::default(),
            &Jobs::default(),
        )
        .unwrap();
        assert!(
            split.contains("stdout:\nпривет \\n C:\\src \"q\"\nafter\nstderr:\noops"),
            "{split}"
        );
        assert!(bash("true", 30).ends_with("(no output)"));
        assert!(bash("false | cat", 30).starts_with("exit 1 ("));
        let python = bash("python -c \"print('ёж')\" 2>/dev/null || echo 'ёж'", 30);
        assert!(python.contains("ёж"), "{python}");
    }

    #[test]
    fn bash_timeout_kills_the_tree_and_keeps_output() {
        if BASH.is_none() {
            return;
        }
        let started = Instant::now();
        let text = bash("echo before\nsleep 30 &\nsleep 30", 1);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(
            text.starts_with("timed out after 1 s") && text.contains("before"),
            "{text}"
        );
    }

    #[test]
    fn bash_stops_when_cancelled() {
        if BASH.is_none() {
            return;
        }
        let cancel = Signals::default();
        let stopper = cancel.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            stopper.cancel();
        });
        let text = run_bash(
            &std::env::temp_dir(),
            &json!({"command": "echo start; sleep 30"}),
            &cancel,
            &Jobs::default(),
        )
        .unwrap();
        assert!(
            text.starts_with("stopped by the person") && text.contains("start"),
            "{text}"
        );
    }

    #[test]
    fn a_message_from_the_person_sends_the_command_to_the_background() {
        if BASH.is_none() {
            return;
        }
        let (dir, jobs) = (std::env::temp_dir(), Jobs::default());
        let signals = Signals::default();
        let wrap_up = signals.wrap_up.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            wrap_up.cancel();
        });
        let text = run_bash(&dir, &json!({"command": "echo start; sleep 2; echo end"}), &signals, &jobs).unwrap();
        assert!(text.contains("in the background as task1") && text.contains("start"), "{text}");
        let task = super::super::jobs::run(&dir, &json!({"id": "task1", "wait": 10}), &Signals::default(), &jobs).unwrap();
        assert!(task.contains("\nend\n") && !task.contains("\nstart"), "only the new output: {task}");
    }

    #[test]
    fn long_output_keeps_head_tail_and_a_file() {
        if BASH.is_none() {
            return;
        }
        let text = bash("for i in $(seq 1 500); do echo line $i; done", 30);
        assert!(
            text.contains("line 80\n… (340 lines skipped; all 500 lines are in "),
            "{text}"
        );
        assert!(text.ends_with("line 500"));
        let dir = std::env::temp_dir();
        let end = run_bash(
            &dir,
            &json!({"command": "seq 1 50", "tail": 3}),
            &Signals::default(),
            &Jobs::default(),
        )
        .unwrap();
        assert!(
            end.contains(")\n… (47 earlier lines not shown; all 50 lines are in ")
                && end.ends_with("\n48\n49\n50"),
            "{end}"
        );
        let file = text
            .split("lines are in ")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap();
        assert_eq!(std::fs::read_to_string(file).unwrap().lines().count(), 500);
    }

    #[test]
    fn powershell_gives_utf8_and_the_program_exit_code() {
        if POWERSHELL.is_none() {
            return;
        }
        let dir = std::env::temp_dir();
        let script = "Write-Output 'привет'\nWrite-Error 'плохо'\ncmd /c exit 4";
        let text = run_powershell(
            &dir,
            &json!({"command": script}),
            &Signals::default(),
            &Jobs::default(),
        )
        .unwrap();
        assert!(text.starts_with("exit 4 ("), "{text}");
        assert!(
            text.contains(")\nпривет\n") && text.contains("плохо"),
            "{text}"
        );
    }

    #[test]
    fn background_tasks_report_wait_and_stop() {
        if BASH.is_none() {
            return;
        }
        let dir = std::env::temp_dir();
        let jobs = Jobs::default();
        let cancel = Signals::default();
        let start = |command: &str| {
            run_bash(
                &dir,
                &json!({"command": command, "background": true}),
                &cancel,
                &jobs,
            )
            .unwrap()
        };
        let task = |args: Value| super::super::jobs::run(&dir, &args, &cancel, &jobs).unwrap();

        let failed = start("echo nope; exit 5");
        assert!(
            failed.starts_with("task1 ended at once.\ntask1 exit 5 (")
                && failed.contains("\nnope\nlog: "),
            "{failed}"
        );
        assert!(jobs.notices().is_empty(), "already told");

        let server = "echo booting\nsleep 1\npython -c \"print('listening on 8080')\" || echo 'listening on 8080'\nsleep 30";
        let ready = run_bash(&dir, &json!({"command": server, "background": true, "wait": 20, "until": "listening on \\d+"}), &cancel, &jobs).unwrap();
        assert!(
            ready.starts_with("task2 started in the background."),
            "{ready}"
        );
        assert!(
            ready.contains("\ntask2 running (")
                && ready.contains("`echo booting …`\nbooting\nlistening on 8080\nlog: "),
            "{ready}"
        );
        let quiet = task(json!({"id": "task2"}));
        assert!(quiet.contains("\n(no new output)\n"), "{quiet}");
        let stopped = task(json!({"id": "task2", "stop": true}));
        assert!(stopped.starts_with("task2 stopped, killed"), "{stopped}");

        start("sleep 3; echo built");
        assert!(
            task(json!({"id": "task3", "wait": 1})).starts_with("still running after the wait")
        );
        std::thread::sleep(Duration::from_secs(3));
        let notes = jobs.notices();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("[background] task3 exit 0 (")
                && notes[0].ends_with("`. Its output:\nbuilt"),
            "{notes:?}"
        );
        assert!(jobs.notices().is_empty());
        assert!(
            task(json!({"id": "task3"})).contains("`\nbuilt\nlog: "),
            "a notice does not use up the output"
        );
        let list = task(json!({}));
        assert!(
            list.starts_with("task1 exit 5")
                && list.contains("\ntask2 stopped")
                && list.contains("\ntask3 exit 0"),
            "{list}"
        );
        assert!(
            super::super::jobs::run(&dir, &json!({"id": "task9"}), &cancel, &jobs)
                .unwrap_err()
                .contains("task1, task2, task3")
        );
    }

    #[test]
    fn progress_bars_and_colors_are_cleaned() {
        assert_eq!(
            clean("\x1b[32mok\x1b[0m\n10%\r50%\r100%\r\ndone\r\n"),
            "ok\n100%\ndone"
        );
    }
}
