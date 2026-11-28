use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use ncore::CancelToken;
use serde_json::{Value, json};

/// Background waits of one session, `wait1`, `wait2`, …; kept in a file so they outlive a restart.
#[derive(Default)]
pub struct Waits {
    pub list: Vec<Wait>,
    count: usize,
    file: Option<PathBuf>,
}

pub struct Wait {
    pub id: String,
    pub set: DateTime<Local>,
    pub due: DateTime<Local>,
    pub note: Option<String>,
    pub watch: Watch,
    /// What its notice says, once it fired or ran out.
    pub fired: Option<String>,
    pub told: bool,
    /// Read back from the file after a restart.
    restored: bool,
    /// Stops the watcher of a page condition.
    pub cancel: CancelToken,
}

#[derive(Clone)]
pub enum Watch {
    Time,
    Tasks { ids: Vec<String>, all: bool },
    /// What it waits for on the page, as the notice says it.
    Page(String),
}

impl Wait {
    pub fn new(id: String, due: DateTime<Local>, note: Option<String>, watch: Watch) -> Self {
        Self { id, set: Local::now(), due, note, watch, fired: None, told: false, restored: false, cancel: CancelToken::new() }
    }

    /// `wait1 fires at 15:30:00, in 4 min 10 s: check the deploy`.
    pub fn state(&self) -> String {
        let now = Local::now();
        let line = match (&self.fired, &self.watch) {
            (Some(fired), _) => return fired.clone(),
            (None, Watch::Time) => format!("{} fires at {}, in {}", self.id, clock(self.due, now), span(until(self.due, now))),
            (None, Watch::Tasks { ids, all }) => {
                let which = if *all { "all of" } else { "the first of" };
                format!("{} waits for {which} {} until {}", self.id, ids.join(", "), clock(self.due, now))
            }
            (None, Watch::Page(what)) => format!("{} waits until {what}, at most until {}", self.id, clock(self.due, now)),
        };
        with_note(line, &self.note)
    }

    /// The notice of a time wait that came due.
    fn due_notice(&self, now: DateTime<Local>) -> String {
        let late = until(now, self.due);
        let line = if self.restored && late > Duration::from_secs(1) {
            format!(
                "{} was due at {} and fired at {}, {} late, because NabuCode was off",
                self.id,
                clock(self.due, now),
                clock(now, now),
                span(late)
            )
        } else {
            format!("{} fired at {} (set at {})", self.id, clock(now, now), clock(self.set, now))
        };
        with_note(line, &self.note)
    }
}

impl Waits {
    /// Waits kept in `file`, read back if it exists.
    pub fn kept_in(file: PathBuf) -> Self {
        let mut waits = Self::default();
        waits.file = Some(file.clone());
        let Ok(saved) = std::fs::read(&file).map_err(drop).and_then(|raw| serde_json::from_slice::<Value>(&raw).map_err(drop)) else {
            return waits;
        };
        waits.count = saved["count"].as_u64().unwrap_or(0) as usize;
        let now = Local::now();
        for item in saved["waits"].as_array().into_iter().flatten() {
            let time = |key: &str| item[key].as_i64().and_then(|t| Local.timestamp_millis_opt(t).single()).unwrap_or(now);
            let text = |key: &str| item[key].as_str().map(str::to_owned);
            let mut wait = Wait {
                id: text("id").unwrap_or_default(),
                set: time("set"),
                due: time("due"),
                note: text("note"),
                watch: Watch::Time,
                fired: text("fired"),
                told: false,
                restored: true,
                cancel: CancelToken::new(),
            };
            // The tasks and tabs it watched went away with the old server
            if wait.fired.is_none() && item["watch"].as_str() != Some("time") {
                let what = text("what").unwrap_or_default();
                wait.fired = Some(with_note(format!("{} stopped waiting for {what}: NabuCode restarted, and what it watched is gone", wait.id), &wait.note));
            }
            waits.list.push(wait);
        }
        waits
    }

    pub fn next_id(&mut self) -> String {
        self.count += 1;
        format!("wait{}", self.count)
    }

    pub fn add(&mut self, wait: Wait) {
        self.list.push(wait);
        self.save();
    }

    /// Fires the time waits that came due; returns whether any did.
    pub fn fire_due(&mut self) -> bool {
        let now = Local::now();
        let mut changed = false;
        for wait in self.list.iter_mut().filter(|w| w.fired.is_none() && matches!(w.watch, Watch::Time) && w.due <= now) {
            wait.fired = Some(wait.due_notice(now));
            changed = true;
        }
        if changed {
            self.save();
        }
        changed
    }

    pub fn fire(&mut self, id: &str, notice: String) {
        if let Some(wait) = self.list.iter_mut().find(|w| w.id == id && w.fired.is_none()) {
            wait.fired = Some(notice);
            wait.cancel.cancel();
            self.save();
        }
    }

    pub fn pending(&self) -> bool {
        self.list.iter().any(|w| w.fired.is_some() && !w.told)
    }

    /// Notices of the waits that fired since the model last heard of them.
    pub fn notices(&mut self) -> Vec<String> {
        let notes: Vec<String> = self
            .list
            .iter_mut()
            .filter(|w| w.fired.is_some() && !w.told)
            .map(|w| {
                w.told = true;
                format!("[background] {}", w.fired.as_deref().unwrap_or_default())
            })
            .collect();
        if !notes.is_empty() {
            self.save();
        }
        notes
    }

    /// The notice of a fired wait, now told; `None` while it waits.
    pub fn take_notice(&mut self, id: &str) -> Option<String> {
        let wait = self.list.iter_mut().find(|w| w.id == id)?;
        let fired = wait.fired.clone()?;
        wait.told = true;
        self.save();
        Some(fired)
    }

    pub fn stop(&mut self, id: &str) -> Option<String> {
        let wait = self.list.iter_mut().find(|w| w.id == id)?;
        if wait.fired.is_none() {
            wait.fired = Some(with_note(format!("{} cancelled at {}", wait.id, clock(Local::now(), Local::now())), &wait.note));
            wait.cancel.cancel();
        }
        wait.told = true;
        let state = wait.state();
        self.save();
        Some(state)
    }

    /// Writes the waits the model has not heard the end of; no such waits, no file.
    fn save(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let waits: Vec<Value> = self
            .list
            .iter()
            .filter(|w| !w.told)
            .map(|w| {
                let (watch, what) = match &w.watch {
                    Watch::Time => ("time", String::new()),
                    Watch::Tasks { ids, .. } => ("tasks", ids.join(", ")),
                    Watch::Page(what) => ("page", what.clone()),
                };
                json!({ "id": w.id, "set": w.set.timestamp_millis(), "due": w.due.timestamp_millis(), "note": w.note, "watch": watch, "what": what, "fired": w.fired })
            })
            .collect();
        let result = match waits.is_empty() && self.count == 0 {
            true => std::fs::remove_file(file).or(Ok(())),
            false => std::fs::write(file, json!({ "count": self.count, "waits": waits }).to_string()),
        };
        if let Err(e) = result {
            eprintln!("[waits] could not save {}: {e}", file.display());
        }
    }
}

impl Drop for Waits {
    fn drop(&mut self) {
        self.list.iter().for_each(|w| w.cancel.cancel());
    }
}

pub fn with_note(line: String, note: &Option<String>) -> String {
    match note {
        Some(note) => format!("{line}: {note}"),
        None => line,
    }
}

/// `15:30:00` today, else with the date.
pub fn clock(time: DateTime<Local>, now: DateTime<Local>) -> String {
    match time.date_naive() == now.date_naive() {
        true => time.format("%H:%M:%S").to_string(),
        false => time.format("%Y-%m-%d %H:%M:%S").to_string(),
    }
}

/// Time from `now` to `time`; zero once it passed.
pub fn until(time: DateTime<Local>, now: DateTime<Local>) -> Duration {
    (time - now).to_std().unwrap_or_default()
}

/// `5 s`, `11 min 53 s`, `17 h 42 min`, `3 d 2 h`.
pub fn span(time: Duration) -> String {
    let s = time.as_secs_f64().round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {} s", s / 60, s % 60),
        3600..86400 => format!("{} h {} min", s / 3600, s % 3600 / 60),
        _ => format!("{} d {} h", s / 86400, s % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_like_a_person_says_them() {
        assert_eq!(span(Duration::from_secs(5)), "5 s");
        assert_eq!(span(Duration::from_secs(713)), "11 min 53 s");
        assert_eq!(span(Duration::from_secs(63_720)), "17 h 42 min");
        assert_eq!(span(Duration::from_secs(266_400)), "3 d 2 h");
    }

    #[test]
    fn waits_come_back_after_a_restart() {
        let file = std::env::temp_dir().join(format!("nabu-waits-{}.waits", std::process::id()));
        let now = Local::now();
        {
            let mut waits = Waits::kept_in(file.clone());
            for (watch, due) in [(Watch::Time, now - chrono::Duration::minutes(5)), (Watch::Page("text \"Saved\" in tab 3".into()), now)] {
                let id = waits.next_id();
                waits.add(Wait { id, set: now, due, note: Some("check".into()), watch, fired: None, told: false, restored: false, cancel: CancelToken::new() });
            }
        }
        let mut waits = Waits::kept_in(file.clone());
        assert!(waits.fire_due());
        let notes = waits.notices();
        assert!(notes[0].contains("wait1 was due at") && notes[0].contains("5 min 0 s late") && notes[0].ends_with(": check"), "{notes:?}");
        assert!(notes[1].contains("wait2 stopped waiting for text \"Saved\" in tab 3: NabuCode restarted"), "{notes:?}");
        assert_eq!(waits.next_id(), "wait3");
        drop(waits);
        let _ = std::fs::remove_file(file);
    }
}
