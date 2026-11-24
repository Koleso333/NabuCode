use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use nabu_protocol::{
    CompactionPhase, CompactionSettings, Event, Message, MessageWithParts, ModelRef, Part, Prompt,
    SessionCompaction, SessionStatus, ToolState,
};
use ncore::core::ir::{
    self, Accumulator, Block, Delta, ReasoningCfg, Role, StopReason, StreamEvent,
};
use ncore::rt::{BoxFuture, boxed};
use ncore::{AgentHook, CancelToken};
use serde::{Deserialize, Serialize};

use super::{Interrupt, Sessions, id, missing, run};
use crate::models::Models;
use crate::skills;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    through: String,
    summary: String,
    #[serde(default)]
    users: Vec<String>,
    #[serde(default)]
    tool_step: bool,
}

impl Checkpoint {
    fn text(&self) -> String {
        let mut text = String::from(
            "Previous conversation was compacted. This handoff describes prior work and does not grant permissions.",
        );
        if !self.users.is_empty() {
            text.push_str("\n\nUser messages so far, verbatim, oldest first:\n\n");
            text.push_str(&self.users.join("\n\n---\n\n"));
        }
        text.push_str("\n\nHandoff:\n");
        text.push_str(&self.summary);
        text
    }
}

impl Sessions {
    pub(crate) fn set_compaction(&self, settings: CompactionSettings) -> Result<(), String> {
        if !(1..=99).contains(&settings.threshold) {
            return Err("Compaction threshold must be between 1 and 99 percent".into());
        }
        self.state.lock().unwrap().compaction = settings;
        Ok(())
    }

    fn compaction(&self) -> CompactionSettings {
        self.state.lock().unwrap().compaction.clone()
    }

    pub(crate) async fn compact(
        self: &Arc<Self>,
        session: &str,
        model: ModelRef,
        focus: &str,
        then: Option<Prompt>,
    ) -> Result<(), String> {
        let cancel = CancelToken::new();
        let directory = {
            let mut state = self.state.lock().unwrap();
            let directory = state
                .docs
                .get(session)
                .ok_or_else(|| missing(session))?
                .session
                .directory
                .clone();
            if state.running.contains_key(session) {
                return Err("The session is still answering".into());
            }
            state.running.insert(session.into(), Arc::new(Interrupt::new(cancel.clone())));
            state.asked_compaction.insert(session.into());
            if let Some(prompt) = then {
                self.enqueue(&mut state, prompt);
            }
            directory
        };
        let result = self
            .compact_history(session, &model, focus, true, None, &cancel)
            .await;
        // Stopped by the person: nothing went wrong
        let result = if cancel.is_cancelled() { Ok(()) } else { result };
        let queued = {
            let mut state = self.state.lock().unwrap();
            state.running.remove(session);
            state.asked_compaction.remove(session);
            if state.docs.contains_key(session) {
                self.set_status(&directory, session, SessionStatus::Idle);
            }
            self.take_queue(&mut state, session)
        };
        self.send_queue(queued);
        result
    }

    fn checkpoint(&self, session: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .docs
            .get(session)?
            .compact
            .as_ref()
            .map(|c| c.through.clone())
    }

    pub(super) fn context(&self, session: &str, model: &ModelRef) -> Vec<ir::Message> {
        let state = self.state.lock().unwrap();
        let Some(doc) = state.docs.get(session) else {
            return Vec::new();
        };
        context_messages(&doc.messages, doc.compact.as_ref(), model, &self.attachments_for(session, model))
    }

    pub(super) fn attachments_for(&self, session: &str, model: &ModelRef) -> run::Attachments {
        run::Attachments { dir: self.store.attachments(session), images: self.models.takes_images(model) }
    }

    async fn compact_history(
        &self,
        session: &str,
        chat: &ModelRef,
        focus: &str,
        keep_last: bool,
        auto_before: Option<u64>,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        let settings = self.compaction();
        let model = if settings.custom_model {
            settings
                .model
                .as_ref()
                .ok_or("Choose a model for context compaction")?
        } else {
            chat
        };
        let (source, directory) = {
            let state = self.state.lock().unwrap();
            let doc = state.docs.get(session).ok_or_else(|| missing(session))?;
            let source = source(&doc.messages, doc.compact.as_ref(), keep_last)
                .ok_or("No earlier messages to compact")?;
            (source, doc.session.directory.clone())
        };
        let model_name = self.models.name(model);
        let route = format!("{session}:compact");
        let progress = CompactionProgress::new(self, session, &directory, auto_before.is_some());
        let summary = summarize(
            &self.models,
            &route,
            model,
            &source.text,
            focus,
            cancel,
            &progress,
        )
        .await?;
        let checkpoint = Checkpoint {
            through: source.through,
            summary,
            users: source.users,
            tool_step: source.tool_step,
        };
        if focus.is_empty() && checkpoint.text().len() >= source.text.len() {
            return Err("The summary did not make the context smaller".into());
        }
        let mut state = self.state.lock().unwrap();
        let doc = state
            .docs
            .get_mut(session)
            .ok_or_else(|| missing(session))?;
        if cancel.is_cancelled() || doc.messages.last().map(|m| m.info.id()) != Some(&source.last) {
            return Err("Compaction cancelled".into());
        }
        let mut updated = doc.clone();
        let attachments = self.attachments_for(session, chat);
        let tokens_before = auto_before.unwrap_or_else(|| {
            estimate_messages(&context_messages(&doc.messages, doc.compact.as_ref(), chat, &attachments))
        });
        let tokens_after =
            estimate_messages(&context_messages(&doc.messages, Some(&checkpoint), chat, &attachments));
        updated.session.compactions.push(SessionCompaction {
            id: id::ascending("cmp"),
            after_message_id: source.last,
            time: id::now(),
            automatic: auto_before.is_some(),
            model: model.clone(),
            model_name,
            tokens_before,
            tokens_after,
        });
        updated.compact = Some(checkpoint);
        self.store
            .save_checked(&mut updated)
            .map_err(|e| format!("Could not save compacted context: {e}"))?;
        doc.compact = updated.compact;
        doc.session = updated.session;
        self.emit(Event::SessionUpdated {
            session: doc.session.clone(),
        });
        Ok(())
    }
}

struct CompactionProgress<'a> {
    sessions: &'a Sessions,
    session: &'a str,
    directory: &'a str,
    automatic: bool,
    started: AtomicBool,
}

impl<'a> CompactionProgress<'a> {
    fn new(sessions: &'a Sessions, session: &'a str, directory: &'a str, automatic: bool) -> Self {
        let progress = Self {
            sessions,
            session,
            directory,
            automatic,
            started: AtomicBool::new(false),
        };
        progress.set(CompactionPhase::Waiting);
        progress
    }

    fn set(&self, phase: CompactionPhase) {
        let mut state = self.sessions.state.lock().unwrap();
        if !state.docs.contains_key(self.session) {
            return;
        }
        state.compacting.insert(self.session.into(), phase);
        self.sessions.set_status(
            self.directory,
            self.session,
            SessionStatus::Busy {
                compaction: Some(phase),
            },
        );
    }

    fn streaming(&self) {
        if !self.started.swap(true, Ordering::Relaxed) {
            self.set(CompactionPhase::Streaming);
        }
    }
}

impl Drop for CompactionProgress<'_> {
    fn drop(&mut self) {
        let mut state = self.sessions.state.lock().unwrap();
        state.compacting.remove(self.session);
        if self.automatic && state.docs.contains_key(self.session) {
            self.sessions.set_status(
                self.directory,
                self.session,
                SessionStatus::Busy { compaction: None },
            );
        }
    }
}

fn context_messages(
    history: &[MessageWithParts],
    checkpoint: Option<&Checkpoint>,
    model: &ModelRef,
    attachments: &run::Attachments,
) -> Vec<ir::Message> {
    let start = after(history, checkpoint);
    let mut messages = Vec::new();
    if let Some(checkpoint) = checkpoint.filter(|_| start > 0) {
        let mut text = checkpoint.text();
        if let Some(skills) = skills_before(&history[..start]) {
            text.push_str("\n\n");
            text.push_str(&skills);
        }
        run::push(&mut messages, Role::User, vec![Block::text(text)]);
        if checkpoint.tool_step {
            run::append_history(&mut messages, &history[start - 1..start], model, attachments);
            shorten_tool_results(&mut messages);
        }
    }
    run::append_history(&mut messages, &history[start..], model, attachments);
    messages
}

/// User messages before a checkpoint whose skills come back in full after it.
const RECENT_SKILL_TURNS: usize = 3;

/// Skills used before a checkpoint: the ones of its last few user messages in full, older ones by name and folder.
fn skills_before(history: &[MessageWithParts]) -> Option<String> {
    let users = history.iter().enumerate().filter(|(_, m)| matches!(m.info, Message::User(_)));
    let recent_from = users.rev().nth(RECENT_SKILL_TURNS - 1).map_or(0, |(i, _)| i);
    // Name, the text the model got, and whether it was used lately; a later use replaces an earlier one
    let mut used: Vec<(String, String, bool)> = Vec::new();
    for (i, entry) in history.iter().enumerate() {
        for part in &entry.parts {
            let found = match part {
                Part::Skill(s) => Some((s.name.clone(), skills::content(&s.name, &s.path, &s.body))),
                Part::Tool(t) if t.tool == "skill" => match &t.state {
                    ToolState::Completed { input, output, .. } => {
                        input.get("name").and_then(|n| n.as_str()).map(|n| (n.to_owned(), output.clone()))
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some((name, content)) = found {
                used.retain(|(n, _, _)| *n != name);
                used.push((name, content, i >= recent_from));
            }
        }
    }
    if used.is_empty() {
        return None;
    }
    let older: Vec<String> = used
        .iter()
        .filter(|(_, _, recent)| !recent)
        .map(|(name, content, _)| match folder_of(content) {
            Some(dir) => format!("- {name} ({dir})"),
            None => format!("- {name}"),
        })
        .collect();
    let mut text = String::new();
    if !older.is_empty() {
        text.push_str("Skills used earlier in this session; compaction dropped their texts, load one again with `skill` when the work needs it:\n");
        text.push_str(&older.join("\n"));
    }
    for (_, content, _) in used.iter().filter(|(_, _, recent)| *recent) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(content);
    }
    Some(text)
}

/// The folder a skill's text names in its header.
fn folder_of(content: &str) -> Option<&str> {
    content.split_once(" dir=\"")?.1.split_once('"').map(|(dir, _)| dir)
}

struct Source {
    text: String,
    users: Vec<String>,
    tool_step: bool,
    through: String,
    last: String,
}

fn after(history: &[MessageWithParts], checkpoint: Option<&Checkpoint>) -> usize {
    checkpoint
        .and_then(|c| history.iter().position(|m| m.info.id() == c.through))
        .map_or(0, |i| i + 1)
}

fn source(
    history: &[MessageWithParts],
    checkpoint: Option<&Checkpoint>,
    keep_last: bool,
) -> Option<Source> {
    let start = after(history, checkpoint);
    let last = history.last()?;
    let keep_last = keep_last || matches!(last.info, Message::User(_));
    let split = history.len() - usize::from(keep_last);
    let older = history.get(start..split)?.last()?;
    let previous = checkpoint.filter(|_| start > 0);
    let mut text = previous.map_or_else(String::new, Checkpoint::text);
    for entry in &history[start..split] {
        let role = if matches!(entry.info, Message::User(_)) {
            "User"
        } else {
            "Assistant"
        };
        text.push_str(&format!("\n\n{role}:\n"));
        for part in &entry.parts {
            match part {
                Part::Text(t) => text.push_str(&format!("{}\n", t.text)),
                Part::Skill(s) => text.push_str(&format!("[Skill marked: {}]\n", s.name)),
                Part::Tool(t) if t.tool == "skill" => text.push_str(&format!("Tool skill {}\n", t.args)),
                Part::File(f) => text.push_str(&format!(
                    "[Attachment: {} ({})]\n",
                    f.filename.as_deref().unwrap_or("unnamed"),
                    f.mime
                )),
                Part::Tool(t) => {
                    let output = match &t.state {
                        ToolState::Completed { output, .. } => output.as_str(),
                        ToolState::Error { error, .. } => error.as_str(),
                        _ => "[Tool did not complete]",
                    };
                    text.push_str(&format!("Tool {} {}\nResult:\n{output}\n", t.tool, t.args));
                }
                _ => {}
            }
        }
    }
    let mut users = previous.map_or_else(Vec::new, |c| c.users.clone());
    users.extend(history[start..split].iter().filter_map(user_text));
    keep_newest(&mut users);
    Some(Source {
        text,
        users,
        tool_step: !keep_last,
        through: older.info.id().into(),
        last: last.info.id().into(),
    })
}

fn user_text(entry: &MessageWithParts) -> Option<String> {
    let Message::User(_) = entry.info else {
        return None;
    };
    let text = entry
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) if !t.synthetic => Some(t.text.clone()),
            Part::Skill(s) => Some(run::mark(&s.name)),
            _ => None,
        })
        .collect::<String>();
    let text = text.trim();
    let end = text.floor_char_boundary(8_000);
    match end {
        0 => None,
        _ if end < text.len() => Some(format!("{}\n[message truncated]", &text[..end])),
        _ => Some(text.to_owned()),
    }
}

// The oldest messages go first when the verbatim block outgrows its budget.
fn keep_newest(users: &mut Vec<String>) {
    const OMITTED: &str = "[Earlier user messages omitted]";
    let omitted = users.first().is_some_and(|m| m == OMITTED);
    if omitted {
        users.remove(0);
    }
    let mut total = 0;
    let keep = users
        .iter()
        .rev()
        .take_while(|m| {
            total += m.len();
            total <= 30_000
        })
        .count()
        .max(1);
    let dropped = users.len() - keep;
    users.drain(..dropped);
    if omitted || dropped > 0 {
        users.insert(0, OMITTED.into());
    }
}

async fn summarize(
    models: &Models,
    session: &str,
    model: &ModelRef,
    history: &str,
    focus: &str,
    cancel: &CancelToken,
    progress: &CompactionProgress<'_>,
) -> Result<String, String> {
    let (context, output) = models
        .limits(model)
        .ok_or("The compaction model is unavailable")?;
    let context = if context > 0 { context } else { 16_000 };
    let output = if output > 0 { output } else { 16_384 };
    let cap = output.min(16_384).min(context / 3);
    let limit = context.saturating_sub(cap + 2_048).min(100_000) as usize * 3;
    if limit < 1_024 || cap < 512 {
        return Err("The compaction model's context window is too small".into());
    }
    // Each chunk is summarized on top of the handoff for everything before it.
    let chunk_limit = limit.saturating_sub(cap as usize * 3);
    let mut note = String::new();
    for chunk in split_text(history, chunk_limit) {
        note = summarize_chunk(
            models, session, model, &note, chunk, focus, cap, cancel, progress,
        )
        .await?;
    }
    Ok(note)
}

fn split_text(mut text: &str, limit: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    while text.len() > limit {
        let end = text.floor_char_boundary(limit);
        let end = text[..end]
            .rfind('\n')
            .filter(|i| *i >= limit / 2)
            .map_or(end, |i| i + 1);
        parts.push(&text[..end]);
        text = &text[end..];
    }
    parts.push(text);
    parts
}

async fn summarize_chunk(
    models: &Models,
    session: &str,
    model: &ModelRef,
    earlier: &str,
    text: &str,
    focus: &str,
    cap: u64,
    cancel: &CancelToken,
    progress: &CompactionProgress<'_>,
) -> Result<String, String> {
    if cancel.is_cancelled() {
        return Err("Compaction cancelled".into());
    }
    // TEMP bench: prompt override.
    let instruction = std::env::var("NABU_COMPACT_PROMPT")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| include_str!("compact.md").into());
    let earlier = match earlier {
        "" => String::new(),
        _ => format!("<earlier_handoff>\n{earlier}\n</earlier_handoff>\n\n"),
    };
    // Weak models answer the last user message unless the task is restated after the transcript.
    let input = format!(
        "{earlier}<transcript>\n{text}\n</transcript>\n\nWrite the handoff for this transcript now, as the system prompt says. Do not answer the user and do not continue the work."
    );
    let focus = &focus[..focus.floor_char_boundary(3_000)];
    let mut request = ir::Request {
        model: model.model_id.clone(),
        system: vec![Block::text(&instruction)],
        messages: vec![ir::Message::new(
            Role::User,
            vec![Block::text(format!("User focus: {focus}\n\n{input}"))],
        )],
        max_tokens: Some(cap),
        reasoning: model.variant.as_ref().map(|effort| ReasoningCfg {
            effort: Some(effort.clone()),
            ..Default::default()
        }),
        stream: true,
        ..Default::default()
    };
    let route = models.route(session, model, &mut request)?;
    let mut stream = models
        .core
        .generate(&route.endpoint, &request, &route.options, cancel)
        .await
        .map_err(|e| e.to_string())?;
    let mut acc = Accumulator::default();
    let mut ended = false;
    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| e.to_string())?;
        if matches!(&event, StreamEvent::Delta { delta: Delta::Thinking(text) | Delta::Text(text), .. } if !text.is_empty())
        {
            progress.streaming();
        }
        ended |= matches!(event, StreamEvent::End { .. });
        acc.push(&event);
    }
    let response = acc.finish();
    if !ended
        || !matches!(
            response.stop,
            None | Some(StopReason::EndTurn | StopReason::StopSequence(_))
        )
    {
        return Err("The compaction model did not finish its summary".into());
    }
    let summary = response
        .content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // TEMP bench: raw output with analysis.
    if let Ok(dir) = std::env::var("NABU_COMPACT_RAW") {
        let _ = std::fs::write(
            format!("{dir}/{}-{}.raw.md", session.replace(':', "_"), id::now()),
            &summary,
        );
    }
    let summary = handoff(&summary);
    if summary.is_empty() {
        return Err("The compaction model returned no summary".into());
    }
    Ok(summary.to_owned())
}

// The model drafts in <analysis> first; only <summary> is kept.
fn handoff(text: &str) -> &str {
    if let Some(start) = text.find("<summary>") {
        let body = &text[start + "<summary>".len()..];
        return body[..body.rfind("</summary>").unwrap_or(body.len())].trim();
    }
    match text.find("</analysis>") {
        Some(end) => text[end + "</analysis>".len()..].trim(),
        None if text.trim_start().starts_with("<analysis>") => "",
        None => text.trim(),
    }
}

pub(super) struct BeforeStep {
    sessions: Arc<Sessions>,
    session: String,
    model: ModelRef,
    cancel: CancelToken,
    initial: Option<String>,
    failed: AtomicBool,
    /// A queued message entered the stored history during this run.
    queue_delivered: AtomicBool,
}

impl BeforeStep {
    pub(super) fn new(
        sessions: Arc<Sessions>,
        session: String,
        model: ModelRef,
        cancel: CancelToken,
    ) -> Self {
        let initial = sessions.checkpoint(&session);
        Self {
            sessions,
            session,
            model,
            cancel,
            initial,
            failed: AtomicBool::new(false),
            queue_delivered: AtomicBool::new(false),
        }
    }
}

impl AgentHook for BeforeStep {
    fn before_step<'a>(
        &'a self,
        _step: u32,
        request: &'a mut ir::Request,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if self.sessions.deliver_queue(&self.session, None) {
                self.queue_delivered.store(true, Ordering::Relaxed);
            }
            if self.queue_delivered.load(Ordering::Relaxed) || self.sessions.checkpoint(&self.session) != self.initial {
                // Let the stream deliver queued tool results before reading the stored history.
                tokio::task::yield_now().await;
                request.messages = self.sessions.context(&self.session, &self.model);
            }
            let settings = self.sessions.compaction();
            if !settings.auto || self.failed.load(Ordering::Relaxed) {
                return Ok(());
            }
            let Some((context, _)) = self
                .sessions
                .models
                .limits(&self.model)
                .filter(|(context, _)| *context > 0)
            else {
                return Ok(());
            };
            let threshold = context.saturating_mul(u64::from(settings.threshold)) / 100;
            if estimate(request) < threshold {
                return Ok(());
            }
            clear_old_results(&mut request.messages);
            if estimate(request) < threshold {
                return Ok(());
            }
            let keep_last = request.messages.last().is_none_or(|message| {
                message
                    .content
                    .iter()
                    .filter(|block| matches!(block, Block::ToolResult(_)))
                    .map(block_tokens)
                    .sum::<u64>()
                    < threshold / 2
            });
            tokio::task::yield_now().await;
            match self
                .sessions
                .compact_history(
                    &self.session,
                    &self.model,
                    "",
                    keep_last,
                    Some(estimate_messages(&request.messages)),
                    &self.cancel,
                )
                .await
            {
                Ok(()) => request.messages = self.sessions.context(&self.session, &self.model),
                Err(error) => {
                    self.failed.store(true, Ordering::Relaxed);
                    if !self.cancel.is_cancelled() {
                        eprintln!("[sessions] auto compact {}: {error}", self.session);
                    }
                }
            }
            Ok(())
        })
    }
}

fn estimate(request: &ir::Request) -> u64 {
    request.system.iter().map(block_tokens).sum::<u64>()
        + estimate_messages(&request.messages)
        + tool_tokens(&request.tools)
}

pub(super) fn tool_tokens(tools: &[ir::ToolDef]) -> u64 {
    tools.iter().map(|t| serde_json::to_string(t).map_or(0, |s| (s.len() as u64).div_ceil(3))).sum()
}

pub(super) fn estimate_messages(messages: &[ir::Message]) -> u64 {
    messages
        .iter()
        .map(|m| 8 + m.content.iter().map(block_tokens).sum::<u64>())
        .sum()
}

pub(super) fn block_tokens(block: &Block) -> u64 {
    match block {
        Block::Text { text, .. } => (text.len() as u64).div_ceil(3),
        Block::Thinking(t) => (t.text.len() as u64).div_ceil(3),
        Block::Media { .. } => 2_000,
        Block::ToolCall(t) => ((t.name.len() + t.args.len() + t.id.len()) as u64).div_ceil(3),
        Block::ToolResult(r) => r.content.iter().map(block_tokens).sum(),
        _ => 16,
    }
}

fn shorten_tool_results(messages: &mut [ir::Message]) {
    for block in messages.iter_mut().flat_map(|m| &mut m.content) {
        let Block::ToolResult(result) = block else {
            continue;
        };
        for block in &mut result.content {
            let Block::Text { text, .. } = block else {
                continue;
            };
            if text.len() <= 3_000 {
                continue;
            }
            let head = &text[..text.floor_char_boundary(1_500)];
            let tail = &text[text.ceil_char_boundary(text.len() - 1_500)..];
            *text = format!(
                "{head}\n[Middle omitted; the full tool result remains in the transcript.]\n{tail}"
            );
        }
    }
}

fn clear_old_results(messages: &mut [ir::Message]) {
    let mut keep = 5;
    for block in messages
        .iter_mut()
        .rev()
        .flat_map(|m| m.content.iter_mut().rev())
    {
        let Block::ToolResult(result) = block else {
            continue;
        };
        if keep > 0 {
            keep -= 1;
            continue;
        }
        if result.is_error
            || !matches!(
                result.name.as_deref(),
                Some("read" | "grep" | "glob" | "list" | "fetch" | "web")
            )
        {
            continue;
        }
        if result
            .content
            .iter()
            .map(|b| match b {
                Block::Text { text, .. } => text.len(),
                _ => 0,
            })
            .sum::<usize>()
            > 256
        {
            result.content = vec![Block::text(
                "[Older tool output omitted from the working context.]",
            )];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nabu_protocol::{Created, TextPart, UserMessage};

    fn entry(id: &str, text: &str) -> MessageWithParts {
        MessageWithParts {
            info: Message::User(UserMessage {
                id: id.into(),
                session_id: "session".into(),
                time: Created { created: 1 },
                agent: "build".into(),
                model: ModelRef {
                    provider_id: "provider".into(),
                    model_id: "model".into(),
                    variant: None,
                    fast: false,
                },
            }),
            parts: vec![Part::Text(TextPart {
                id: format!("part-{id}"),
                session_id: "session".into(),
                message_id: id.into(),
                text: text.into(),
                time: None,
                sig: None,
                synthetic: false,
            })],
        }
    }

    #[test]
    fn skills_come_back_after_compaction_in_full_only_when_recent() {
        let marked = |id: &str, name: &str| {
            let mut entry = entry(id, "go");
            entry.parts.push(Part::Skill(nabu_protocol::SkillPart {
                id: format!("skill-{id}"),
                session_id: "session".into(),
                message_id: id.into(),
                name: name.into(),
                at: false,
                path: format!("/skills/{name}"),
                body: format!("{name} steps"),
            }));
            entry
        };
        let history = [marked("1", "old"), entry("2", "a"), entry("3", "b"), marked("4", "new"), entry("5", "c")];
        let text = skills_before(&history).unwrap();
        assert!(text.contains("- old (/skills/old)") && !text.contains("old steps"), "{text}");
        assert!(text.contains("new steps") && !text.contains("- new"), "{text}");
        assert_eq!(skills_before(&history[1..3]), None);
    }

    #[test]
    fn responses_includes_an_empty_reply_error_between_prompts() {
        use ncore::core::{Format, codec};
        use serde_json::{Value, json};

        let failed = serde_json::from_value(json!({
            "info": {
                "role": "assistant",
                "id": "failed",
                "sessionID": "session",
                "parentID": "first",
                "providerID": "agentrouter",
                "modelID": "model",
                "agent": "build",
                "time": { "created": 2, "completed": 3 },
                "error": { "name": "APIError", "data": { "message": "Quota exhausted" } },
                "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "cost": 0
            },
            "parts": []
        }))
        .unwrap();
        let history = [entry("first", "ку"), failed, entry("second", "ку")];
        let model = ModelRef {
            provider_id: "agentrouter".into(),
            model_id: "model".into(),
            variant: None,
            fast: false,
        };
        let request = ir::Request {
            model: model.model_id.clone(),
            messages: context_messages(&history, None, &model, &run::Attachments { dir: Default::default(), images: true }),
            ..ir::Request::default()
        };
        let encoded =
            codec::encode_request(Format::OpenAiResponses, &request, &Default::default()).unwrap();
        let body: Value = serde_json::from_slice(&encoded.body).unwrap();
        assert_eq!(
            body["input"],
            json!([
                { "type": "message", "role": "user", "content": "ку" },
                { "type": "message", "role": "assistant", "content": "[NabuCode] No response was generated. Error: Quota exhausted" },
                { "type": "message", "role": "user", "content": "ку" }
            ])
        );
    }

    #[test]
    fn repeated_compaction_uses_the_checkpoint_and_keeps_the_newest_message() {
        let history = [
            entry("old", "old transcript"),
            entry("middle", "keep this constraint exactly"),
            entry("new", "latest turn"),
        ];
        let checkpoint = Checkpoint {
            through: "old".into(),
            summary: "previous handoff".into(),
            users: vec!["old transcript".into()],
            tool_step: false,
        };
        let source = source(&history, Some(&checkpoint), true).unwrap();
        assert_eq!(source.through, "middle");
        assert_eq!(source.last, "new");
        assert!(source.text.contains("previous handoff"));
        assert!(!source.text.contains("latest turn"));
        assert_eq!(
            source.users,
            ["old transcript", "keep this constraint exactly"]
        );
    }

    #[test]
    fn verbatim_user_messages_drop_the_oldest_over_budget() {
        let mut users = vec!["a".repeat(20_000), "b".repeat(20_000), "c".into()];
        keep_newest(&mut users);
        assert_eq!(users.len(), 3);
        assert_eq!(users[0], "[Earlier user messages omitted]");
        assert!(users[1].starts_with('b'));
        keep_newest(&mut users);
        assert_eq!(users.len(), 3);
    }

    #[test]
    fn handoff_keeps_only_the_summary() {
        assert_eq!(
            handoff("<analysis>draft</analysis>\n<summary>\nfacts\n</summary>"),
            "facts"
        );
        assert_eq!(handoff("<analysis>draft</analysis>\nfacts"), "facts");
        assert_eq!(handoff("<analysis>cut off"), "");
        assert_eq!(handoff("plain facts"), "plain facts");
    }

    #[test]
    fn splitting_a_long_line_keeps_every_byte_and_fits_the_budget() {
        let text = "First line\n".to_owned() + &"строка ⚙️".repeat(200);
        let chunks = split_text(&text, 97);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= 97)
        );
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn microcompact_keeps_recent_results_and_old_errors() {
        use ncore::core::ir::ToolResult;
        let large = "x".repeat(500);
        let mut messages = vec![ir::Message::new(
            Role::User,
            (0..7)
                .map(|i| {
                    Block::ToolResult(ToolResult {
                        call_id: i.to_string(),
                        name: Some("read".into()),
                        content: vec![Block::text(&large)],
                        is_error: i == 1,
                        cache: None,
                    })
                })
                .collect(),
        )];
        let original = messages.clone();
        clear_old_results(&mut messages);
        assert_ne!(messages[0].content[0], original[0].content[0]);
        assert_eq!(messages[0].content[1..], original[0].content[1..]);
    }

    #[test]
    fn compacted_tool_step_keeps_the_result_id_and_both_ends() {
        use ncore::core::ir::ToolResult;
        let original = format!("start{}end", "ж".repeat(5_000));
        let mut messages = [ir::Message::new(
            Role::User,
            vec![Block::ToolResult(ToolResult {
                call_id: "call".into(),
                name: Some("read".into()),
                content: vec![Block::text(&original)],
                is_error: false,
                cache: None,
            })],
        )];
        shorten_tool_results(&mut messages);
        let Block::ToolResult(result) = &messages[0].content[0] else {
            panic!("tool result")
        };
        let Block::Text { text, .. } = &result.content[0] else {
            panic!("text result")
        };
        assert_eq!(result.call_id, "call");
        assert!(text.starts_with("start") && text.ends_with("end"));
        assert!(text.len() < original.len() / 2);
    }
}
