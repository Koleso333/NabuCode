mod archive;
mod compact;
pub(crate) mod id;
mod interrupt;
mod marks;
mod questions;
mod queue;
mod run;
mod store;
mod subagent;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nabu_protocol::{
    CompactionPhase, CompactionSettings, ContextParts, Created, Event, FilePart, Message, MessagePage, MessageWithParts,
    ModelRef, Part, Prompt, PromptPart, CommandPart, Session, SessionStatus, SessionTime, TextPart, ToolState,
    UserMessage,
};
use ncore::CancelToken;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::models::Models;
use crate::tools::Jobs;
use archive::{Archive, Entry};
pub use interrupt::Interrupt;
use store::{Doc, Store};
pub use marks::commands;
pub use subagent::Spawner;

#[derive(Default)]
struct State {
    docs: HashMap<String, Doc>,
    running: HashMap<String, Arc<Interrupt>>,
    /// Background tasks by session; dropping a session's entry kills them.
    jobs: HashMap<String, Arc<Jobs>>,
    /// Metadata a running tool gave its call, by `session/call id`, until the call ends.
    links: HashMap<String, Value>,
    /// Questions waiting for the user, by request id.
    questions: HashMap<String, questions::Waiting>,
    compaction: CompactionSettings,
    compacting: HashMap<String, CompactionPhase>,
    /// Prompts sent while their session answers, by session.
    queue: HashMap<String, Vec<Prompt>>,
    /// The last queued message stored mid-turn, by session, until the turn's next step picks it up.
    delivered: HashMap<String, String>,
    /// The queued message an interruption sends ahead of the rest, by session.
    interrupting: HashMap<String, String>,
    /// Sessions whose queue sits out the step that answers the interrupting message.
    held: std::collections::HashSet<String>,
    /// Sessions compacting because the person asked; an interruption cancels it.
    asked_compaction: std::collections::HashSet<String>,
    /// Where skills are looked for, the first source winning.
    skill_sources: Vec<nabu_protocol::SkillSource>,
    /// Files of the skills turned off.
    disabled_skills: Vec<String>,
}

/// Chat sessions: history on disk, replies streamed to clients as events.
pub struct Sessions {
    store: Store,
    /// Folders of saved sessions without a project.
    chats: std::path::PathBuf,
    archive: Archive,
    models: Arc<Models>,
    events: broadcast::Sender<Event>,
    state: Mutex<State>,
}

impl Sessions {
    pub fn load(
        data: &std::path::Path,
        models: Arc<Models>,
        events: broadcast::Sender<Event>,
    ) -> Arc<Self> {
        let store = Store::new(data.join("sessions"));
        let docs: HashMap<String, Doc> = store
            .load_all()
            .into_iter()
            .map(|mut doc| {
                // A session left nameless, by a bug or an older version, keeps the placeholder for good
                let nameless = doc.session.title.trim().is_empty();
                if nameless {
                    doc.session.title = UNTITLED.into();
                }
                // Older versions only marked archived sessions: those come back to the lists
                let marked = doc.session.time.archived.take().is_some();
                if nameless || marked {
                    store.save(&mut doc);
                }
                (doc.session.id.clone(), doc)
            })
            .collect();
        let jobs = waiting(&store, &docs);
        let sessions = Arc::new(Self {
            store,
            chats: data.join("chats"),
            archive: Archive::load(data.join("archive")),
            models,
            events,
            state: Mutex::new(State {
                jobs,
                docs,
                skill_sources: crate::skills::default_sources(),
                ..State::default()
            }),
        });
        sessions.watch();
        sessions
    }

    pub fn list(&self, directory: &str) -> Vec<Session> {
        let state = self.state.lock().unwrap();
        let mut list: Vec<Session> = state
            .docs
            .values()
            .map(|d| &d.session)
            .filter(|s| s.directory == directory && !s.temporary)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn recent(&self, limit: usize) -> Vec<Session> {
        let state = self.state.lock().unwrap();
        let mut list: Vec<Session> = state
            .docs
            .values()
            .map(|d| &d.session)
            .filter(|s| s.parent_id.is_none() && !s.temporary)
            .cloned()
            .collect();
        list.sort_by(|a, b| b.time.updated.cmp(&a.time.updated));
        list.truncate(limit);
        list
    }

    /// An empty `directory` gives the session a folder of its own: under the data folder, or in temp for a temporary one.
    pub fn create(&self, directory: String, temporary: bool, tools: Option<Vec<String>>, chrome: Option<String>) -> Result<Session, String> {
        let id = id::descending("ses");
        let no_project = directory.trim().is_empty();
        let directory = match (no_project, temporary) {
            (false, _) => directory,
            (true, false) => own_folder(&self.chats.join(&id))?,
            (true, true) => own_folder(&std::env::temp_dir().join("nabucode-chats").join(&id))?,
        };
        let mut session = self.blank(id, directory, UNTITLED.into(), None);
        (session.no_project, session.temporary) = (no_project, temporary);
        Ok(self.add(session, tools, chrome))
    }

    /// A session for a subagent of `parent`, in the same directory.
    fn create_child(
        &self,
        parent: &str,
        title: &str,
        tools: Option<Vec<String>>,
    ) -> Result<Session, String> {
        let parent_session = self.get(parent)?;
        let mut session = self.blank(id::descending("ses"), parent_session.directory, named(title), Some(parent.into()));
        (session.no_project, session.temporary) = (parent_session.no_project, parent_session.temporary);
        Ok(self.add(session, tools, None))
    }

    fn blank(&self, id: String, directory: String, title: String, parent_id: Option<String>) -> Session {
        let now = id::now();
        Session {
            id,
            directory,
            title,
            time: SessionTime { created: now, updated: now, archived: None },
            parent_id,
            compactions: Vec::new(),
            no_project: false,
            temporary: false,
        }
    }

    fn add(&self, session: Session, tools: Option<Vec<String>>, chrome: Option<String>) -> Session {
        let mut doc = Doc {
            session: session.clone(),
            messages: Vec::new(),
            tools,
            chrome,
            compact: None,
        };
        self.store.save(&mut doc);
        self.state
            .lock()
            .unwrap()
            .docs
            .insert(session.id.clone(), doc);
        self.emit(Event::SessionUpdated {
            session: session.clone(),
        });
        session
    }

    fn set_tools(&self, id: &str, tools: Option<Vec<String>>) {
        let mut state = self.state.lock().unwrap();
        if let Some(doc) = state.docs.get_mut(id) {
            doc.tools = tools;
            self.store.save(doc);
        }
    }

    /// The tools a subagent session may use, and whether the session is a subagent's.
    fn tools_of(&self, id: &str) -> (Option<Vec<String>>, bool) {
        let state = self.state.lock().unwrap();
        state.docs.get(id).map_or((None, false), |d| {
            (d.tools.clone(), d.session.parent_id.is_some())
        })
    }

    /// What the session's next request would hold, answering its last user message.
    pub fn context_parts(self: &Arc<Self>, id: &str) -> Result<ContextParts, String> {
        let (directory, user) = {
            let state = self.state.lock().unwrap();
            let doc = state.docs.get(id).ok_or_else(|| format!("unknown session {id}"))?;
            let user = doc.messages.iter().rev().find_map(|m| match &m.info {
                Message::User(u) => Some(u.clone()),
                Message::Assistant(_) => None,
            });
            (doc.session.directory.clone(), user.ok_or("the session has no messages yet")?)
        };
        Ok(run::context_parts(self, &directory, &user))
    }

    /// The model of the session's last user message.
    fn last_model(&self, id: &str) -> Option<ModelRef> {
        let state = self.state.lock().unwrap();
        state
            .docs
            .get(id)?
            .messages
            .iter()
            .rev()
            .find_map(|m| match &m.info {
                Message::User(u) => Some(u.model.clone()),
                Message::Assistant(_) => None,
            })
    }

    /// Gives a running tool call metadata the UI shows, like the session of the subagent it started.
    pub(crate) fn link(&self, session: &str, call_id: &str, metadata: Value) {
        let updated = {
            let mut state = self.state.lock().unwrap();
            state
                .links
                .insert(format!("{session}/{call_id}"), metadata.clone());
            let Some(doc) = state.docs.get_mut(session) else {
                return;
            };
            let directory = doc.session.directory.clone();
            let part = doc
                .messages
                .iter_mut()
                .rev()
                .flat_map(|m| m.parts.iter_mut())
                .find_map(|p| match p {
                    Part::Tool(t) if t.call_id == call_id => Some(t),
                    _ => None,
                });
            part.and_then(|t| match &mut t.state {
                ToolState::Running { metadata: old, .. } => {
                    *old = metadata;
                    Some((directory, Part::Tool(t.clone())))
                }
                _ => None,
            })
        };
        if let Some((directory, part)) = updated {
            self.emit(Event::PartUpdated { directory, part });
        }
    }

    /// The metadata `link` gave the call; with `end`, forgets it.
    fn linked(&self, session: &str, call_id: &str, end: bool) -> Value {
        let mut state = self.state.lock().unwrap();
        let key = format!("{session}/{call_id}");
        let value = if end {
            state.links.remove(&key)
        } else {
            state.links.get(&key).cloned()
        };
        value.unwrap_or(Value::Null)
    }

    pub fn get(&self, id: &str) -> Result<Session, String> {
        let state = self.state.lock().unwrap();
        state
            .docs
            .get(id)
            .map(|d| d.session.clone())
            .ok_or_else(|| missing(id))
    }

    pub fn statuses(&self, directory: &str) -> HashMap<String, SessionStatus> {
        let state = self.state.lock().unwrap();
        state
            .running
            .keys()
            .filter(|id| {
                state
                    .docs
                    .get(*id)
                    .is_some_and(|d| d.session.directory == directory)
            })
            .map(|id| {
                (
                    id.clone(),
                    SessionStatus::Busy {
                        compaction: state.compacting.get(id).copied(),
                    },
                )
            })
            .collect()
    }

    pub fn messages(&self, id: &str) -> Result<Vec<MessageWithParts>, String> {
        let state = self.state.lock().unwrap();
        state
            .docs
            .get(id)
            .map(|d| d.messages.clone())
            .ok_or_else(|| missing(id))
    }

    pub fn page(&self, id: &str, before: Option<&str>, limit: usize) -> Result<MessagePage, String> {
        let (mut messages, more) = {
            let state = self.state.lock().unwrap();
            let all = &state.docs.get(id).ok_or_else(|| missing(id))?.messages;
            let end = before.and_then(|b| all.iter().position(|m| m.info.id() == b)).unwrap_or(all.len());
            // A page starts with the user's message of its first turn, so no reply comes without its question
            let mut start = end.saturating_sub(limit);
            while start > 0 && !matches!(all[start].info, Message::User(_)) {
                start -= 1;
            }
            (all[start..end].to_vec(), start > 0)
        };
        self.store.link_files(id, &mut messages);
        Ok(MessagePage { messages, more })
    }

    pub fn attachment(&self, session: &str, name: &str) -> Option<std::path::PathBuf> {
        self.store.attachment(session, name)
    }

    /// Where an uploaded file of a part still to be sent goes, and the reference that part carries then.
    pub fn upload(&self, session: &str, part: &str, mime: &str, filename: Option<String>) -> Result<(std::path::PathBuf, String), String> {
        self.get(session)?;
        let file = FilePart {
            id: part.into(),
            session_id: session.into(),
            message_id: String::new(),
            mime: mime.into(),
            filename,
            url: String::new(),
        };
        self.store.upload(&file).map_err(|e| e.to_string())
    }

    /// A new name is not activity: the session keeps its place in the lists.
    pub fn rename(&self, id: &str, title: String) -> Result<(), String> {
        self.edit(id, |s| s.title = named(&title))
    }

    /// Packs the session with its subagents' sessions into a zip and takes them out of the lists.
    pub fn archive(&self, id: &str) -> Result<(), String> {
        if self.get(id)?.temporary {
            return Err("a temporary session can't be archived".into());
        }
        let ids = self.tree(id);
        ids.iter().for_each(|id| self.stop(id));
        let mut docs: Vec<Doc> = {
            let mut state = self.state.lock().unwrap();
            ids.iter().filter_map(|id| state.docs.remove(id)).collect()
        };
        if let Err(e) = self.pack(&mut docs) {
            let mut state = self.state.lock().unwrap();
            state.docs.extend(docs.into_iter().map(|doc| (doc.session.id.clone(), doc)));
            return Err(format!("could not archive the session: {e}"));
        }
        let archived = id::now();
        for mut doc in docs {
            self.store.remove(&doc.session.id);
            doc.session.time.archived = Some(archived);
            self.emit(Event::SessionUpdated { session: doc.session });
        }
        Ok(())
    }

    fn pack(&self, docs: &mut [Doc]) -> std::io::Result<()> {
        let mut files = Vec::new();
        for doc in docs.iter_mut() {
            self.store.save_checked(doc)?;
            files.extend(self.store.files_of(&doc.session.id));
        }
        let session = &docs[0].session;
        let entry = Entry {
            id: session.id.clone(),
            title: session.title.clone(),
            directory: session.directory.clone(),
            updated: session.time.updated,
            zip: String::new(),
        };
        self.archive.pack(entry, &files)
    }

    /// Brings an archived session back to the lists with its subagents' sessions.
    pub fn unarchive(&self, id: &str) -> Result<(), String> {
        let restore = || -> Result<Vec<Doc>, String> {
            let ids = self.archive.unpack(id, self.store.dir()).map_err(|e| e.to_string())?;
            ids.iter().map(|id| self.store.load_one(id)).collect()
        };
        let docs = restore().map_err(|e| format!("could not unarchive the session: {e}"))?;
        self.state.lock().unwrap().docs.extend(docs.iter().map(|doc| (doc.session.id.clone(), doc.clone())));
        self.archive.forget(id);
        for doc in docs {
            self.emit(Event::SessionUpdated { session: doc.session });
        }
        Ok(())
    }

    /// Deletes the session with its subagents' sessions.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        for child in self.children(id) {
            self.delete(&child)?;
        }
        self.stop(id);
        let doc = self.state.lock().unwrap().docs.remove(id).ok_or_else(|| missing(id))?;
        self.store.remove(id);
        // Subagents work in their parent's folder
        if doc.session.no_project && doc.session.parent_id.is_none() {
            let _ = std::fs::remove_dir_all(&doc.session.directory);
        }
        self.emit(Event::SessionDeleted {
            session: doc.session,
        });
        Ok(())
    }

    /// Ends the session's reply, queued messages, background tasks and browser tabs.
    fn stop(&self, id: &str) {
        self.state.lock().unwrap().queue.remove(id);
        self.abort(id);
        self.state.lock().unwrap().jobs.remove(id);
        let _ = std::fs::remove_file(self.store.waits(id));
        crate::tools::close_browser_tabs(id);
    }

    /// The Chrome the session's chat works in.
    pub fn chrome_of(&self, id: &str) -> Option<String> {
        let chat = self.chat_of(id);
        self.state.lock().unwrap().docs.get(&chat).and_then(|d| d.chrome.clone())
    }

    /// The chat a session belongs to: its own id, or the top session above a subagent's.
    pub fn chat_of(&self, id: &str) -> String {
        let state = self.state.lock().unwrap();
        let mut chat = id.to_owned();
        for _ in 0..16 {
            match state.docs.get(&chat).and_then(|d| d.session.parent_id.clone()) {
                Some(parent) => chat = parent,
                None => break,
            }
        }
        chat
    }

    /// The session with its subagents' sessions at any depth, the session first.
    fn tree(&self, id: &str) -> Vec<String> {
        let mut ids = vec![id.to_string()];
        let mut next = 0;
        while next < ids.len() {
            let children = self.children(&ids[next]);
            ids.extend(children);
            next += 1;
        }
        ids
    }

    fn children(&self, id: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .docs
            .values()
            .filter(|d| d.session.parent_id.as_deref() == Some(id))
            .map(|d| d.session.id.clone())
            .collect()
    }

    pub fn abort(&self, id: &str) {
        if let Some(interrupt) = self.state.lock().unwrap().running.get(id) {
            interrupt.abort();
        }
    }

    /// Hands the queued messages to the model now instead of after the step; nothing to do without them.
    pub fn interrupt(&self, id: &str, message: &str) {
        let mut state = self.state.lock().unwrap();
        let queued = state.queue.get(id).is_some_and(|queue| queue.iter().any(|prompt| prompt.message_id == message));
        let asked = state.asked_compaction.contains(id);
        // Compaction rewrites the history the queue goes into, so a turn's own waits for it; one the person asked for is cancelled
        let Some(interrupt) = state.running.get(id).filter(|_| queued && (asked || !state.compacting.contains_key(id))).cloned() else {
            return;
        };
        state.interrupting.insert(id.into(), message.into());
        if asked {
            interrupt.abort();
        } else {
            interrupt.fire();
        }
    }

    /// Stores the user turn and starts the reply; the reply itself arrives as events.
    pub fn prompt(self: &Arc<Self>, prompt: Prompt) -> Result<(), String> {
        let id = prompt.session_id.clone();
        let marked = self.marked(&prompt)?;
        let interrupt = Arc::new(Interrupt::default());
        let (directory, user, parts, jobs) = {
            let mut state = self.state.lock().unwrap();
            if state.running.contains_key(&id) {
                self.enqueue(&mut state, prompt);
                return Ok(());
            }
            let jobs = self.jobs_of(&mut state, &id);
            let doc = state.docs.get_mut(&id).ok_or_else(|| missing(&id))?;
            // A message taken back from the queue keeps its old id, which may sort before the history by now
            let message_id = match doc.messages.last().map(|m| m.info.id()) {
                Some(last) if prompt.message_id.as_str() <= last => id::ascending_after("msg", last),
                _ => prompt.message_id,
            };
            let user = UserMessage {
                id: message_id,
                session_id: id.clone(),
                time: Created { created: id::now() },
                agent: prompt.agent,
                model: prompt.model,
            };
            let mut parts: Vec<Part> = prompt
                .parts
                .into_iter()
                .filter_map(|p| part_of(p, &id, &user.id, &marked))
                .collect();
            self.store.keep(&id, &mut parts);
            let notes = jobs.notices();
            if !notes.is_empty() {
                parts.push(note_part(notes.join("\n"), &id, &user.id));
            }
            // The first message names a session nobody named yet
            if doc.messages.is_empty() && doc.session.title == UNTITLED {
                doc.session.title = named(&title_of(&parts));
            }
            let directory = self.store_user(&mut state, &user, &parts, &interrupt);
            (directory, user, parts, jobs)
        };
        self.begin(directory, user, parts, jobs, interrupt);
        Ok(())
    }

    /// The session's background work; a saved session keeps its waits in a file.
    fn jobs_of(&self, state: &mut State, id: &str) -> Arc<Jobs> {
        let temporary = state.docs.get(id).is_none_or(|d| d.session.temporary);
        let file = self.store.waits(id);
        state
            .jobs
            .entry(id.into())
            .or_insert_with(|| Arc::new(if temporary { Jobs::default() } else { Jobs::kept_in(file) }))
            .clone()
    }

    /// Starts a turn by itself when a background task ended while the session was idle.
    fn wake(self: &Arc<Self>, id: &str) {
        let interrupt = Arc::new(Interrupt::default());
        let (directory, user, parts, jobs) = {
            let mut state = self.state.lock().unwrap();
            let Some(jobs) = state.jobs.get(id).cloned() else {
                return;
            };
            if state.running.contains_key(id) {
                return;
            }
            let Some(doc) = state.docs.get(id) else {
                return;
            };
            // A subagent hears of its tasks when its agent continues it; a wait of its own wakes it
            if let Some(parent) = doc.session.parent_id.clone() {
                if jobs.wait_fired() {
                    drop(state);
                    self.wake_sub(id, &parent);
                }
                return;
            }
            if !jobs.pending() {
                return;
            }
            let Some((model, agent)) = doc.messages.iter().rev().find_map(|m| match &m.info {
                Message::User(u) => Some((u.model.clone(), u.agent.clone())),
                Message::Assistant(_) => None,
            }) else {
                return;
            };
            let last = doc
                .messages
                .last()
                .map_or(String::new(), |m| m.info.id().to_owned());
            let user = UserMessage {
                id: id::ascending_after("msg", &last),
                session_id: id.into(),
                time: Created { created: id::now() },
                agent,
                model,
            };
            let note = format!(
                "(Not a message from the person: this turn started because of the background notice below.)\n{}",
                jobs.notices().join("\n")
            );
            let parts = vec![note_part(note, id, &user.id)];
            let directory = self.store_user(&mut state, &user, &parts, &interrupt);
            (directory, user, parts, jobs)
        };
        self.begin(directory, user, parts, jobs, interrupt);
    }

    /// A subagent whose wait fired goes on by itself; its new report comes to its agent as a background subagent's does.
    fn wake_sub(self: &Arc<Self>, child: &str, parent: &str) {
        let Some(jobs) = self.state.lock().unwrap().jobs.get(parent).cloned() else {
            return;
        };
        let Some(sub) = jobs.sub_of(child) else {
            return;
        };
        let (Ok((_, description)), Some(model)) = (jobs.sub_session(&sub), self.last_model(child)) else {
            return;
        };
        let cancel = CancelToken::new();
        jobs.sub_start(Some(&sub), child, &description, true, &cancel);
        let (this, child) = (self.clone(), child.to_owned());
        tokio::spawn(async move {
            let report = this.subagent_turn(&child, model, String::new(), cancel).await;
            jobs.sub_end(&sub, &report);
        });
    }

    /// Saves the user message and marks the session running; returns its directory.
    fn store_user(
        &self,
        state: &mut State,
        user: &UserMessage,
        parts: &[Part],
        interrupt: &Arc<Interrupt>,
    ) -> String {
        let id = &user.session_id;
        let doc = state.docs.get_mut(id).expect("checked by the caller");
        doc.session.time.updated = user.time.created;
        doc.messages.push(MessageWithParts {
            info: Message::User(user.clone()),
            parts: parts.to_vec(),
        });
        self.store.save(doc);
        state.running.insert(id.clone(), interrupt.clone());
        let session = state.docs[id].session.clone();
        self.emit(Event::SessionUpdated {
            session: session.clone(),
        });
        session.directory
    }

    /// Announces the stored user message and runs the reply.
    fn begin(
        self: &Arc<Self>,
        directory: String,
        user: UserMessage,
        parts: Vec<Part>,
        jobs: Arc<Jobs>,
        interrupt: Arc<Interrupt>,
    ) {
        let this = self.clone();
        tokio::spawn(async move { this.answer(directory, user, parts, jobs, interrupt).await });
    }

    async fn answer(
        self: &Arc<Self>,
        directory: String,
        user: UserMessage,
        mut parts: Vec<Part>,
        jobs: Arc<Jobs>,
        interrupt: Arc<Interrupt>,
    ) {
        let id = user.session_id.clone();
        self.store.link(&id, parts.iter_mut());
        self.emit(Event::MessageUpdated {
            directory: directory.clone(),
            message: Message::User(user.clone()),
        });
        for part in parts {
            self.emit(Event::PartUpdated {
                directory: directory.clone(),
                part,
            });
        }
        self.set_status(&directory, &id, SessionStatus::Busy { compaction: None });
        run::reply(self, &directory, &user, jobs, interrupt).await;
        let queued = {
            let mut state = self.state.lock().unwrap();
            state.running.remove(&id);
            state.delivered.remove(&id);
            state.held.remove(&id);
            self.take_queue(&mut state, &id)
        };
        self.set_status(&directory, &id, SessionStatus::Idle);
        self.send_queue(queued);
    }

    /// Gives a subagent's session the task `text` and waits for its reply; returns its report.
    async fn subagent_turn(
        self: &Arc<Self>,
        id: &str,
        model: ModelRef,
        text: String,
        cancel: CancelToken,
    ) -> String {
        let interrupt = Arc::new(Interrupt::new(cancel));
        let started = {
            let mut state = self.state.lock().unwrap();
            if state.running.contains_key(id) {
                return "(the subagent was already running)".into();
            }
            let jobs = self.jobs_of(&mut state, id);
            let last = state
                .docs
                .get(id)
                .and_then(|d| d.messages.last())
                .map_or(String::new(), |m| m.info.id().to_owned());
            let user = UserMessage {
                id: id::ascending_after("msg", &last),
                session_id: id.into(),
                time: Created { created: id::now() },
                agent: "subagent".into(),
                model,
            };
            // No task: a wait of its own woke it
            let woken = text.is_empty();
            let mut parts = Vec::new();
            if !woken {
                parts.push(Part::Text(TextPart {
                    id: id::ascending("prt"),
                    session_id: id.into(),
                    message_id: user.id.clone(),
                    text,
                    time: None,
                    sig: None,
                    synthetic: false,
                }));
            }
            let mut notes = jobs.notices();
            if woken {
                notes.insert(0, "(Not a message from your agent: this turn started because of the background notice below.)".into());
            }
            if !notes.is_empty() {
                parts.push(note_part(notes.join("\n"), id, &user.id));
            }
            let directory = self.store_user(&mut state, &user, &parts, &interrupt);
            (directory, user, parts, jobs)
        };
        let (directory, user, parts, jobs) = started;
        let user_id = user.id.clone();
        self.answer(directory, user, parts, jobs, interrupt).await;
        subagent::report(&self.messages(id).unwrap_or_default(), &user_id)
    }

    /// Checks background tasks twice a second and wakes idle sessions whose tasks ended.
    fn watch(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let Some(sessions) = this.upgrade() else {
                    return;
                };
                let ids: Vec<String> = sessions
                    .state
                    .lock()
                    .unwrap()
                    .jobs
                    .keys()
                    .cloned()
                    .collect();
                for id in ids {
                    sessions.wake(&id);
                }
            }
        });
    }

    fn add_message(&self, directory: &str, message: MessageWithParts) {
        let info = message.info.clone();
        if let Some(doc) = self.state.lock().unwrap().docs.get_mut(session_of(&info)) {
            doc.messages.push(message);
        }
        self.emit(Event::MessageUpdated {
            directory: directory.into(),
            message: info,
        });
    }

    fn put_part(&self, directory: &str, part: Part) {
        self.with_message(part.session_id(), part.message_id(), |m| {
            match m.parts.iter_mut().find(|p| p.id() == part.id()) {
                Some(old) => *old = part.clone(),
                None => m.parts.push(part.clone()),
            }
        });
        self.emit(Event::PartUpdated {
            directory: directory.into(),
            part,
        });
    }

    fn append(&self, part: &TextPart, text: &str) {
        self.with_message(&part.session_id, &part.message_id, |m| {
            if let Some(p) = m
                .parts
                .iter_mut()
                .find(|p| p.id() == part.id)
                .and_then(Part::text_mut)
            {
                p.text.push_str(text);
            }
        });
    }

    fn finish_message(&self, directory: &str, info: Message) {
        let session = {
            let mut state = self.state.lock().unwrap();
            let Some(doc) = state.docs.get_mut(session_of(&info)) else {
                return;
            };
            if let Some(m) = doc.messages.iter_mut().find(|m| m.info.id() == info.id()) {
                m.info = info.clone();
            }
            doc.session.time.updated = id::now();
            self.store.save(doc);
            doc.session.clone()
        };
        self.emit(Event::MessageUpdated {
            directory: directory.into(),
            message: info,
        });
        self.emit(Event::SessionUpdated { session });
    }

    fn with_message(
        &self,
        session: &str,
        message: &str,
        change: impl FnOnce(&mut MessageWithParts),
    ) {
        let mut state = self.state.lock().unwrap();
        if let Some(m) = state
            .docs
            .get_mut(session)
            .and_then(|d| d.messages.iter_mut().find(|m| m.info.id() == message))
        {
            change(m);
        }
    }

    fn edit(&self, id: &str, change: impl FnOnce(&mut Session)) -> Result<(), String> {
        let session = {
            let mut state = self.state.lock().unwrap();
            let doc = state.docs.get_mut(id).ok_or_else(|| missing(id))?;
            change(&mut doc.session);
            self.store.save(doc);
            doc.session.clone()
        };
        self.emit(Event::SessionUpdated { session });
        Ok(())
    }

    pub(super) fn set_status(&self, directory: &str, id: &str, status: SessionStatus) {
        self.emit(Event::SessionStatus {
            directory: directory.into(),
            session_id: id.into(),
            status,
        });
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// A stored part of the person's message; a command mark is never stored.
fn part_of(part: PromptPart, session_id: &str, message_id: &str, marked: &[marks::Marked]) -> Option<Part> {
    if let PromptPart::Skill { id, name, at } = &part {
        let skill = marked.iter().find(|m| m.name() == name)?;
        return Some(Part::Skill(skill.part(id.clone(), session_id, message_id, *at)));
    }
    let (session_id, message_id) = (session_id.to_owned(), message_id.to_owned());
    Some(match part {
        PromptPart::Text { id, text } => Part::Text(TextPart {
            id,
            session_id,
            message_id,
            text,
            time: None,
            sig: None,
            synthetic: false,
        }),
        PromptPart::File {
            id,
            mime,
            filename,
            url,
        } => Part::File(FilePart {
            id,
            session_id,
            message_id,
            mime,
            filename,
            url,
        }),
        PromptPart::Command { id, name } => Part::Command(CommandPart { id, session_id, message_id, name }),
        PromptPart::Skill { .. } => return None,
    })
}

/// Server notes for the model that go with the person's message.
fn note_part(text: String, session_id: &str, message_id: &str) -> Part {
    Part::Text(TextPart {
        id: id::ascending("prt"),
        session_id: session_id.into(),
        message_id: message_id.into(),
        text,
        time: None,
        sig: None,
        synthetic: true,
    })
}

fn session_of(message: &Message) -> &str {
    match message {
        Message::User(m) => &m.session_id,
        Message::Assistant(m) => &m.session_id,
    }
}

/// Sessions with background waits saved by the last run of the server: their waits go on.
fn waiting(store: &Store, docs: &HashMap<String, Doc>) -> HashMap<String, Arc<Jobs>> {
    docs.iter()
        .filter(|(_, doc)| !doc.session.temporary)
        .map(|(id, _)| (id, store.waits(id)))
        .filter(|(_, file)| file.exists())
        .map(|(id, file)| (id.clone(), Arc::new(Jobs::kept_in(file))))
        .collect()
}

fn missing(id: &str) -> String {
    format!("No session {id}")
}

/// The name of a session nobody named, in every language.
const UNTITLED: &str = "Untitled";

/// A session's name is never empty.
fn named(title: &str) -> String {
    match title.trim() {
        "" => UNTITLED.into(),
        title => title.into(),
    }
}

/// First line of the first message, shortened.
fn title_of(parts: &[Part]) -> String {
    let text = parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) if !t.synthetic => Some(t.text.as_str()),
            _ => None,
        })
        .find(|t| !t.trim().is_empty())
        .unwrap_or("");
    let line = text.trim().lines().next().unwrap_or("").trim();
    match line.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line.to_owned(),
    }
}

/// Makes a session's own folder; its path as the session's directory.
fn own_folder(path: &std::path::Path) -> Result<String, String> {
    std::fs::create_dir_all(path).map_err(|e| format!("creating {} failed: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}
