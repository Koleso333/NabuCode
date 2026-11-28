use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Local;
use ncore::CancelToken;
use ncore::core::ir::ToolDef;
use regex::Regex;
use serde_json::{Value, json};

use super::shell::{self, Script};
use super::waits::{Watch, Waits, clock, with_note};
use super::{Signals, num_arg, opt_str};

/// How long a new task is watched before the call returns, so a command that fails at once says so.
const SETTLE: Duration = Duration::from_secs(1);
const WAIT_MAX: u64 = 600;
const COMMAND_SHOWN: usize = 80;
/// Lines of output a notice about an ended task carries.
const NOTICE_LINES: usize = 20;

/// Background work of one session: commands `task1`, `task2`, …, subagents `sub1`, `sub2`, … and waits `wait1`, `wait2`, …
/// Running ones are killed when the session is closed or deleted.
#[derive(Default)]
pub struct Jobs {
    list: Mutex<Vec<Job>>,
    subs: Mutex<Vec<Sub>>,
    pub(super) waits: Mutex<Waits>,
}

/// A subagent; it works in its own child session.
struct Sub {
    id: String,
    session: String,
    description: String,
    cancel: CancelToken,
    started: Instant,
    /// Set when its run ended: how long it took and its report.
    end: Option<(Duration, String)>,
    told: bool,
}

impl Sub {
    /// `sub1 running (3.2 s so far): find the parser`.
    fn state(&self) -> String {
        let what = match &self.end {
            None => format!(
                "running ({:.1} s so far)",
                self.started.elapsed().as_secs_f64()
            ),
            Some((took, _)) => format!("finished ({:.1} s)", took.as_secs_f64()),
        };
        format!("{} {what}: {}", self.id, self.description)
    }

    fn report(&mut self) -> String {
        match &self.end {
            Some((_, report)) => {
                self.told = true;
                format!("{}. Its report:\n{report}", self.state())
            }
            None => self.state(),
        }
    }
}

struct Job {
    id: String,
    command: String,
    log: PathBuf,
    child: Child,
    _script: Script,
    started: Instant,
    end: Option<Ended>,
    /// Bytes of the log already shown to the model.
    seen: u64,
    /// Whether the model already knows the task ended.
    told: bool,
}

#[derive(Clone, Copy)]
struct Ended {
    code: Option<i32>,
    took: Duration,
    stopped: bool,
}

impl Job {
    fn new(id: String, command: &str, log: PathBuf, child: Child, script: Script, started: Instant) -> Self {
        Self { id, command: short(command), log, child, _script: script, started, end: None, seen: 0, told: false }
    }

    fn poll(&mut self) {
        if self.end.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.end = Some(Ended {
                code: status.code(),
                took: self.started.elapsed(),
                stopped: false,
            });
        }
    }

    fn stop(&mut self) {
        self.poll();
        if self.end.is_none() {
            shell::kill_tree(&mut self.child);
            self.end = Some(Ended {
                code: None,
                took: self.started.elapsed(),
                stopped: true,
            });
        }
    }

    /// `task1 exit 0 (3.2 s): cargo build`.
    fn state(&self) -> String {
        let what = match self.end {
            None => format!(
                "running ({:.1} s so far)",
                self.started.elapsed().as_secs_f64()
            ),
            Some(Ended {
                stopped: true,
                took,
                ..
            }) => format!(
                "stopped, killed with its child processes ({:.1} s)",
                took.as_secs_f64()
            ),
            Some(Ended {
                code: Some(code),
                took,
                ..
            }) => format!("exit {code} ({:.1} s)", took.as_secs_f64()),
            Some(Ended {
                code: None, took, ..
            }) => format!("ended without an exit code ({:.1} s)", took.as_secs_f64()),
        };
        format!("{} {what}: {}", self.id, self.command)
    }

    /// Log text after what was already shown; while running, only whole lines.
    fn fresh(&mut self) -> String {
        let bytes = self.unseen();
        self.seen += bytes.len() as u64;
        shell::clean(&String::from_utf8_lossy(&bytes))
    }

    /// Log bytes no `task` call showed yet; while running, only whole lines.
    fn unseen(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if let Ok(mut file) = File::open(&self.log) {
            let _ = file.seek(SeekFrom::Start(self.seen));
            let _ = file.read_to_end(&mut bytes);
        }
        if self.end.is_none() {
            bytes.truncate(bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1));
        }
        bytes
    }

    /// Whether a line written since the last look matches `until`.
    fn printed(&self, until: &Regex) -> bool {
        shell::clean(&String::from_utf8_lossy(&self.unseen()))
            .lines()
            .any(|line| until.is_match(line))
    }

    fn lines(&self) -> usize {
        std::fs::read(&self.log).map_or(0, |b| b.iter().filter(|&&c| c == b'\n').count())
    }

    /// State, new output and the log path.
    fn report(&mut self, tail: Option<usize>) -> String {
        let fresh = self.fresh();
        if self.end.is_some() {
            self.told = true;
        }
        let output = if fresh.is_empty() {
            "(no new output)".to_owned()
        } else {
            shell::shorten(&self.id, &fresh, tail)
        };
        format!(
            "{}\n{output}\nlog: {} ({} lines)",
            self.state(),
            self.log.display(),
            self.lines()
        )
    }
}

impl Jobs {
    /// Jobs whose waits are kept in `file` and read back from it.
    pub fn kept_in(file: PathBuf) -> Self {
        let jobs = Self::default();
        *jobs.waits.lock().unwrap() = Waits::kept_in(file);
        jobs
    }

    /// Ids of all background work, for messages about an unknown one.
    pub(super) fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.list.lock().unwrap().iter().map(|j| j.id.clone()).collect();
        ids.extend(self.subs.lock().unwrap().iter().map(|s| s.id.clone()));
        ids.extend(self.waits.lock().unwrap().list.iter().map(|w| w.id.clone()));
        ids
    }

    pub(super) fn unknown_work(&self, id: &str) -> String {
        format!("no task {id}; this session has {}", or_none(&self.ids()))
    }

    /// Whether the task, subagent or wait `id` ended; `None` when there is none.
    pub(super) fn ended(&self, id: &str) -> Option<bool> {
        if let Some(job) = self.list.lock().unwrap().iter_mut().find(|j| j.id == id) {
            job.poll();
            return Some(job.end.is_some());
        }
        if let Some(sub) = self.subs.lock().unwrap().iter().find(|s| s.id == id) {
            return Some(sub.end.is_some());
        }
        let mut waits = self.waits.lock().unwrap();
        waits.fire_due();
        waits.list.iter().find(|w| w.id == id).map(|w| w.fired.is_some())
    }

    pub(super) fn state_of(&self, id: &str) -> String {
        if let Some(job) = self.list.lock().unwrap().iter().find(|j| j.id == id) {
            return job.state();
        }
        if let Some(sub) = self.subs.lock().unwrap().iter().find(|s| s.id == id) {
            return sub.state();
        }
        let state = self.waits.lock().unwrap().list.iter().find(|w| w.id == id).map(|w| w.state());
        state.unwrap_or_else(|| self.unknown_work(id))
    }

    /// Its report as `task` gives it; the model then knows it ended.
    pub(super) fn report_of(&self, id: &str) -> String {
        if let Some(job) = self.list.lock().unwrap().iter_mut().find(|j| j.id == id) {
            return job.report(None);
        }
        if let Some(sub) = self.subs.lock().unwrap().iter_mut().find(|s| s.id == id) {
            return format!("{}\nsession: {}", sub.report(), sub.session);
        }
        let notice = self.waits.lock().unwrap().take_notice(id);
        notice.unwrap_or_else(|| self.state_of(id))
    }

    /// Fires the waits that came due or whose tasks ended.
    fn poll_waits(&self) {
        let watching: Vec<_> = {
            let mut waits = self.waits.lock().unwrap();
            waits.fire_due();
            waits
                .list
                .iter()
                .filter(|w| w.fired.is_none())
                .filter_map(|w| match &w.watch {
                    Watch::Tasks { ids, all } => Some((w.id.clone(), ids.clone(), *all, w.due, w.note.clone())),
                    _ => None,
                })
                .collect()
        };
        for (id, ids, all, due, note) in watching {
            let (ended, running): (Vec<&String>, Vec<&String>) = ids.iter().partition(|t| self.ended(t) != Some(false));
            let met = if all { running.is_empty() } else { !ended.is_empty() };
            let now = Local::now();
            if !met && now < due {
                continue;
            }
            let ended: Vec<String> = ended.iter().map(|t| self.state_of(t)).collect();
            let mut line = match met {
                true => format!("{id} fired at {}: {}", clock(now, now), ended.join("; ")),
                false => format!("{id} ran out at {}", clock(now, now)),
            };
            if !running.is_empty() {
                let running: Vec<&str> = running.iter().map(|t| t.as_str()).collect();
                line.push_str(&format!("; still running: {}", running.join(", ")));
            }
            self.waits.lock().unwrap().fire(&id, with_note(line, &note));
        }
    }

    /// The subagent working in `session`.
    pub fn sub_of(&self, session: &str) -> Option<String> {
        self.subs.lock().unwrap().iter().find(|s| s.session == session).map(|s| s.id.clone())
    }

    /// Whether a wait fired and the model was not told yet.
    pub fn wait_fired(&self) -> bool {
        self.poll_waits();
        self.waits.lock().unwrap().pending()
    }

    /// Starts `process` with its output going to a log file; `script` lives as long as the task.
    pub fn start(
        &self,
        mut process: Command,
        script: Script,
        command: &str,
        args: &Value,
        signals: &Signals,
    ) -> Result<String, String> {
        let mut list = self.list.lock().unwrap();
        let (id, log, file) = new_log(&list)?;
        let copy = file
            .try_clone()
            .map_err(|e| format!("cannot open the log: {e}"))?;
        process.stdout(copy).stderr(file);
        let child = process
            .spawn()
            .map_err(|e| format!("cannot start the shell: {e}"))?;
        shell::contain(&child);
        list.push(Job::new(id.clone(), command, log, child, script, Instant::now()));
        drop(list);

        let started = format!(
            "{id} started in the background. `task` with id \"{id}\" shows its status and new output, \
`wait` waits for its end or for a line (`until`), `stop` kills it. When it ends, a notice comes with your next tool result or message."
        );
        if num_arg(args, "wait").is_some() {
            return Ok(format!("{started}\n{}", self.look(&id, args, signals)?));
        }
        let settled = Instant::now() + SETTLE;
        while Instant::now() < settled {
            std::thread::sleep(Duration::from_millis(50));
            let mut list = self.list.lock().unwrap();
            let job = list.iter_mut().find(|j| j.id == id).expect("just added");
            job.poll();
            if job.end.is_some() {
                return Ok(format!("{id} ended at once.\n{}", job.report(None)));
            }
        }
        let list = self.list.lock().unwrap();
        let job = list.iter().find(|j| j.id == id).expect("just added");
        Ok(format!(
            "{started}\n{}\nlog: {}",
            job.state(),
            job.log.display()
        ))
    }

    /// Takes over a command that ran in the foreground and returns its task id.
    /// `fill` moves its output into the new log and says how many bytes of it the agent already saw.
    pub fn adopt(
        &self,
        child: Child,
        script: Script,
        command: &str,
        started: Instant,
        fill: impl FnOnce(&File) -> u64,
    ) -> Result<String, String> {
        let mut list = self.list.lock().unwrap();
        let (id, log, file) = new_log(&list)?;
        let mut job = Job::new(id.clone(), command, log, child, script, started);
        job.seen = fill(&file);
        list.push(job);
        Ok(id)
    }

    /// Registers a subagent run in `session`, or a new run of the subagent `id`; returns its id.
    pub fn sub_start(
        &self,
        id: Option<&str>,
        session: &str,
        description: &str,
        background: bool,
        cancel: &CancelToken,
    ) -> String {
        let mut subs = self.subs.lock().unwrap();
        let sub = Sub {
            id: id.map_or_else(|| format!("sub{}", subs.len() + 1), str::to_owned),
            session: session.into(),
            description: description.into(),
            cancel: cancel.clone(),
            started: Instant::now(),
            end: None,
            told: !background,
        };
        let id = sub.id.clone();
        match subs.iter_mut().find(|s| s.id == id) {
            Some(old) => *old = sub,
            None => subs.push(sub),
        }
        id
    }

    /// A subagent the agent waited for goes on in the background; its report comes as a notice.
    pub fn sub_detach(&self, id: &str) {
        if let Some(sub) = self.subs.lock().unwrap().iter_mut().find(|s| s.id == id) {
            sub.told = false;
        }
    }

    pub fn sub_end(&self, id: &str, report: &str) {
        if let Some(sub) = self.subs.lock().unwrap().iter_mut().find(|s| s.id == id) {
            sub.end = Some((sub.started.elapsed(), report.into()));
        }
    }

    /// The child session of the subagent `id`, when it is not running.
    pub fn sub_session(&self, id: &str) -> Result<(String, String), String> {
        let subs = self.subs.lock().unwrap();
        let Some(sub) = subs.iter().find(|s| s.id == id) else {
            let ids: Vec<String> = subs.iter().map(|s| s.id.clone()).collect();
            return Err(format!(
                "no subagent {id}; this session has {}",
                or_none(&ids)
            ));
        };
        match sub.end {
            None => Err(format!(
                "{id} is still running; `task` with id \"{id}\" and `wait` waits for its report"
            )),
            Some(_) => Ok((sub.session.clone(), sub.description.clone())),
        }
    }

    /// Whether a task or background subagent ended and the model was not told yet.
    pub fn pending(&self) -> bool {
        if self.wait_fired() {
            return true;
        }
        if self
            .subs
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.end.is_some() && !s.told)
        {
            return true;
        }
        let mut list = self.list.lock().unwrap();
        list.iter_mut().any(|job| {
            job.poll();
            job.end.is_some() && !job.told
        })
    }

    /// A note for each task that ended since the model last heard of it, with the end of its unseen output.
    pub fn notices(&self) -> Vec<String> {
        self.poll_waits();
        let mut list = self.list.lock().unwrap();
        let mut notes = Vec::new();
        for job in list.iter_mut() {
            job.poll();
            if job.end.is_none() || job.told {
                continue;
            }
            job.told = true;
            // A notice only peeks: the next `task` call still shows this output.
            let fresh = shell::clean(&String::from_utf8_lossy(&job.unseen()));
            let lines: Vec<&str> = fresh.lines().collect();
            let shown = lines[lines.len().saturating_sub(NOTICE_LINES)..].join("\n");
            let output = match lines.len() {
                0 => String::new(),
                n if n <= NOTICE_LINES => format!(" Its output:\n{shown}"),
                n => format!(
                    " Last {NOTICE_LINES} of {n} new lines; `task` with id \"{}\" shows them all:\n{shown}",
                    job.id
                ),
            };
            notes.push(format!("[background] {}.{output}", job.state()));
        }
        for sub in self
            .subs
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|s| s.end.is_some() && !s.told)
        {
            notes.push(format!("[background] {}", sub.report()));
        }
        notes.extend(self.waits.lock().unwrap().notices());
        notes
    }

    /// State of the subagent `id`; waits for its end or stops it like a task.
    fn look_sub(&self, id: &str, args: &Value, signals: &Signals) -> Result<String, String> {
        let wait =
            Duration::from_secs(num_arg(args, "wait").map_or(0, |s| (s as u64).min(WAIT_MAX)));
        let deadline = Instant::now() + wait;
        if args.get("stop").and_then(Value::as_bool).unwrap_or(false)
            && let Some(sub) = self.subs.lock().unwrap().iter().find(|s| s.id == id)
        {
            sub.cancel.cancel();
        }
        loop {
            let mut subs = self.subs.lock().unwrap();
            let ids: Vec<String> = subs.iter().map(|s| s.id.clone()).collect();
            let sub = subs
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or_else(|| format!("no subagent {id}; this session has {}", or_none(&ids)))?;
            if sub.end.is_some() || Instant::now() >= deadline || stopped(signals) {
                let note = match (sub.end.is_none() && !wait.is_zero(), signals.wrap_up.is_cancelled()) {
                    (true, true) => WRAPPED_UP,
                    (true, false) => "still running after the wait.\n",
                    (false, _) => "",
                };
                return Ok(format!("{note}{}\nsession: {}", sub.report(), sub.session));
            }
            drop(subs);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// State of the wait `id`; waits for it to fire or cancels it.
    fn look_wait(&self, id: &str, args: &Value, signals: &Signals) -> Result<String, String> {
        if args.get("stop").and_then(Value::as_bool).unwrap_or(false) {
            let stopped = self.waits.lock().unwrap().stop(id);
            return stopped.ok_or_else(|| self.unknown_work(id));
        }
        let wait = Duration::from_secs(num_arg(args, "wait").map_or(0, |s| (s as u64).min(WAIT_MAX)));
        let deadline = Instant::now() + wait;
        loop {
            self.poll_waits();
            if self.ended(id).ok_or_else(|| self.unknown_work(id))? {
                return Ok(self.report_of(id));
            }
            if Instant::now() >= deadline || stopped(signals) {
                let note = match (!wait.is_zero(), signals.wrap_up.is_cancelled()) {
                    (true, true) => "stopped waiting: the person sent you a message; it keeps waiting.\n",
                    (true, false) => "still waiting after the wait.\n",
                    (false, _) => "",
                };
                return Ok(format!("{note}{}", self.state_of(id)));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn look(&self, id: &str, args: &Value, signals: &Signals) -> Result<String, String> {
        if id.starts_with("sub") {
            return self.look_sub(id, args, signals);
        }
        if id.starts_with("wait") {
            return self.look_wait(id, args, signals);
        }
        let until = match opt_str(args, "until") {
            Some(pattern) => Some(
                Regex::new(pattern)
                    .map_err(|e| format!("bad regular expression in `until`: {e}"))?,
            ),
            None => None,
        };
        let wait = num_arg(args, "wait").map(|s| Duration::from_secs((s as u64).min(WAIT_MAX)));
        let deadline = Instant::now() + wait.unwrap_or_default();
        loop {
            let mut list = self.list.lock().unwrap();
            let at = list
                .iter()
                .position(|j| j.id == id)
                .ok_or_else(|| unknown(id, &list))?;
            let job = &mut list[at];
            if args.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                job.stop();
                return Ok(job.report(num_arg(args, "tail")));
            }
            job.poll();
            let found = until.as_ref().is_some_and(|until| job.printed(until));
            if job.end.is_some() || found || Instant::now() >= deadline || stopped(signals) {
                let silent = job.log.metadata().map_or(true, |m| m.len() <= job.seen);
                let note = if wait.is_some() && !found && job.end.is_none() {
                    match (&until, signals.cancel.is_cancelled()) {
                        (_, true) => {
                            "stopped waiting: the person stopped you; the task keeps running.\n"
                        }
                        _ if signals.wrap_up.is_cancelled() => WRAPPED_UP,
                        (Some(_), _) if silent => {
                            "no line matched `until` in the wait time: the task wrote nothing at all yet \
(some programs hold their output back when it does not go to a terminal); it keeps running.\n"
                        }
                        (Some(_), _) => {
                            "no new line matched `until` in the wait time; the task keeps running.\n"
                        }
                        (None, _) => "still running after the wait; it keeps running.\n",
                    }
                } else {
                    ""
                };
                return Ok(format!("{note}{}", job.report(num_arg(args, "tail"))));
            }
            drop(list);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn overview(&self) -> String {
        let mut list = self.list.lock().unwrap();
        let mut lines: Vec<String> = list
            .iter_mut()
            .map(|job| {
                job.poll();
                format!("{}\n  log: {}", job.state(), job.log.display())
            })
            .collect();
        lines.extend(self.subs.lock().unwrap().iter().map(Sub::state));
        drop(list);
        self.poll_waits();
        lines.extend(self.waits.lock().unwrap().list.iter().filter(|w| !w.told).map(|w| w.state()));
        if lines.is_empty() {
            return "no background tasks, subagents or waits in this session".into();
        }
        lines.join("\n")
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        for job in self.list.get_mut().unwrap().iter_mut() {
            job.stop();
        }
        for sub in self.subs.get_mut().unwrap().iter() {
            sub.cancel.cancel();
        }
    }
}

const WRAPPED_UP: &str = "stopped waiting: the person sent you a message; it keeps running.\n";

fn stopped(signals: &Signals) -> bool {
    signals.cancel.is_cancelled() || signals.wrap_up.is_cancelled()
}

/// The next task id and its log file.
fn new_log(list: &[Job]) -> Result<(String, PathBuf, File), String> {
    let id = format!("task{}", list.len() + 1);
    let log = shell::scratch().join(format!("{}-{id}.log", shell::unique()));
    let file = File::create(&log).map_err(|e| format!("cannot create the log {}: {e}", log.display()))?;
    Ok((id, log, file))
}

fn or_none(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".into()
    } else {
        ids.join(", ")
    }
}

fn unknown(id: &str, list: &[Job]) -> String {
    let ids: Vec<&str> = list.iter().map(|j| j.id.as_str()).collect();
    match ids.is_empty() {
        true => format!("no task {id}: this session has no background tasks"),
        false => format!("no task {id}; this session has {}", ids.join(", ")),
    }
}

/// The command's first line, shortened, in backticks.
fn short(command: &str) -> String {
    let first = command.trim().lines().next().unwrap_or("");
    let more = command.trim().lines().count() > 1;
    match first.char_indices().nth(COMMAND_SHOWN) {
        Some((at, _)) => format!("`{}…`", &first[..at]),
        None if more => format!("`{first} …`"),
        None => format!("`{first}`"),
    }
}

pub fn def() -> ToolDef {
    super::def(
        "task",
        "Checks, waits for or stops a background command started by bash or powershell with `background: true`, \
or a subagent started by `agent` (ids like \"sub1\"; for those, `wait` returns at its end with its full report, \
`stop` stops it, and `until`/`tail` do not apply), or a background wait started by `wait` (ids like \"wait1\"; `wait` waits for it to fire, `stop` cancels it). Without `id` lists the session's tasks, subagents and waits. With `id` shows the task's state (running, or its exit code) and its new output, \
cleaned of colors: each `task` call shows only lines no earlier `task` call showed (notices do not count, so output a notice \
showed comes again with the next `task` call). \
The whole log stays in the file named in the result: read or grep it for earlier lines. \
`wait: N` blocks up to N seconds (at most 600) until the task ends; with `until`, also until a new output line matches \
that regular expression — for a server, wait for its \"listening\" line instead of sleeping. The `wait` tool waits for \
the first or all of several at once (`tasks`). `stop: true` kills the task \
and its child processes; a task that already ended keeps its exit code. With `tail`, the earlier new lines are skipped \
and count as seen. When a task ends, a notice with the last lines of its output comes with your next tool result or message. \
When you have nothing else to do until a task ends, call `task` with a long `wait` instead of short checks in a row. Programs write to a log, not a terminal: Python is unbuffered here, but other programs may hold output \
back until they exit. `task` calls run in parallel with the shell calls of the same step. Background tasks keep running \
between your steps and messages, are not killed when the person stops you, and end when the session is closed.",
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Task id, like \"task1\", \"sub1\" or \"wait1\". Omit to list all of them." },
                "wait": { "type": "integer", "description": "Seconds to wait for the task to end (or for `until`). Default 0: look right away." },
                "until": { "type": "string", "description": "Regular expression; with `wait`, return as soon as a new output line matches it." },
                "stop": { "type": "boolean", "description": "Kill the task and its child processes." },
                "tail": { "type": "integer", "description": "Show only the last N lines of the new output." }
            }
        }),
    )
}

pub fn run(
    _directory: &Path,
    args: &Value,
    signals: &Signals,
    jobs: &Jobs,
) -> Result<String, String> {
    match opt_str(args, "id") {
        Some(id) => jobs.look(id, args, signals),
        None => Ok(jobs.overview()),
    }
}
