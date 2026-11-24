//! Messages the person sends while a session answers: they wait here, reach the model at its next step,
//! or start the next turn when the answer ends first.
use std::sync::Arc;

use nabu_protocol::{Created, Event, Message, MessageWithParts, Prompt, UserMessage};

use super::{Sessions, State, id, note_part, part_of};

impl Sessions {
    /// Queued messages of a session, oldest first.
    pub fn queue(&self, session: &str) -> Vec<Prompt> {
        self.state.lock().unwrap().queue.get(session).cloned().unwrap_or_default()
    }

    /// Takes a prompt back out of the queue; fails when it already went to the model.
    pub fn dequeue(&self, session: &str, message: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let queue = state.queue.get_mut(session).ok_or("The message was already sent")?;
        let count = queue.len();
        queue.retain(|prompt| prompt.message_id != message);
        if queue.len() == count {
            return Err("The message was already sent".into());
        }
        self.announce_queue(&state, session);
        Ok(())
    }

    /// Queued prompts keep the order they were written in, so one taken back and sent again returns to its place.
    pub(super) fn enqueue(&self, state: &mut State, prompt: Prompt) {
        let session = prompt.session_id.clone();
        let queue = state.queue.entry(session.clone()).or_default();
        let at = queue.partition_point(|queued| queued.message_id < prompt.message_id);
        queue.insert(at, prompt);
        self.announce_queue(state, &session);
    }

    /// The whole queue, oldest first, with fresh ids after the session's last message;
    /// after an interruption only its message, the rest waits for the next step.
    pub(super) fn take_queue(&self, state: &mut State, session: &str) -> Vec<Prompt> {
        let queue = state.queue.remove(session).unwrap_or_default();
        let ahead = state.interrupting.remove(session);
        let (queued, waiting): (Vec<_>, Vec<_>) = match ahead.filter(|id| queue.iter().any(|prompt| &prompt.message_id == id)) {
            Some(id) => queue.into_iter().partition(|prompt| prompt.message_id == id),
            None => (queue, Vec::new()),
        };
        if !waiting.is_empty() {
            state.queue.insert(session.into(), waiting);
            state.held.insert(session.into());
        }
        if queued.is_empty() {
            return queued;
        }
        self.announce_queue(state, session);
        let mut last = state.docs.get(session).and_then(|doc| doc.messages.last()).map_or(String::new(), |m| m.info.id().to_owned());
        queued
            .into_iter()
            .map(|mut prompt| {
                last = id::ascending_after("msg", &last);
                prompt.message_id = last.clone();
                prompt
            })
            .collect()
    }

    /// Stores the queue as the person's messages between two steps of the running turn; true when there was one.
    /// `note` goes to the model before the first of them.
    pub(super) fn deliver_queue(&self, session: &str, note: Option<&str>) -> bool {
        let queued = {
            let mut state = self.state.lock().unwrap();
            if state.held.remove(session) {
                return false;
            }
            self.take_queue(&mut state, session)
        };
        let notes = std::iter::once(note).chain(std::iter::repeat(None));
        let Some(last) = queued.into_iter().zip(notes).filter_map(|(prompt, note)| self.store_queued(prompt, note)).last() else {
            return false;
        };
        self.state.lock().unwrap().delivered.insert(session.into(), last);
        true
    }

    /// Sends what waited for the ended turn: all but the last are stored as they are, the last starts the next turn.
    pub(super) fn send_queue(self: &Arc<Self>, mut queued: Vec<Prompt>) {
        let Some(last) = queued.pop() else { return };
        for prompt in queued {
            self.store_queued(prompt, None);
        }
        // Fails only when the session is gone
        let _ = self.prompt(last);
    }

    // None when the session is gone
    fn store_queued(&self, prompt: Prompt, note: Option<&str>) -> Option<String> {
        let session = prompt.session_id.clone();
        // Checked when queued; a skill gone since then just drops its mark
        let marked = self.marked(&prompt).unwrap_or_default();
        let user = UserMessage {
            id: prompt.message_id.clone(),
            session_id: session.clone(),
            time: Created { created: id::now() },
            agent: prompt.agent,
            model: prompt.model,
        };
        let note = note.map(|text| note_part(text.into(), &session, &user.id));
        let mut parts: Vec<_> = note.into_iter().chain(prompt.parts.into_iter().filter_map(|part| part_of(part, &session, &user.id, &marked))).collect();
        self.store.keep(&session, &mut parts);
        let mut state = self.state.lock().unwrap();
        let doc = state.docs.get_mut(&session)?;
        doc.messages.push(MessageWithParts { info: Message::User(user.clone()), parts: parts.clone() });
        self.store.save(doc);
        let directory = doc.session.directory.clone();
        drop(state);
        self.store.link(&session, parts.iter_mut());
        self.emit(Event::MessageUpdated { directory: directory.clone(), message: Message::User(user.clone()) });
        for part in parts {
            self.emit(Event::PartUpdated { directory: directory.clone(), part });
        }
        Some(user.id)
    }

    /// The person's message delivered since the turn last asked, which the next step answers under.
    pub(super) fn take_delivered(&self, session: &str) -> Option<String> {
        self.state.lock().unwrap().delivered.remove(session)
    }

    fn announce_queue(&self, state: &State, session: &str) {
        let Some(doc) = state.docs.get(session) else { return };
        self.emit(Event::QueueUpdated {
            directory: doc.session.directory.clone(),
            session_id: session.into(),
            queue: state.queue.get(session).cloned().unwrap_or_default(),
        });
    }
}
