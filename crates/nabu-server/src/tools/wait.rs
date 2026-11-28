use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone};
use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::browse::{self, PageWatch};
use super::waits::{Wait, Watch, clock, span, until, with_note};
use super::{Jobs, Signals, bool_arg, num_arg, opt_str, str_list};

/// Longer waits go to the background.
const BLOCK_MAX: u64 = 600;
const TICK: Duration = Duration::from_millis(100);
const PAGE_TICK: Duration = Duration::from_millis(250);

const DESCRIPTION: &str = "Waits for a time, or for background work to end, then returns; every result gives the current time.
Give `seconds`, or `at` with a local time: \"15:30\", \"15:30:34\" or \"2026-11-28 09:00:34\" (not both). A time without a date \
means today and must be later than now.
Up to 600 s the call blocks. A longer wait, or `background: true`, returns at once with an id like \"wait1\" and runs in the \
background: when it fires, a notice with its `note` comes with your next tool result, or starts a new turn if you are idle. \
The `task` tool lists background waits, waits for them and cancels them (`stop: true`). They survive a restart of \
NabuCode; one that came due while NabuCode was off fires at its start and says how late it is.
`tasks` waits for background tasks, subagents or waits by id (\"task1\", \"sub2\", \"wait1\"). It returns when the first of \
them ends, with its report as `task` gives it and the state of the others; with `all: true`, when all of them have ended. \
Here `seconds` or `at` is the most it waits, 600 s if neither is given; when the time runs out, the result says what is \
still running.
A message from the person ends a blocking wait; background waits go on.";

const PAGE: &str = "
In a browser tab (`tab`), it can also wait until `text` appears on the page, `gone` disappears from it, an element matches \
`selector`, or the URL contains `url`; several of them mean all of them. The result has `met: true` with what was found, \
or `met: false` when the time ran out, which is not an error. As with `tasks`, `seconds` or `at` is the most it waits, \
600 s if neither is given.";

/// The browser conditions come only once the browser tools are loaded.
pub fn def(browser: bool) -> ToolDef {
    let mut properties = json!({
        "seconds": { "type": "integer", "description": "How long to wait. With `tasks`: the most to wait." },
        "at": { "type": "string", "description": "Local time to wait until: \"15:30\", \"15:30:34\" or \"2026-11-28 09:00:34\"." },
        "tasks": { "type": "array", "items": { "type": "string" }, "description": "Ids of background tasks, subagents or waits to wait for." },
        "all": { "type": "boolean", "description": "With `tasks`: return when all of them have ended, not the first." },
        "background": { "type": "boolean", "description": "Return at once with a wait id; a notice comes when it fires." },
        "note": { "type": "string", "description": "Comes back in the notice when a background wait fires." }
    });
    let mut description = DESCRIPTION.to_owned();
    if browser {
        description.push_str(PAGE);
        properties["seconds"]["description"] = json!("How long to wait. With `tasks` or a page condition: the most to wait.");
        let page = json!({
            "tab": { "type": "integer", "description": "Browser tab to watch." },
            "text": { "type": "string", "description": "Wait until this text appears on the page." },
            "gone": { "type": "string", "description": "Wait until this text disappears from the page." },
            "selector": { "type": "string", "description": "Wait until an element matches this CSS selector." },
            "url": { "type": "string", "description": "Wait until the tab's URL contains this." }
        });
        properties.as_object_mut().unwrap().extend(page.as_object().unwrap().clone());
    }
    super::def("wait", &description, json!({ "type": "object", "properties": properties }))
}

pub fn plain_def() -> ToolDef {
    def(false)
}

/// What a `wait` call needs from its session.
pub struct Call {
    pub args: Value,
    pub jobs: Arc<Jobs>,
    pub signals: Signals,
    /// The chat whose browser tabs it watches.
    pub chat: String,
    /// Whether the browser tools are loaded.
    pub browser: bool,
}

pub async fn run(call: Call) -> Result<String, String> {
    let args = &call.args;
    let now = Local::now();
    let due = due_time(args, now)?;
    let tasks = str_list(args, "tasks");
    let page = page_watch(args, call.browser)?;
    if !tasks.is_empty() && page.is_some() {
        return Err("give `tasks` or page conditions, not both".into());
    }
    if let Some(id) = tasks.iter().find(|id| call.jobs.ended(id).is_none()) {
        return Err(call.jobs.unknown_work(id));
    }
    let watching = !tasks.is_empty() || page.is_some();
    let due = match due {
        Some(due) => due,
        None if watching => now + chrono::Duration::seconds(BLOCK_MAX as i64),
        None => {
            let page = if call.browser { " or a page condition" } else { "" };
            return Err(format!("nothing to wait for: give `seconds`, `at`, `tasks`{page}"));
        }
    };
    let long = until(due, now) > Duration::from_secs(BLOCK_MAX);
    if bool_arg(args, "background") || long {
        let started = background(&call, due, tasks, page);
        return Ok(match long && !bool_arg(args, "background") {
            true => format!("longer than {BLOCK_MAX} s, so it runs in the background: {started}"),
            false => started,
        });
    }
    match page {
        Some(page) => block_page(&call, &page, due).await,
        None if !tasks.is_empty() => Ok(block_tasks(&call, &tasks, due).await),
        None => Ok(block_time(&call, due).await),
    }
}

fn due_time(args: &Value, now: DateTime<Local>) -> Result<Option<DateTime<Local>>, String> {
    match (args.get("seconds").filter(|v| !v.is_null()), opt_str(args, "at")) {
        (Some(_), Some(_)) => Err("`seconds` and `at` both given; give one".into()),
        (Some(seconds), None) => {
            let seconds = seconds.as_f64().filter(|s| *s >= 0.0).ok_or("`seconds` must be a number of seconds, 0 or more")?;
            Ok(Some(now + chrono::Duration::milliseconds((seconds * 1000.0) as i64)))
        }
        (None, Some(at)) => parse_at(at, now).map(Some),
        (None, None) => Ok(None),
    }
}

/// `15:30`, `15:30:34` today, or with a date: `2026-11-28 09:00:34`.
fn parse_at(at: &str, now: DateTime<Local>) -> Result<DateTime<Local>, String> {
    let at = at.trim();
    let dated = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(at, format).ok());
    let today = || {
        ["%H:%M:%S", "%H:%M"]
            .iter()
            .find_map(|format| NaiveTime::parse_from_str(at, format).ok())
            .map(|time| now.date_naive().and_time(time))
    };
    let naive = dated
        .or_else(today)
        .ok_or_else(|| format!("{at:?} is not a time; give \"15:30\", \"15:30:34\" or \"2026-11-28 09:00:34\""))?;
    let time = Local
        .from_local_datetime(&naive)
        .earliest()
        .ok_or_else(|| format!("{at} does not exist in local time: the clock skips it"))?;
    if time > now {
        return Ok(time);
    }
    Err(match dated {
        None => format!("{at} today has passed; now {}. Give a later time, or a date", clock(now, now)),
        Some(_) => format!("{at} has passed; now {}", now.format("%Y-%m-%d %H:%M:%S")),
    })
}

fn page_watch(args: &Value, browser: bool) -> Result<Option<PageWatch>, String> {
    let text = |key: &str| opt_str(args, key).map(str::to_owned);
    let watch = PageWatch { tab: 0, text: text("text"), gone: text("gone"), selector: text("selector"), url: text("url") };
    if watch.is_empty() {
        return Ok(None);
    }
    if !browser {
        return Err("page conditions need the browser tools: load them with `tools` first".into());
    }
    let tab = num_arg(args, "tab").ok_or("missing `tab`: the browser tab to watch")?;
    Ok(Some(PageWatch { tab: tab as u32, ..watch }))
}

fn background(call: &Call, due: DateTime<Local>, tasks: Vec<String>, page: Option<PageWatch>) -> String {
    let now = Local::now();
    let all = bool_arg(&call.args, "all");
    let note = opt_str(&call.args, "note").map(str::to_owned);
    let (line, watch) = match (&page, tasks.is_empty()) {
        (Some(page), _) => (format!("waits until {}, at most until {}", page.describe(), clock(due, now)), Watch::Page(page.describe())),
        (None, false) => {
            let which = if all { "all of" } else { "the first of" };
            (format!("waits for {which} {} until {}", tasks.join(", "), clock(due, now)), Watch::Tasks { ids: tasks, all })
        }
        (None, true) => (format!("fires at {}, in {}", clock(due, now), span(until(due, now))), Watch::Time),
    };
    let mut waits = call.jobs.waits.lock().unwrap();
    let id = waits.next_id();
    let wait = Wait::new(id.clone(), due, note.clone(), watch);
    let cancel = wait.cancel.clone();
    waits.add(wait);
    drop(waits);
    if let Some(page) = page {
        let (jobs, chat, id) = (call.jobs.clone(), call.chat.clone(), id.clone());
        tokio::spawn(async move {
            let notice = tokio::select! {
                notice = watch_page(&chat, &page, due) => notice,
                () = cancel.cancelled() => return,
            };
            jobs.waits.lock().unwrap().fire(&id, with_note(format!("{id} {notice}"), &note));
        });
    }
    format!("{id} {line}; now {}", clock(now, now))
}

/// The notice of a background page wait, without its id.
async fn watch_page(chat: &str, page: &PageWatch, due: DateTime<Local>) -> String {
    loop {
        let now = Local::now();
        match browse::page_check(chat, page).await {
            Ok((true, status)) => return format!("fired at {}: met: true; {status}", clock(now, now)),
            Ok((false, status)) if now >= due => return format!("ran out at {}: met: false; {status}", clock(now, now)),
            Err(error) => return format!("stopped at {}: {error}", clock(now, now)),
            Ok(_) => tokio::time::sleep(PAGE_TICK).await,
        }
    }
}

/// Why a blocking wait ended early.
fn interrupted(signals: &Signals) -> Option<&'static str> {
    if signals.cancel.is_cancelled() {
        Some("the person stopped you")
    } else if signals.wrap_up.is_cancelled() {
        Some("the person sent you a message")
    } else {
        None
    }
}

fn now_text() -> String {
    let now = Local::now();
    clock(now, now)
}

async fn block_time(call: &Call, due: DateTime<Local>) -> String {
    let start = Instant::now();
    let end = start + until(due, Local::now());
    let signals = &call.signals;
    tokio::select! {
        () = tokio::time::sleep_until(end.into()) => {}
        () = signals.cancel.cancelled() => {}
        () = signals.wrap_up.cancelled() => {}
    }
    let waited = span(start.elapsed());
    match (interrupted(signals), opt_str(&call.args, "at")) {
        (Some(why), _) => format!("stopped waiting after {waited}: {why}; now {}", now_text()),
        (None, Some(_)) => format!("waited until {}; now {}", clock(due, Local::now()), now_text()),
        (None, None) => format!("waited {waited}; now {}", now_text()),
    }
}

async fn block_tasks(call: &Call, tasks: &[String], due: DateTime<Local>) -> String {
    let start = Instant::now();
    let all = bool_arg(&call.args, "all");
    loop {
        let (ended, running): (Vec<&String>, Vec<&String>) = tasks.iter().partition(|id| call.jobs.ended(id) != Some(false));
        let met = if all { running.is_empty() } else { !ended.is_empty() };
        let why = interrupted(&call.signals);
        if met || why.is_some() || Local::now() >= due {
            let waited = span(start.elapsed());
            let names: Vec<&str> = ended.iter().map(|id| id.as_str()).collect();
            let head = match why {
                _ if met => format!("{} ended after {waited} of waiting", names.join(", ")),
                Some(why) => format!("stopped waiting after {waited}: {why}"),
                None if ended.is_empty() => format!("{waited} passed and none ended"),
                None => format!("{waited} passed and not all ended"),
            };
            let mut text = format!("{head}; now {}.", now_text());
            for id in &ended {
                text.push_str(&format!("\n\n{}", call.jobs.report_of(id)));
            }
            if !running.is_empty() {
                let running: Vec<String> = running.iter().map(|id| call.jobs.state_of(id)).collect();
                text.push_str(&format!("\n\nStill running:\n{}", running.join("\n")));
            }
            return text;
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn block_page(call: &Call, page: &PageWatch, due: DateTime<Local>) -> Result<String, String> {
    let start = Instant::now();
    loop {
        let (met, status) = browse::page_check(&call.chat, page).await?;
        let waited = span(start.elapsed());
        if met {
            return Ok(format!("met: true after {waited}; {status}; now {}", now_text()));
        }
        if let Some(why) = interrupted(&call.signals) {
            return Ok(format!("met: false: stopped waiting after {waited}: {why}; {status}; now {}", now_text()));
        }
        if Local::now() >= due {
            return Ok(format!("met: false: after {waited} {status}; now {}", now_text()));
        }
        tokio::time::sleep(PAGE_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Result<DateTime<Local>, String> {
        let now = Local.with_ymd_and_hms(2026, 11, 28, 15, 18, 7).unwrap();
        parse_at(text, now)
    }

    #[test]
    fn times_are_read_in_every_given_form() {
        assert_eq!(at("15:30").unwrap().format("%F %T").to_string(), "2026-11-28 15:30:00");
        assert_eq!(at("15:30:34").unwrap().format("%F %T").to_string(), "2026-11-28 15:30:34");
        assert_eq!(at("2026-11-29 09:00:34").unwrap().format("%F %T").to_string(), "2026-11-29 09:00:34");
        assert_eq!(at("2026-11-29 09:00").unwrap().format("%F %T").to_string(), "2026-11-29 09:00:00");
    }

    #[test]
    fn a_passed_time_is_an_error_not_tomorrow() {
        assert_eq!(at("15:00").unwrap_err(), "15:00 today has passed; now 15:18:07. Give a later time, or a date");
        assert_eq!(at("2026-11-27 09:00").unwrap_err(), "2026-11-27 09:00 has passed; now 2026-11-28 15:18:07");
        assert!(at("15.30").unwrap_err().starts_with("\"15.30\" is not a time"));
    }

    #[test]
    fn seconds_and_at_do_not_go_together() {
        let now = Local::now();
        assert!(due_time(&json!({ "seconds": 5, "at": "23:59" }), now).unwrap_err().contains("both given"));
        assert!(due_time(&json!({ "seconds": -1 }), now).is_err());
    }

    fn call(args: Value, jobs: Arc<Jobs>) -> Call {
        Call { args, jobs, signals: Signals::default(), chat: String::new(), browser: false }
    }

    #[tokio::test]
    async fn short_waits_block_and_long_ones_go_to_the_background() {
        let jobs = Arc::new(Jobs::default());
        let short = run(call(json!({ "seconds": 0.2 }), jobs.clone())).await.unwrap();
        assert!(short.starts_with("waited 0 s; now "), "{short}");
        let long = run(call(json!({ "seconds": 900, "note": "check" }), jobs.clone())).await.unwrap();
        assert!(long.starts_with("longer than 600 s, so it runs in the background: wait1 fires at "), "{long}");
        let fast = run(call(json!({ "seconds": 0.1, "background": true }), jobs.clone())).await.unwrap();
        assert!(fast.starts_with("wait2 fires at "), "{fast}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(jobs.pending());
        let notes = jobs.notices();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("[background] wait2 fired at "), "{notes:?}");
    }

    #[tokio::test]
    async fn waits_for_other_waits_and_says_what_still_runs() {
        let jobs = Arc::new(Jobs::default());
        run(call(json!({ "seconds": 0.2, "background": true }), jobs.clone())).await.unwrap();
        run(call(json!({ "seconds": 500, "background": true }), jobs.clone())).await.unwrap();
        let text = run(call(json!({ "tasks": ["wait1", "wait2"], "seconds": 5 }), jobs.clone())).await.unwrap();
        assert!(text.starts_with("wait1 ended after "), "{text}");
        assert!(text.contains("wait1 fired at ") && text.contains("Still running:\nwait2 fires at "), "{text}");
        let none = run(call(json!({ "tasks": ["wait2"], "seconds": 0.3 }), jobs.clone())).await.unwrap();
        assert!(none.contains("passed and none ended") && none.contains("Still running:\nwait2 fires at "), "{none}");
        let unknown = run(call(json!({ "tasks": ["task7"] }), jobs.clone())).await.unwrap_err();
        assert_eq!(unknown, "no task task7; this session has wait1, wait2");
        assert!(jobs.notices().is_empty(), "wait1 was reported by the wait, no notice again");
    }

    #[test]
    fn page_conditions_need_the_browser_tools() {
        let error = page_watch(&json!({ "tab": 1, "text": "Saved" }), false).err().unwrap();
        assert!(error.contains("load them with `tools`"));
        assert!(page_watch(&json!({ "text": "Saved" }), true).err().unwrap().contains("missing `tab`"));
    }
}
