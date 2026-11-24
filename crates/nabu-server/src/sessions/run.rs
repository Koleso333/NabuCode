use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nabu_protocol::{
    AssistantMessage, CacheTokens, Completed, ContextParts, ErrorData, Event, FilePart, Message, MessageError,
    MessageWithParts, ModelRef, Part, RetryPart, SessionStatus, Span, TextPart, Tokens, ToolPart, ToolState,
    UserMessage,
};
use ncore::core::ir::{
    self, Block, BlockStart, Delta, Media, MediaSource, ReasoningCfg, Role, StopReason,
    StreamEvent, Thinking, ToolCall, ToolResult,
};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Agent, AgentHook, CachePolicy, CancelToken, Error, LoopEvent, RunOptions, RunStop, Tools, Usage,
};
use ncore_tokio::TokioClock;
use serde_json::Value;

use super::store::{base64_of, local_path};
use super::{Interrupt, Sessions, Spawner, compact, id};
use crate::api_keys::agentrouter_upstream_busy;
use crate::prompt;
use crate::skills;
use crate::tools::{Jobs, Toolbox};

/// Runs the agent for `user`: every model step becomes its own assistant message.
pub async fn reply(
    sessions: &Arc<Sessions>,
    directory: &str,
    user: &UserMessage,
    jobs: Arc<Jobs>,
    interrupt: Arc<Interrupt>,
) {
    let (toolbox, subagent) = toolbox_of(sessions, directory, user, jobs);
    let toolbox = toolbox
        .showing_in(sessions.clone(), &user.session_id)
        .interrupted_by(interrupt.clone());
    let mut turn = Turn::new(sessions, directory, user);
    let mut retries = 0;
    let mut portable_history = false;
    let mut failover_error = None;
    let stop = loop {
        let request = request(sessions, directory, user, subagent);
        let last_id = turn.last_id.clone();
        let cancel = interrupt.next_run();
        let stop = match interrupt
            .guard(run(sessions, &user.model, request, &toolbox, &cancel, &mut turn, portable_history))
            .await
        {
            Ok(RunStop::Failed(Error::Config(message))) => {
                break RunStop::Failed(
                    failover_error
                        .take()
                        .map(Error::Provider)
                        .unwrap_or(Error::Config(message)),
                );
            }
            Ok(stop) => {
                failover_error = None;
                stop
            }
            Err(error) => {
                if matches!(user.model.provider_id.as_str(), "agentrouter" | "gemini") {
                    failover_error = Some(error);
                }
                continue;
            }
        };
        if interrupt.take_cut() {
            if !turn.cut() {
                break RunStop::Cancelled;
            }
            retries = 0;
            continue;
        }
        // A step that went through starts the delays over.
        if turn.last_id != last_id {
            retries = 0;
        }
        if turn.message.is_some() {
            break stop;
        }
        let Some((delay, max)) = retry_delay(&user.model, &stop, retries) else {
            break stop;
        };
        portable_history |= agentrouter_resource_mismatch(&user.model, &stop);
        retries = retries.saturating_add(1);
        let cancel = interrupt.next_run();
        if !interrupt.guard(wait_retry(&mut turn, (retries, max), &stop, delay, &cancel)).await {
            // A message sent during the wait goes to the model right away
            if interrupt.take_cut() && turn.cut() {
                retries = 0;
                continue;
            }
            break RunStop::Cancelled;
        }
    };
    turn.end(stop);
    sessions
        .models
        .refresh_quota(&user.model.provider_id, AFTER_TURN_GAP);
}

/// The session's tools for `user`'s model, and whether the session is a subagent's.
fn toolbox_of(sessions: &Arc<Sessions>, directory: &str, user: &UserMessage, jobs: Arc<Jobs>) -> (Toolbox, bool) {
    let (tools, subagent) = sessions.tools_of(&user.session_id);
    let loaded = loaded_tools(&sessions.messages(&user.session_id).unwrap_or_default());
    let toolbox = Toolbox::new(Path::new(directory), jobs).limited(tools).loading(loaded).with_skills(sessions.skills(directory));
    if subagent {
        return (toolbox, true);
    }
    let spawner = Spawner { sessions: sessions.clone(), parent: user.session_id.clone(), model: user.model.clone() };
    (toolbox.spawning(spawner), false)
}

/// The request the next step would send, estimated by kind.
pub(super) fn context_parts(sessions: &Arc<Sessions>, directory: &str, user: &UserMessage) -> ContextParts {
    let (toolbox, subagent) = toolbox_of(sessions, directory, user, Arc::default());
    let request = request(sessions, directory, user, subagent);
    ContextParts {
        system: request.system.iter().map(compact::block_tokens).sum(),
        tools: compact::tool_tokens(&toolbox.definitions()),
        messages: compact::estimate_messages(&request.messages),
        deferred: compact::tool_tokens(&toolbox.unloaded()),
    }
}

/// Default pauses before retrying a failed request.
const RETRY_DELAYS: [u64; 6] = [1, 2, 5, 10, 10, 30];

/// The pause before the next attempt and how many attempts there are, None when unlimited.
fn retry_delay(model: &ModelRef, stop: &RunStop, retries: u32) -> Option<(u64, Option<u32>)> {
    // DeepAI may bill a failed request, so only its 409 comes back: the chat's request cut a moment ago still holds the session
    if model.provider_id == "deepai-account" {
        let busy = matches!(stop, RunStop::Failed(Error::Provider(e)) if e.status == Some(409));
        return (busy && retries < 3).then_some((2, Some(3)));
    }
    let transient = match stop {
        RunStop::Failed(Error::Provider(e))
            if agentrouter_resource_mismatch(model, stop)
                || (model.provider_id == "agentrouter"
                    && agentrouter_upstream_busy(&e.message)) =>
        {
            return Some((if retries < 5 { 1 } else { 5 }, None));
        }
        RunStop::Failed(Error::Provider(e)) => e.kind.is_transient(),
        RunStop::Failed(Error::Transport(_)) => true,
        _ => false,
    };
    let delay = RETRY_DELAYS.get(retries as usize).copied().filter(|_| transient)?;
    Some((delay, Some(RETRY_DELAYS.len() as u32)))
}

fn agentrouter_resource_mismatch(model: &ModelRef, stop: &RunStop) -> bool {
    let RunStop::Failed(Error::Provider(error)) = stop else {
        return false;
    };
    model.provider_id == "agentrouter"
        && error
            .message
            .split_once("The requested item was created under a different ")
            .is_some_and(|(_, resource)| {
                resource.starts_with("Azure OpenAI resource.")
                    || resource.starts_with("*** OpenAI resource.")
            })
}

struct PortableAgentRouterHistory;

impl AgentHook for PortableAgentRouterHistory {
    fn before_step<'a>(
        &'a self,
        _step: u32,
        request: &'a mut ir::Request,
    ) -> BoxFuture<'a, Result<(), String>> {
        // Tool results refer to call_id. The separate Responses item id belongs to Azure.
        request.messages.retain_mut(|message| {
            message.content.retain_mut(|block| match block {
                Block::Thinking(_) | Block::Opaque(_) => false,
                Block::ToolCall(call) => {
                    call.sig = None;
                    true
                }
                _ => true,
            });
            !message.content.is_empty()
        });
        boxed(async { Ok(()) })
    }
}

/// Shows the retry in the timeline and the session status and sleeps; false when the turn was cancelled meanwhile.
async fn wait_retry(
    turn: &mut Turn<'_>,
    (attempt, max): (u32, Option<u32>),
    stop: &RunStop,
    delay: u64,
    cancel: &CancelToken,
) -> bool {
    let RunStop::Failed(error) = stop else {
        return true;
    };
    let next = id::now() + delay * 1000;
    turn.show_retry(attempt, max, retry_reason(error), next);
    let (sessions, directory, user) = (turn.sessions, turn.directory, turn.user);
    let status = SessionStatus::Retry {
        attempt,
        message: error_of(error.clone()).data.message,
        next,
    };
    sessions.set_status(directory, &user.session_id, status);
    let waited =
        ncore::cancel::or_cancel(cancel, tokio::time::sleep(Duration::from_secs(delay)))
            .await
            .is_some();
    sessions.set_status(
        directory,
        &user.session_id,
        SessionStatus::Busy { compaction: None },
    );
    waited
}

/// The status code and the provider's text, as the retry row shows it.
fn retry_reason(error: &Error) -> String {
    match error {
        Error::Provider(e) => match e.status {
            Some(status) => format!("{status}: {}", e.message),
            None => e.message.clone(),
        },
        other => error_of(other.clone()).data.message,
    }
}

/// One agent run on one route. A quota refusal before streaming triggers account failover.
async fn run(
    sessions: &Arc<Sessions>,
    model: &ModelRef,
    mut request: ir::Request,
    toolbox: &Toolbox,
    cancel: &CancelToken,
    turn: &mut Turn<'_>,
    portable_history: bool,
) -> Result<RunStop, ncore::core::ProviderError> {
    let route = match sessions
        .models
        .route(&turn.user.session_id, model, &mut request)
    {
        Ok(route) => route,
        Err(e) => return Ok(RunStop::Failed(Error::Config(e))),
    };
    let mut generate = route.options.clone();
    generate.cache.get_or_insert_with(CachePolicy::default);
    let options = RunOptions {
        generate,
        cancel: cancel.clone(),
        restart_streams: false,
        max_steps: 1000,
        ..RunOptions::default()
    };
    let hook = compact::BeforeStep::new(
        sessions.clone(),
        turn.user.session_id.clone(),
        model.clone(),
        cancel.clone(),
    );
    let agent = Agent::new(&sessions.models.core, &route.endpoint, Arc::new(TokioClock))
        .tools(toolbox)
        .hook(Arc::new(hook));
    let agent = if portable_history {
        agent.hook(Arc::new(PortableAgentRouterHistory))
    } else {
        agent
    };
    let mut events = agent.run(request, options);
    while let Some(event) = events.next().await {
        let LoopEvent::Done(mut out) = event else {
            turn.event(event);
            continue;
        };
        // Record Gemini's model-specific refusal even if an earlier step already streamed.
        if let (RunStop::Failed(Error::Provider(e)), Some(account)) = (&mut out.stop, &route.account)
            && !model.fast
            && (model.provider_id == "gemini" || turn.message.is_none())
            && sessions.models.limited(model, account, e)
            && turn.message.is_none()
        {
            return Err(e.clone());
        }
        return Ok(out.stop);
    }
    Ok(RunStop::Cancelled)
}

/// The conversation so far as one request, behind the system prompt.
fn request(
    sessions: &Sessions,
    directory: &str,
    user: &UserMessage,
    subagent: bool,
) -> ir::Request {
    let messages = sessions.context(&user.session_id, &user.model);
    let reasoning = user.model.variant.clone().map(|effort| ReasoningCfg {
        effort: Some(effort),
        include: Some(true),
        ..ReasoningCfg::default()
    });
    let system = prompt::system(&prompt::Context {
        model_name: &sessions.models.name(&user.model),
        model_id: &user.model.model_id,
        directory: Path::new(directory),
        subagent,
    });
    ir::Request {
        model: user.model.model_id.clone(),
        system,
        messages,
        reasoning,
        stream: true,
        ..ir::Request::default()
    }
}

pub(super) fn append_history(
    messages: &mut Vec<ir::Message>,
    history: &[MessageWithParts],
    model: &ModelRef,
    attachments: &Attachments,
) {
    let mut after_empty_error = false;
    for m in history {
        match &m.info {
            Message::User(_) => {
                let blocks = user_message(&m.parts, attachments);
                if blocks.is_empty() {
                    continue;
                }
                if after_empty_error {
                    messages.push(ir::Message::new(Role::User, blocks));
                } else {
                    push(messages, Role::User, blocks);
                }
                after_empty_error = false;
            }
            Message::Assistant(info) => {
                let preserve_reasoning = preserves_reasoning(model, &info.provider_id);
                let (mut blocks, results) = assistant_blocks(&m.parts, preserve_reasoning, attachments);
                if m.parts.iter().all(|part| matches!(part, Part::Retry(_)))
                    && let Some(error) = &info.error
                {
                    blocks.push(Block::text(format!(
                        "[NabuCode] No response was generated. Error: {}",
                        error.data.message
                    )));
                }
                after_empty_error =
                    blocks.is_empty() && (after_empty_error || info.error.is_some());
                push(messages, Role::Assistant, blocks);
                push(messages, Role::User, results);
            }
        }
    }
}

fn preserves_reasoning(model: &ModelRef, provider: &str) -> bool {
    model.provider_id == provider && matches!(provider, "workbuddy-account" | "deepai-account")
}

/// Deferred tools loaded by earlier `tools` calls of the session.
fn loaded_tools(history: &[MessageWithParts]) -> Vec<String> {
    let names = history
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|part| match part {
            Part::Tool(t) if t.tool == "tools" => match &t.state {
                ToolState::Completed { input, .. } => input.get("load").and_then(Value::as_array),
                _ => None,
            },
            _ => None,
        });
    names
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// Appends blocks, joining them to the last message of the same role.
pub(super) fn push(messages: &mut Vec<ir::Message>, role: Role, blocks: Vec<Block>) {
    if blocks.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.extend(blocks),
        _ => messages.push(ir::Message::new(role, blocks)),
    }
}

/// The person's message: skill marks read inside the text around them, the marked skills' texts after it.
fn user_message(parts: &[Part], attachments: &Attachments) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    // Whether the last block is the person's text, which a mark goes on, and whether it ends with a mark, which text goes on
    let (mut after_text, mut after_mark) = (false, false);
    for part in parts {
        let (piece, is_mark) = match part {
            Part::Skill(s) => (mark(&s.name), true),
            Part::Text(t) if !t.synthetic => (t.text.clone(), false),
            // The command ran before the message was sent
            Part::Command(_) => continue,
            _ => {
                blocks.extend(user_blocks(part, attachments));
                (after_text, after_mark) = (false, false);
                continue;
            }
        };
        match blocks.last_mut() {
            Some(Block::Text { text, .. }) if (is_mark && after_text) || after_mark => text.push_str(&piece),
            _ if piece.is_empty() => continue,
            _ => blocks.push(Block::text(piece)),
        }
        (after_text, after_mark) = (true, is_mark);
    }
    let mut seen: Vec<&str> = Vec::new();
    for part in parts {
        if let Part::Skill(s) = part
            && !seen.contains(&s.name.as_str())
        {
            seen.push(&s.name);
            blocks.push(Block::text(skills::content(&s.name, &s.path, &s.body)));
        }
    }
    blocks
}

/// Where the person marked a skill in their text.
pub(super) fn mark(name: &str) -> String {
    format!("<skill name=\"{name}\"/>")
}

fn user_blocks(part: &Part, attachments: &Attachments) -> Vec<Block> {
    match part {
        Part::Text(t) if !t.text.is_empty() => vec![Block::text(t.text.clone())],
        Part::File(f) => match attachments.inline(f).then(|| attachments.media(f)).flatten() {
            Some(media) => vec![media, Block::text(attachments.note(f, true))],
            None => vec![Block::text(attachments.note(f, false))],
        },
        _ => Vec::new(),
    }
}

/// How the person's attachments reach the model: the path of the kept copy or of the original,
/// and the file itself when the model can read it.
pub(super) struct Attachments {
    pub dir: std::path::PathBuf,
    pub images: bool,
}

impl Attachments {
    fn inline(&self, file: &FilePart) -> bool {
        let readable = matches!(file.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "application/pdf");
        // A file too big to copy is only ever given by its path
        self.images && readable && local_path(&file.url).is_none()
    }

    fn note(&self, file: &FilePart, shown: bool) -> String {
        let name = file.filename.as_deref().unwrap_or("unnamed");
        let path = local_path(&file.url).unwrap_or_else(|| self.dir.join(super::store::kept_name(file)));
        if file.mime == FOLDER {
            return format!("[Attached folder \"{name}\" at {}]", path.display());
        }
        let shown = if shown { "" } else { "is not shown to you directly; it " };
        format!("[Attached file \"{name}\" ({}) {shown}is saved at {}]", file.mime, path.display())
    }

    /// An attachment as the model sees it.
    fn media(&self, file: &FilePart) -> Option<Block> {
        Some(Block::Media {
            media: Media {
                mime: Some(file.mime.clone()),
                source: MediaSource::Base64(base64_of(&self.dir, file)?),
                filename: file.filename.clone(),
            },
            cache: None,
            ext: Default::default(),
        })
    }
}

/// A folder isn't copied: it comes as a `file:` URL of its own place.
const FOLDER: &str = "inode/directory";

/// A step's own blocks, and the results of its tool calls for the next user turn.
fn assistant_blocks(parts: &[Part], preserve_reasoning: bool, attachments: &Attachments) -> (Vec<Block>, Vec<Block>) {
    let mut blocks = Vec::new();
    let mut results = Vec::new();
    for part in parts {
        match part {
            Part::Text(t) if !t.text.is_empty() => blocks.push(Block::Text {
                text: t.text.clone(),
                cache: None,
                sig: sig_of(&t.sig),
                ext: Default::default(),
            }),
            Part::Reasoning(t) if t.sig.is_some() || preserve_reasoning => {
                blocks.push(Block::Thinking(Thinking {
                    text: t.text.clone(),
                    sig: sig_of(&t.sig),
                    redacted: false,
                }))
            }
            // A call cut off mid-arguments never ran and cannot be sent back.
            Part::Tool(t) if serde_json::from_str::<Value>(&t.args).is_ok() => {
                blocks.push(Block::ToolCall(ToolCall {
                    id: t.call_id.clone(),
                    name: t.tool.clone(),
                    args: t.args.clone(),
                    sig: sig_of(&t.sig),
                }));
                results.push(Block::ToolResult(result_of(t, attachments)));
            }
            _ => {}
        }
    }
    (blocks, results)
}

// Provider quota caches live about a minute, so asking more often mid-reply gives nothing new.
const AFTER_STEP_GAP: Duration = Duration::from_secs(60);
// Short, so the end of a reply is not read twice right after its last step.
const AFTER_TURN_GAP: Duration = Duration::from_secs(10);

/// What a stopped call gives the model, the same text NabuCore gives; the UI shows such calls as stopped.
const CANCELLED: &str = "the call was cancelled by the user";
/// Goes to the model before the messages the person cut its reply off with.
const CUT_NOTE: &str = "[The person interrupted you to send this.]";

fn result_of(part: &ToolPart, kept: &Attachments) -> ToolResult {
    let (content, is_error) = match &part.state {
        ToolState::Completed {
            output, attachments, ..
        } => {
            let mut content = vec![Block::text(output.clone())];
            content.extend(attachments.iter().filter_map(|file| kept.media(file)));
            (content, false)
        }
        ToolState::Error { error, .. } => (vec![Block::text(error.clone())], true),
        ToolState::Pending { .. } | ToolState::Running { .. } => (vec![Block::text(CANCELLED)], true),
    };
    ToolResult {
        call_id: part.call_id.clone(),
        name: Some(part.tool.clone()),
        content,
        is_error,
        cache: None,
    }
}

fn sig_of(value: &Option<Value>) -> Option<ir::Opaque> {
    value.clone().and_then(|v| serde_json::from_value(v).ok())
}

fn tool_attachment(part: &ToolPart, block: &Block) -> Option<FilePart> {
    let Block::Media { media, .. } = block else {
        return None;
    };
    let mime = media.mime.as_ref()?;
    let MediaSource::Base64(data) = &media.source else {
        return None;
    };
    Some(FilePart {
        id: id::ascending("prt"),
        session_id: part.session_id.clone(),
        message_id: part.message_id.clone(),
        mime: mime.clone(),
        filename: media.filename.clone(),
        url: format!("data:{mime};base64,{data}"),
    })
}

/// Loop events turned into assistant messages and their parts.
struct Turn<'a> {
    sessions: &'a Sessions,
    directory: &'a str,
    user: &'a UserMessage,
    /// The current step's message, created with its first part.
    message: Option<AssistantMessage>,
    /// The person's message the steps answer: the turn's own, then the last queued one delivered.
    parent_id: String,
    last_id: String,
    text: Option<(TextPart, bool)>,
    tool: Option<ToolPart>,
    tools: Vec<ToolPart>,
    /// The message showing retries of the current step; closed once a step goes through or the turn ends.
    retry: Option<(AssistantMessage, RetryPart)>,
}

impl<'a> Turn<'a> {
    fn new(sessions: &'a Sessions, directory: &'a str, user: &'a UserMessage) -> Self {
        Self {
            sessions,
            directory,
            user,
            message: None,
            parent_id: user.id.clone(),
            last_id: user.id.clone(),
            text: None,
            tool: None,
            tools: Vec::new(),
            retry: None,
        }
    }

    fn event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::Stream { event, .. } => self.stream(event),
            LoopEvent::ToolStart { call, .. } => self.tool_start(&call),
            LoopEvent::ToolEnd { result, .. } => self.tool_end(&result),
            LoopEvent::StepEnd { usage, stop, .. } => self.step_end(&usage, stop),
            _ => {}
        }
    }

    fn stream(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::BlockStart {
                block: BlockStart::Text,
                ..
            } => self.open_text(false),
            StreamEvent::BlockStart {
                block: BlockStart::Thinking { .. },
                ..
            } => self.open_text(true),
            StreamEvent::BlockStart {
                block: BlockStart::ToolCall { id, name },
                ..
            } => self.open_tool(id, name),
            StreamEvent::Delta {
                delta: Delta::Text(text) | Delta::Thinking(text),
                ..
            } => self.append(text),
            StreamEvent::Delta {
                delta: Delta::ToolArgs(args),
                ..
            } => {
                if let Some(tool) = self.tool.as_mut() {
                    tool.args.push_str(&args);
                }
            }
            StreamEvent::Delta {
                delta: Delta::Sig(sig),
                ..
            } => self.sign(serde_json::to_value(sig).ok()),
            StreamEvent::BlockEnd { .. } => {
                self.close_text();
                self.close_tool();
            }
            _ => {}
        }
    }

    fn message_id(&mut self) -> String {
        if let Some(m) = &self.message {
            return m.id.clone();
        }
        self.close_retry();
        let message = self.open_message();
        self.message = Some(message);
        self.last_id.clone()
    }

    fn open_message(&mut self) -> AssistantMessage {
        // A step after a delivered queued message answers under it
        if let Some(queued) = self.sessions.take_delivered(&self.user.session_id) {
            self.parent_id = queued.clone();
            self.last_id = queued;
        }
        let user = self.user;
        let message = AssistantMessage {
            id: id::ascending_after("msg", &self.last_id),
            session_id: user.session_id.clone(),
            parent_id: self.parent_id.clone(),
            provider_id: user.model.provider_id.clone(),
            model_id: user.model.model_id.clone(),
            variant: user.model.variant.clone(),
            agent: user.agent.clone(),
            time: Completed {
                created: id::now(),
                completed: None,
            },
            error: None,
            tokens: Tokens::default(),
            cost: 0.0,
            finish: None,
        };
        self.last_id = message.id.clone();
        self.sessions.add_message(
            self.directory,
            MessageWithParts {
                info: Message::Assistant(message.clone()),
                parts: Vec::new(),
            },
        );
        message
    }

    fn show_retry(&mut self, attempt: u32, max: Option<u32>, error: String, next: u64) {
        if self.retry.is_none() {
            let message = self.open_message();
            let part = RetryPart {
                id: id::ascending("prt"),
                session_id: self.user.session_id.clone(),
                message_id: message.id.clone(),
                attempt,
                max,
                error: String::new(),
                next: None,
            };
            self.retry = Some((message, part));
        }
        let (_, part) = self.retry.as_mut().expect("opened");
        part.attempt = attempt;
        part.max = max;
        part.error = error;
        part.next = Some(next);
        self.sessions.put_part(self.directory, Part::Retry(part.clone()));
    }

    fn close_retry(&mut self) {
        if let Some(mut message) = self.end_retry() {
            message.time.completed = Some(id::now());
            self.sessions.finish_message(self.directory, Message::Assistant(message));
        }
    }

    /// Stops the retry row's countdown and hands back its message.
    fn end_retry(&mut self) -> Option<AssistantMessage> {
        let (message, mut part) = self.retry.take()?;
        part.next = None;
        self.sessions.put_part(self.directory, Part::Retry(part));
        Some(message)
    }

    fn open_text(&mut self, reasoning: bool) {
        self.close_text();
        let body = TextPart {
            id: id::ascending("prt"),
            session_id: self.user.session_id.clone(),
            message_id: self.message_id(),
            text: String::new(),
            time: Some(Span {
                start: id::now(),
                end: None,
            }),
            sig: None,
            synthetic: false,
        };
        self.sessions
            .put_part(self.directory, text_part(body.clone(), reasoning));
        self.text = Some((body, reasoning));
    }

    fn append(&mut self, text: String) {
        if self.text.is_none() {
            self.open_text(false);
        }
        let (part, _) = self.text.as_mut().expect("opened");
        part.text.push_str(&text);
        self.sessions.append(part, &text);
        self.sessions.emit(Event::PartDelta {
            directory: self.directory.into(),
            message_id: part.message_id.clone(),
            part_id: part.id.clone(),
            field: "text".into(),
            delta: text,
        });
    }

    fn close_text(&mut self) {
        if let Some((mut part, reasoning)) = self.text.take() {
            if let Some(time) = part.time.as_mut() {
                time.end = Some(id::now());
            }
            self.sessions
                .put_part(self.directory, text_part(part, reasoning));
        }
    }

    fn open_tool(&mut self, call_id: String, tool: String) {
        self.close_text();
        let part = ToolPart {
            id: id::ascending("prt"),
            session_id: self.user.session_id.clone(),
            message_id: self.message_id(),
            call_id,
            tool,
            state: ToolState::Pending {
                input: Value::Object(Default::default()),
                raw: String::new(),
            },
            args: String::new(),
            sig: None,
        };
        self.sessions
            .put_part(self.directory, Part::Tool(part.clone()));
        self.tool = Some(part);
    }

    fn close_tool(&mut self) {
        if let Some(mut part) = self.tool.take() {
            part.state = ToolState::Pending {
                input: input_of(&part.args),
                raw: part.args.clone(),
            };
            self.sessions
                .put_part(self.directory, Part::Tool(part.clone()));
            self.tools.push(part);
        }
    }

    /// A signature belongs to the block streaming right now.
    fn sign(&mut self, sig: Option<Value>) {
        if let Some((part, _)) = self.text.as_mut() {
            part.sig = sig;
        } else if let Some(part) = self.tool.as_mut() {
            part.sig = sig;
        }
    }

    fn tool_start(&mut self, call: &ToolCall) {
        // The loop renames empty or repeated call ids; match those by name instead.
        let found = self
            .tools
            .iter()
            .position(|t| t.call_id == call.id)
            .or_else(|| {
                self.tools.iter().position(|t| {
                    t.tool == call.name && matches!(t.state, ToolState::Pending { .. })
                })
            });
        let Some(i) = found else { return };
        let part = &mut self.tools[i];
        part.call_id = call.id.clone();
        let metadata = self
            .sessions
            .linked(&self.user.session_id, &part.call_id, false);
        part.state = ToolState::Running {
            input: input_of(&part.args),
            time: Span {
                start: id::now(),
                end: None,
            },
            metadata,
        };
        self.sessions
            .put_part(self.directory, Part::Tool(part.clone()));
    }

    fn tool_end(&mut self, result: &ToolResult) {
        let Some(part) = self.tools.iter_mut().find(|t| t.call_id == result.call_id) else {
            return;
        };
        let start = match &part.state {
            ToolState::Running { time, .. } => time.start,
            _ => id::now(),
        };
        let time = Span {
            start,
            end: Some(id::now()),
        };
        let input = input_of(&part.args);
        let text = text_of(&result.content);
        let metadata = match self
            .sessions
            .linked(&self.user.session_id, &part.call_id, true)
        {
            Value::Null => Value::Object(Default::default()),
            linked => linked,
        };
        part.state = if result.is_error {
            ToolState::Error {
                input,
                error: text,
                time,
            }
        } else {
            ToolState::Completed {
                input,
                output: text,
                attachments: result
                    .content
                    .iter()
                    .filter_map(|block| tool_attachment(part, block))
                    .collect(),
                title: String::new(),
                metadata,
                time,
            }
        };
        self.sessions
            .put_part(self.directory, Part::Tool(part.clone()));
    }

    fn step_end(&mut self, usage: &Usage, stop: Option<StopReason>) {
        if let Some(message) = self.message.as_mut() {
            message.tokens = tokens(usage);
            message.finish = stop.map(finish_of);
        }
        self.finish(None);
        self.sessions
            .models
            .refresh_quota(&self.user.model.provider_id, AFTER_STEP_GAP);
    }

    /// Closes the reply the person cut off and hands the model their queued messages; false when the queue emptied meanwhile.
    fn cut(&mut self) -> bool {
        let note = self.message.is_some().then_some(CUT_NOTE);
        if !self.sessions.deliver_queue(&self.user.session_id, note) {
            return false;
        }
        self.finish(None);
        true
    }

    /// Closes the current message; a run that did not finish leaves its error on it.
    fn end(mut self, stop: RunStop) {
        let error = match stop {
            RunStop::Finished(_) | RunStop::ToolCalls(_) => return self.finish(None),
            RunStop::Cancelled => error_of(Error::Cancelled),
            RunStop::Failed(e) => error_of(e),
            RunStop::MaxSteps => MessageError {
                name: "UnknownError".into(),
                data: ErrorData {
                    message: "Stopped: the step limit was reached".into(),
                },
            },
        };
        // A turn that failed while retrying keeps its error on the retry row
        if self.message.is_none() {
            self.message = self.end_retry();
        }
        self.message_id();
        self.finish(Some(error));
    }

    fn finish(&mut self, error: Option<MessageError>) {
        self.close_retry();
        self.close_text();
        self.close_tool();
        for mut part in self.tools.drain(..) {
            if let ToolState::Pending { input, .. } | ToolState::Running { input, .. } = &part.state
            {
                let time = Span {
                    start: id::now(),
                    end: Some(id::now()),
                };
                part.state = ToolState::Error {
                    input: input.clone(),
                    error: CANCELLED.into(),
                    time,
                };
                self.sessions.put_part(self.directory, Part::Tool(part));
            }
        }
        if let Some(mut message) = self.message.take() {
            message.error = error;
            message.time.completed = Some(id::now());
            self.sessions
                .finish_message(self.directory, Message::Assistant(message));
        }
    }
}

fn text_part(body: TextPart, reasoning: bool) -> Part {
    if reasoning {
        Part::Reasoning(body)
    } else {
        Part::Text(body)
    }
}

/// Arguments for display; an object even when the model wrote broken JSON.
fn input_of(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tokens(usage: &Usage) -> Tokens {
    let read = usage.cache_read.unwrap_or(0);
    let write = usage.cache_write.unwrap_or(0);
    let reasoning = usage.reasoning.unwrap_or(0);
    Tokens {
        input: usage.input_total.unwrap_or(0).saturating_sub(read + write),
        output: usage.output_total.unwrap_or(0).saturating_sub(reasoning),
        reasoning,
        cache: CacheTokens { read, write },
    }
}

fn finish_of(stop: StopReason) -> String {
    match stop {
        StopReason::EndTurn | StopReason::StopSequence(_) => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool-calls",
        StopReason::ContentFilter | StopReason::Refusal => "content-filter",
        _ => "other",
    }
    .into()
}

fn error_of(error: Error) -> MessageError {
    let (name, message) = match error {
        Error::Cancelled => ("MessageAbortedError", "Aborted".to_owned()),
        Error::Provider(e) => ("APIError", e.message),
        Error::Config(m) => ("UnknownError", m),
        other => ("UnknownError", other.to_string()),
    };
    MessageError {
        name: name.into(),
        data: ErrorData { message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_keys::{ApiKeyAccounts, KeyProvider};
    use crate::models::Models;
    use nabu_protocol::{Prompt, PromptPart};
    use ncore::core::ErrorKind;
    use ncore::{Core, HttpRequest, HttpResponse, Transport, TransportError};
    use ncore_tokio::HyperTransport;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::sync::broadcast;

    fn no_files() -> Attachments {
        Attachments { dir: Default::default(), images: true }
    }

    #[test]
    fn attachments_the_model_cannot_read_go_as_paths() {
        let file = |id: &str, mime: &str, name: &str| {
            Part::File(FilePart {
                id: id.into(),
                session_id: "ses_1".into(),
                message_id: "msg_1".into(),
                mime: mime.into(),
                filename: Some(name.into()),
                url: "data:application/octet-stream;base64,AAAA".into(),
            })
        };
        let image = file("prt_1", "image/png", "shot.png");
        let archive = file("prt_2", "application/zip", "logs.zip");
        let dir = std::path::PathBuf::from("sessions").join("ses_1");
        let seeing = Attachments { dir: dir.clone(), images: true };
        let blind = Attachments { dir: dir.clone(), images: false };

        let png = dir.join("prt_1.png").display().to_string();
        let shown = user_blocks(&image, &seeing);
        let [Block::Media { .. }, Block::Text { text, .. }] = shown.as_slice() else {
            panic!("expected the image and its path")
        };
        assert!(text.contains(&png) && !text.contains("not shown"));
        let hidden = user_blocks(&image, &blind);
        let [Block::Text { text, .. }] = hidden.as_slice() else {
            panic!("expected a note")
        };
        assert!(text.contains("\"shot.png\"") && text.contains("not shown") && text.contains(&png));
        let archived = user_blocks(&archive, &seeing);
        let [Block::Text { text, .. }] = archived.as_slice() else {
            panic!("expected a note")
        };
        assert!(text.contains(&dir.join("prt_2.zip").display().to_string()));

        let Part::File(mut folder) = file("prt_3", FOLDER, "My App") else { unreachable!() };
        folder.url = if cfg!(windows) { "file:///C:/src/My%20App" } else { "file:///src/My%20App" }.into();
        let place = if cfg!(windows) { r"C:\src\My App" } else { "/src/My App" };
        let listed = user_blocks(&Part::File(folder), &seeing);
        let [Block::Text { text, .. }] = listed.as_slice() else {
            panic!("expected a note")
        };
        assert_eq!(text, &format!("[Attached folder \"My App\" at {place}]"));
    }

    #[derive(Default)]
    struct RouterMock {
        inputs: Mutex<Vec<Value>>,
    }

    impl Transport for RouterMock {
        fn send(
            &self,
            request: HttpRequest,
        ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                let (status, body) = if request.url.ends_with("/v1/models") {
                    (200, json!({"data":[{"id":"gpt-6.1-sol"}]}).to_string())
                } else {
                    let request: Value = serde_json::from_slice(&request.body).unwrap();
                    if request["stream"] == false {
                        (
                            200,
                            json!({"status":"completed","output":[{"content":[{"text":"OK"}]}]})
                                .to_string(),
                        )
                    } else {
                        let mut inputs = self.inputs.lock().unwrap();
                        inputs.push(request["input"].clone());
                        match inputs.len() {
                            1 | 4 => (200, router_tool_step(inputs.len())),
                            2 => (429, json!({"error":{"message":"Too many requests"}}).to_string()),
                            3 => (400, json!({"error":{"message":"The requested item was created under a different Azure OpenAI resource."}}).to_string()),
                            5 => (200, router_events([
                                json!({"type":"response.content_part.added"}),
                                json!({"type":"response.output_text.delta","delta":"Recovered"}),
                                json!({"type":"response.content_part.done"}),
                                json!({"type":"response.completed","response":{"status":"completed"}}),
                            ])),
                            6 => (429, json!({"error":{"type":"rate_limit_error","code":"insufficient_quota","message":"Quota exhausted"}}).to_string()),
                            _ => panic!("unexpected model request"),
                        }
                    }
                };
                Ok(HttpResponse {
                    status,
                    headers: Vec::new(),
                    body: Box::pin(futures_util::stream::once(async move { Ok(body.into()) })),
                })
            })
        }
    }

    fn router_events(events: impl IntoIterator<Item = Value>) -> String {
        events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    fn router_tool_step(index: usize) -> String {
        let reasoning =
            json!({"type":"reasoning","id":format!("rs_{index}"),"encrypted_content":"opaque"});
        let call = json!({
            "type":"function_call", "id":format!("fc_{index}"), "call_id":format!("call_{index}"),
            "name":"read", "arguments":"{\"path\":\"fixture.txt\"}"
        });
        router_events([
            json!({"type":"response.output_item.added","output_index":0,"item":reasoning}),
            json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
            json!({"type":"response.output_item.added","output_index":1,"item":call}),
            json!({"type":"response.output_item.done","output_index":1,"item":call}),
            json!({"type":"response.completed","response":{"status":"completed"}}),
        ])
    }

    /// Sessions whose only provider is AgentRouter, answered by `mock`.
    async fn sessions_over<T: Transport + 'static>(root: &Path, mock: Arc<T>, events: broadcast::Sender<Event>) -> Arc<Sessions> {
        let router = ApiKeyAccounts::load(
            KeyProvider::AgentRouter,
            &root,
            mock.clone(),
            events.clone(),
        );
        router.add(vec!["fake-key".into()]).unwrap();
        router.check_all().await;
        let net = Arc::new(HyperTransport::new());
        let models = Models::new(
            Core::builder(mock.clone()).build(),
            crate::antigravity::Accounts::load(
                root.join("antigravity-accounts.json"),
                net.clone(),
                events.clone(),
            ),
            crate::claude::ClaudeAccounts::load(&root, net.clone(), events.clone()),
            crate::chatgpt::ChatGptAccounts::load(&root, net.clone(), events.clone()),
            crate::zcode::ZCodeAccounts::load(&root, net.clone(), events.clone()),
            crate::workbuddy::WorkBuddyAccounts::load(&root, net.clone(), events.clone()),
            crate::deepai::DeepAIAccounts::load(&root, net.clone(), events.clone()),
            crate::grok::GrokAccounts::load(&root, net.clone(), events.clone()),
            router,
            ApiKeyAccounts::load(KeyProvider::CoArena, &root, net.clone(), events.clone()),
            ApiKeyAccounts::load(KeyProvider::Clauden, &root, net.clone(), events.clone()),
            ApiKeyAccounts::load(KeyProvider::Gemini, &root, net.clone(), events.clone()),
            ApiKeyAccounts::load(KeyProvider::NvidiaBuild, &root, net, events.clone()),
            crate::custom_providers::CustomProviders::load(&root),
            crate::providers::Switches::load(&root),
        );
        Sessions::load(root, Arc::new(models), events)
    }

    #[tokio::test]
    async fn agentrouter_recovers_from_throttling_and_azure_history_rejection() {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "nabu-router-retry-{:x}",
            u64::from_ne_bytes(random)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("fixture.txt"), "result from the tool").unwrap();
        let (events, mut received) = broadcast::channel(128);
        let mock = Arc::new(RouterMock::default());
        let sessions = sessions_over(&root, mock.clone(), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let mut prompt = Prompt {
            session_id: session.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef {
                provider_id: "agentrouter".into(),
                model_id: "gpt-6.1-sol".into(),
                variant: None,
                fast: false,
            },
            parts: vec![PromptPart::Text {
                id: id::ascending("prt"),
                text: "Read fixture.txt".into(),
            }],
        };
        sessions.prompt(prompt.clone()).unwrap();

        let mut retry_messages = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match received.recv().await.unwrap() {
                    Event::SessionStatus {
                        status: SessionStatus::Retry { message, .. },
                        ..
                    } => {
                        retry_messages.push(message);
                    }
                    Event::SessionStatus {
                        status: SessionStatus::Idle,
                        ..
                    } => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let history = sessions.messages(&session.id).unwrap();
        let inputs = mock.inputs.lock().unwrap();
        assert_eq!(retry_messages.len(), 2);
        assert_eq!(retry_messages[0], "Too many requests");
        assert!(retry_messages[1].contains("different Azure OpenAI resource"));
        assert_eq!(inputs.len(), 5);
        assert_eq!(inputs[1], inputs[2]);
        let rejected = inputs[2].as_array().unwrap();
        assert!(
            rejected
                .iter()
                .any(|item| item["encrypted_content"] == "opaque")
        );
        assert!(rejected.iter().any(|item| item["id"] == "fc_1"));
        for (input, calls) in [
            (&inputs[3], &["call_1"][..]),
            (&inputs[4], &["call_1", "call_4"][..]),
        ] {
            let items = input.as_array().unwrap();
            assert!(
                items
                    .iter()
                    .all(|item| item.get("id").is_none() && item["type"] != "reasoning")
            );
            for call in calls {
                assert!(
                    items
                        .iter()
                        .any(|item| item["type"] == "function_call" && item["call_id"] == *call)
                );
                assert!(items.iter().any(|item| {
                    item["type"] == "function_call_output"
                        && item["call_id"] == *call
                        && item["output"]
                            .as_str()
                            .is_some_and(|output| output.contains("result from the tool"))
                }));
            }
        }
        let parts: Vec<_> = history.iter().flat_map(|message| &message.parts).collect();
        let retries: Vec<_> = parts.iter().filter_map(|part| match part {
            Part::Retry(p) => Some((p.attempt, p.max, p.error.clone(), p.next)),
            _ => None,
        }).collect();
        // Both failures came before the step went through, so they share one row, closed once it did
        let reason = "400: The requested item was created under a different Azure OpenAI resource.".to_owned();
        assert_eq!(retries, [(2, None, reason, None)]);
        assert_eq!(
            parts
                .iter()
                .filter(|part| matches!(part, Part::Tool(_)))
                .count(),
            2
        );
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, Part::Reasoning(p) if p.sig.is_some()))
        );
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, Part::Text(p) if p.text == "Recovered"))
        );
        drop(inputs);
        prompt.message_id = id::ascending("msg");
        prompt.parts = vec![PromptPart::Text {
            id: id::ascending("prt"),
            text: "Continue".into(),
        }];
        sessions.prompt(prompt).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match received.recv().await.unwrap() {
                    Event::SessionStatus {
                        status: SessionStatus::Idle,
                        ..
                    } => break,
                    Event::SessionStatus {
                        status: SessionStatus::Retry { .. },
                        ..
                    } => {
                        panic!("confirmed quota exhaustion started a retry");
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let history = sessions.messages(&session.id).unwrap();
        let Message::Assistant(last) = &history.last().unwrap().info else {
            panic!("expected an assistant error");
        };
        let error = last.error.as_ref().unwrap();
        assert_eq!(error.name, "APIError");
        assert_eq!(error.data.message, "Quota exhausted");
        assert_eq!(mock.inputs.lock().unwrap().len(), 6);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Answers a tool step, then texts; requests 1 and 3 hold until the test lets them go.
    #[derive(Default)]
    struct QueueMock {
        inputs: Mutex<Vec<Value>>,
        started: tokio::sync::Notify,
        go: tokio::sync::Notify,
    }

    impl Transport for QueueMock {
        fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                if request.url.ends_with("/v1/models") {
                    let body = json!({"data":[{"id":"gpt-6.1-sol"}]}).to_string();
                    return Ok(HttpResponse { status: 200, headers: Vec::new(), body: Box::pin(futures_util::stream::once(async move { Ok(body.into()) })) });
                }
                let request: Value = serde_json::from_slice(&request.body).unwrap();
                let body = if request["stream"] != true {
                    json!({"status":"completed","output":[{"content":[{"text":"OK"}]}]}).to_string()
                } else {
                    let count = {
                        let mut inputs = self.inputs.lock().unwrap();
                        inputs.push(request["input"].clone());
                        inputs.len()
                    };
                    if count == 1 || count == 3 {
                        self.started.notify_one();
                        self.go.notified().await;
                    }
                    let text = |text: &str| router_events([
                        json!({"type":"response.content_part.added"}),
                        json!({"type":"response.output_text.delta","delta":text}),
                        json!({"type":"response.content_part.done"}),
                        json!({"type":"response.completed","response":{"status":"completed"}}),
                    ]);
                    match count {
                        1 => router_tool_step(1),
                        2 => text("Done"),
                        3 => text("Answer"),
                        _ => text("Second"),
                    }
                };
                Ok(HttpResponse { status: 200, headers: Vec::new(), body: Box::pin(futures_util::stream::once(async move { Ok(body.into()) })) })
            })
        }
    }

    #[tokio::test]
    async fn queued_messages_reach_the_next_step_or_the_next_turn() {
        let root = std::env::temp_dir().join(format!("nabu-queue-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("fixture.txt"), "result from the tool").unwrap();
        let (events, mut received) = broadcast::channel(256);
        let mock = Arc::new(QueueMock::default());
        let sessions = sessions_over(&root, mock.clone(), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let prompt = |text: &str| Prompt {
            session_id: session.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
            parts: vec![PromptPart::Text { id: id::ascending("prt"), text: text.into() }],
        };
        let idle = async |received: &mut broadcast::Receiver<Event>| {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(received.recv().await.unwrap(), Event::SessionStatus { status: SessionStatus::Idle, .. }) {}
            })
            .await
            .unwrap();
        };
        let texts = |sessions: &Sessions| -> Vec<String> {
            sessions.messages(&session.id).unwrap().iter().map(|message| {
                let role = if matches!(message.info, Message::User(_)) { "user" } else { "model" };
                let text: Vec<&str> = message.parts.iter().filter_map(|part| match part { Part::Text(p) => Some(p.text.as_str()), _ => None }).collect();
                format!("{role}: {}", text.join(" "))
            }).collect()
        };

        sessions.prompt(prompt("Read fixture.txt")).unwrap();
        mock.started.notified().await;
        let first = prompt("Also check");
        sessions.prompt(first.clone()).unwrap();
        sessions.prompt(prompt("the logs")).unwrap();
        // Taken back and sent again, a message returns to its place in the queue
        sessions.dequeue(&session.id, &first.message_id).unwrap();
        sessions.prompt(first).unwrap();
        let queued: Vec<_> = sessions.queue(&session.id).into_iter().map(|p| match &p.parts[0] { PromptPart::Text { text, .. } => text.clone(), _ => String::new() }).collect();
        assert_eq!(queued, ["Also check", "the logs"]);
        mock.go.notify_one();
        idle(&mut received).await;
        assert!(sessions.queue(&session.id).is_empty());
        assert_eq!(texts(&sessions), ["user: Read fixture.txt", "model: ", "user: Also check", "user: the logs", "model: Done"]);
        let history = sessions.messages(&session.id).unwrap();
        let Message::Assistant(answer) = &history[4].info else { panic!("expected the answer") };
        assert_eq!(answer.parent_id, history[3].info.id());
        let second = mock.inputs.lock().unwrap()[1].to_string();
        let (output, queued) = (second.find("result from the tool").unwrap(), second.find("Also check").unwrap());
        assert!(output < queued, "the queued message follows the tool result");

        sessions.prompt(prompt("Question")).unwrap();
        mock.started.notified().await;
        sessions.prompt(prompt("One more")).unwrap();
        sessions.prompt(prompt("and this")).unwrap();
        mock.go.notify_one();
        idle(&mut received).await;
        idle(&mut received).await;
        assert_eq!(&texts(&sessions)[5..], ["user: Question", "model: Answer", "user: One more", "user: and this", "model: Second"]);
        assert_eq!(mock.inputs.lock().unwrap().len(), 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Answers every request with a long text; the one after `hold` is set waits until the test lets it go.
    #[derive(Default)]
    struct CompactMock {
        hold: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        go: tokio::sync::Notify,
    }

    impl Transport for CompactMock {
        fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                let models = request.url.ends_with("/v1/models");
                let request: Value = serde_json::from_slice(&request.body).unwrap_or_default();
                let body = if models {
                    json!({"data":[{"id":"gpt-6.1-sol"}]}).to_string()
                } else if request["stream"] != true {
                    json!({"status":"completed","output":[{"content":[{"text":"OK"}]}]}).to_string()
                } else {
                    if self.hold.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        self.started.notify_one();
                        self.go.notified().await;
                    }
                    router_events([
                        json!({"type":"response.content_part.added"}),
                        json!({"type":"response.output_text.delta","delta":"word ".repeat(400)}),
                        json!({"type":"response.content_part.done"}),
                        json!({"type":"response.completed","response":{"status":"completed"}}),
                    ])
                };
                Ok(HttpResponse { status: 200, headers: Vec::new(), body: Box::pin(futures_util::stream::once(async move { Ok(body.into()) })) })
            })
        }
    }

    #[tokio::test]
    async fn a_command_runs_first_and_the_rest_follows_without_it() {
        let root = std::env::temp_dir().join(format!("nabu-command-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, mut received) = broadcast::channel(256);
        let mock = Arc::new(CompactMock::default());
        let sessions = sessions_over(&root, mock.clone(), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let prompt = |parts: Vec<PromptPart>| Prompt {
            session_id: session.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
            parts,
        };
        let text = |text: &str| PromptPart::Text { id: id::ascending("prt"), text: text.into() };
        let command = || PromptPart::Command { id: id::ascending("prt"), name: "compact".into() };
        let idle = async |received: &mut broadcast::Receiver<Event>| {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(received.recv().await.unwrap(), Event::SessionStatus { status: SessionStatus::Idle, .. }) {}
            })
            .await
            .unwrap();
        };
        let last_user = |sessions: &Sessions| -> Vec<String> {
            let history = sessions.messages(&session.id).unwrap();
            let user = history.iter().rev().find(|message| matches!(message.info, Message::User(_))).unwrap();
            user.parts.iter().map(|part| match part { Part::Text(p) => p.text.clone(), Part::Command(_) => "<command>".into(), _ => "?".into() }).collect()
        };

        sessions.prompt(prompt(vec![text("Hi")])).unwrap();
        idle(&mut received).await;
        // One short exchange: the summary can't make it smaller, the rest of the message still goes
        let failed = sessions.send(prompt(vec![command(), text(" then look")])).await;
        assert!(failed.is_err());
        idle(&mut received).await;
        idle(&mut received).await;
        assert_eq!(last_user(&sessions), ["then look"]);

        // Stop and send from the queue: the compaction is cancelled, that is no error
        mock.hold.store(true, std::sync::atomic::Ordering::SeqCst);
        let message = prompt(vec![text("look "), command(), text(" now")]);
        let queued = message.message_id.clone();
        let running = tokio::spawn({
            let sessions = sessions.clone();
            async move { sessions.send(message).await }
        });
        tokio::time::timeout(Duration::from_secs(10), mock.started.notified()).await.unwrap();
        let shown: Vec<_> = sessions.queue(&session.id).iter().flat_map(|p| p.parts.iter().map(|part| matches!(part, PromptPart::Command { .. }))).collect();
        assert_eq!(shown, [false], "the queue shows the message without its command");
        sessions.interrupt(&session.id, &queued);
        let stopped = tokio::time::timeout(Duration::from_secs(10), running).await.unwrap().unwrap();
        mock.go.notify_one();
        assert!(stopped.is_ok());
        idle(&mut received).await;
        idle(&mut received).await;
        assert_eq!(last_user(&sessions), ["look now"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retries_that_run_out_leave_their_error_on_the_retry_row() {
        let root = std::env::temp_dir().join(format!("nabu-retry-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(64);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let user = UserMessage {
            id: id::ascending("msg"),
            session_id: session.id.clone(),
            time: nabu_protocol::Created { created: id::now() },
            agent: "build".into(),
            model: ModelRef { provider_id: "custom".into(), model_id: "m".into(), variant: None, fast: false },
        };
        let directory = root.to_string_lossy().into_owned();
        let mut turn = Turn::new(&sessions, &directory, &user);
        turn.show_retry(1, Some(6), "503: busy".into(), id::now());
        turn.show_retry(2, Some(6), "503: still busy".into(), id::now());
        turn.end(RunStop::Failed(Error::Config("still busy".into())));

        let history = sessions.messages(&session.id).unwrap();
        let [retry] = history.as_slice() else { panic!("one message, got {}", history.len()) };
        let Message::Assistant(info) = &retry.info else { panic!("an answer") };
        assert_eq!(info.error.as_ref().unwrap().data.message, "still busy");
        let [Part::Retry(part)] = retry.parts.as_slice() else { panic!("the retry row alone") };
        assert_eq!((part.attempt, part.max, part.error.as_str(), part.next), (2, Some(6), "503: still busy", None));

        let mut context = Vec::new();
        append_history(&mut context, &history, &user.model, &no_files());
        assert!(text_of(&context[0].content).contains("No response was generated. Error: still busy"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn context_parts_estimate_the_next_request_by_kind() {
        let root = std::env::temp_dir().join(format!("nabu-context-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(64);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        assert!(sessions.context_parts(&session.id).is_err(), "no message, so no model to ask for");
        let user = UserMessage {
            id: id::ascending("msg"),
            session_id: session.id.clone(),
            time: nabu_protocol::Created { created: id::now() },
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
        };
        let text = TextPart {
            id: id::ascending("prt"), session_id: session.id.clone(), message_id: user.id.clone(),
            text: "x".repeat(3_000), time: None, sig: None, synthetic: false,
        };
        let directory = root.to_string_lossy().into_owned();
        sessions.add_message(&directory, MessageWithParts { info: Message::User(user), parts: vec![Part::Text(text)] });

        let parts = sessions.context_parts(&session.id).unwrap();
        assert!((1_000..1_100).contains(&parts.messages), "{parts:?}");
        assert!(parts.system > 0 && parts.tools > 0 && parts.deferred > 0, "{parts:?}");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn skill_marks_stay_in_the_text_and_bring_their_skills_once() {
        let root = std::env::temp_dir().join(format!("nabu-skills-{}", id::ascending("t")));
        let skill = root.join(".nabucode").join("skills").join("review");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: review\ndescription: Reviews the diff\n---\nLook for bugs.").unwrap();
        let (events, _received) = broadcast::channel(64);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let text = |text: &str| PromptPart::Text { id: id::ascending("prt"), text: text.into() };
        let mark = |name: &str| PromptPart::Skill { id: id::ascending("prt"), name: name.into(), at: false };
        let command = PromptPart::Command { id: id::ascending("prt"), name: "compact".into() };
        let model = ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false };
        let prompt = |parts| Prompt { session_id: session.id.clone(), message_id: "msg_1".into(), agent: "build".into(), model: model.clone(), parts };

        let sent = prompt(vec![text("please "), mark("review"), command, text(" now")]);
        let marked = sessions.marked(&sent).unwrap();
        let parts: Vec<Part> = sent.parts.into_iter().filter_map(|p| super::super::part_of(p, &session.id, "msg_1", &marked)).collect();
        let blocks = user_message(&parts, &no_files());
        let texts: Vec<String> = blocks.iter().map(|b| match b { Block::Text { text, .. } => text.clone(), _ => String::new() }).collect();
        let dir = skill.display().to_string();
        assert_eq!(texts, [
            "please <skill name=\"review\"/> now".to_owned(),
            format!("<skill-content name=\"review\" dir=\"{dir}\">\nLook for bugs.\n</skill-content>"),
        ]);

        assert!(sessions.marked(&prompt(vec![mark("nope")])).err().unwrap().contains("unknown skill"));
        assert!(sessions.marked(&prompt(vec![mark("review"), mark("review")])).err().unwrap().contains("twice"));
        assert!(sessions.marked(&prompt((0..11).map(|_| mark("review")).collect())).is_err());

        let toolbox = Toolbox::new(&root, Arc::default()).with_skills(sessions.skills(&root.to_string_lossy()));
        let listed = toolbox.definitions().into_iter().any(|d| match d {
            ncore::core::ir::ToolDef::Function(t) => t.name == "skill" && t.description.unwrap_or_default().contains("- review: Reviews the diff"),
            _ => false,
        });
        assert!(listed, "the skill tool lists the project's skills");
        std::fs::remove_dir_all(root).ok();
    }

    /// The first reply stops halfway and never ends; the next one answers.
    #[derive(Default)]
    struct CutMock {
        inputs: Mutex<Vec<Value>>,
    }

    impl Transport for CutMock {
        fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                let once = |body: String| Box::pin(futures_util::stream::once(async move { Ok(body.into()) }));
                if request.url.ends_with("/v1/models") {
                    let body = json!({"data":[{"id":"gpt-6.1-sol"}]}).to_string();
                    return Ok(HttpResponse { status: 200, headers: Vec::new(), body: once(body) });
                }
                let request: Value = serde_json::from_slice(&request.body).unwrap();
                if request["stream"] != true {
                    let body = json!({"status":"completed","output":[{"content":[{"text":"OK"}]}]}).to_string();
                    return Ok(HttpResponse { status: 200, headers: Vec::new(), body: once(body) });
                }
                let first = {
                    let mut inputs = self.inputs.lock().unwrap();
                    inputs.push(request["input"].clone());
                    inputs.len() == 1
                };
                let body = if first {
                    let half = router_events([
                        json!({"type":"response.content_part.added"}),
                        json!({"type":"response.output_text.delta","delta":"Half a rep"}),
                    ]);
                    Box::pin(futures_util::stream::once(async move { Ok(half.into()) }).chain(futures_util::stream::pending())) as _
                } else {
                    let text = router_events([
                        json!({"type":"response.content_part.added"}),
                        json!({"type":"response.output_text.delta","delta":"Sure"}),
                        json!({"type":"response.content_part.done"}),
                        json!({"type":"response.completed","response":{"status":"completed"}}),
                    ]);
                    Box::pin(futures_util::stream::once(async move { Ok(text.into()) })) as _
                };
                Ok(HttpResponse { status: 200, headers: Vec::new(), body })
            })
        }
    }

    #[tokio::test]
    async fn interrupt_cuts_the_reply_and_hands_over_its_message_only() {
        let root = std::env::temp_dir().join(format!("nabu-cut-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, mut received) = broadcast::channel(256);
        let mock = Arc::new(CutMock::default());
        let sessions = sessions_over(&root, mock.clone(), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let prompt = |text: &str| Prompt {
            session_id: session.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
            parts: vec![PromptPart::Text { id: id::ascending("prt"), text: text.into() }],
        };

        sessions.prompt(prompt("Hi")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(received.recv().await.unwrap(), Event::PartDelta { delta, .. } if delta == "Half a rep") {}
        })
        .await
        .unwrap();
        // Nothing waits in the queue, so there is nothing to interrupt for
        sessions.interrupt(&session.id, "msg_none");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(mock.inputs.lock().unwrap().len(), 1);

        sessions.prompt(prompt("Later")).unwrap();
        let wait = prompt("Wait");
        let wait_id = wait.message_id.clone();
        sessions.prompt(wait).unwrap();
        sessions.interrupt(&session.id, &wait_id);
        // The message left in the queue starts the next turn once this one ends
        tokio::time::timeout(Duration::from_secs(10), async {
            while sessions.messages(&session.id).unwrap().len() < 6 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let history = sessions.messages(&session.id).unwrap();
        let texts: Vec<String> = history.iter().map(|message| {
            let text: Vec<&str> = message.parts.iter().filter_map(|part| match part { Part::Text(p) => Some(p.text.as_str()), _ => None }).collect();
            text.join(" ")
        }).collect();
        assert_eq!(texts, ["Hi", "Half a rep", &format!("{CUT_NOTE} Wait"), "Sure", "Later", "Sure"]);
        let Message::Assistant(cut) = &history[1].info else { panic!("expected the cut reply") };
        assert!(cut.error.is_none() && cut.time.completed.is_some());
        let Message::Assistant(answer) = &history[3].info else { panic!("expected the answer") };
        assert_eq!(answer.parent_id, history[2].info.id());
        let second = mock.inputs.lock().unwrap()[1].to_string();
        assert!(second.find("Half a rep").unwrap() < second.find("Wait").unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_session_is_never_nameless() {
        let root = std::env::temp_dir().join(format!("nabu-title-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(256);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        let title = |id: &str| sessions.get(id).unwrap().title;
        let named = sessions.create(root.to_string_lossy().into_owned());
        sessions.rename(&named.id, "  Logs ".into()).unwrap();
        assert_eq!(title(&named.id), "Logs");
        sessions.rename(&named.id, "   ".into()).unwrap();
        assert_eq!(title(&named.id), super::super::UNTITLED);

        let session = sessions.create(root.to_string_lossy().into_owned());
        assert_eq!(session.title, super::super::UNTITLED);
        let prompt = Prompt {
            session_id: session.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
            parts: vec![PromptPart::Text { id: id::ascending("prt"), text: "Read the logs".into() }],
        };
        sessions.prompt(prompt).unwrap();
        assert_eq!(title(&session.id), "Read the logs", "the first message names it");
        sessions.abort(&session.id);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn renaming_keeps_the_sessions_place() {
        let root = std::env::temp_dir().join(format!("nabu-rename-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(256);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        let session = sessions.create(root.to_string_lossy().into_owned());
        tokio::time::sleep(Duration::from_millis(5)).await;
        sessions.rename(&session.id, "Logs".into()).unwrap();
        assert_eq!(sessions.get(&session.id).unwrap().time.updated, session.time.updated);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn archiving_packs_the_chat_into_a_zip_and_brings_it_back() {
        let root = std::env::temp_dir().join(format!("nabu-archive-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(256);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events.clone()).await;
        let chat = sessions.create(root.to_string_lossy().into_owned());
        let prompt = Prompt {
            session_id: chat.id.clone(),
            message_id: id::ascending("msg"),
            agent: "build".into(),
            model: ModelRef { provider_id: "agentrouter".into(), model_id: "gpt-6.1-sol".into(), variant: None, fast: false },
            parts: vec![
                PromptPart::Text { id: id::ascending("prt"), text: "Look at the shot".into() },
                PromptPart::File { id: id::ascending("prt"), mime: "image/png".into(), filename: Some("shot.png".into()), url: "data:image/png;base64,AAAA".into() },
            ],
        };
        sessions.prompt(prompt).unwrap();
        let child = sessions.create_child(&chat.id, "Explore", None).unwrap();
        let kept = |id: &str| root.join("sessions").join(format!("{id}.json")).exists();
        let zip = root.join("archive").join(format!("{}.zip", chat.id));

        sessions.archive(&chat.id).unwrap();
        assert!(sessions.recent(50).is_empty(), "the answering chat is stopped and leaves the lists");
        assert!(zip.exists() && !kept(&chat.id) && !kept(&child.id) && !root.join("sessions").join(&chat.id).exists());

        std::fs::remove_file(root.join("archive").join("index.json")).unwrap();
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        sessions.unarchive(&chat.id).unwrap();
        assert_eq!(sessions.get(&chat.id).unwrap().title, "Look at the shot");
        assert_eq!(sessions.get(&child.id).unwrap().parent_id, Some(chat.id.clone()));
        assert_eq!(std::fs::read_dir(root.join("sessions").join(&chat.id)).unwrap().count(), 1, "the attachment is back");
        assert!(!zip.exists() && std::fs::read_to_string(root.join("archive").join("index.json")).unwrap().trim() == "[]");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn chats_only_marked_archived_come_back_to_the_lists() {
        let root = std::env::temp_dir().join(format!("nabu-marked-{}", id::ascending("t")));
        std::fs::create_dir_all(&root).unwrap();
        let (events, _received) = broadcast::channel(256);
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events.clone()).await;
        let chat = sessions.create(root.to_string_lossy().into_owned());
        sessions.edit(&chat.id, |s| s.time.archived = Some(1)).unwrap();
        let sessions = sessions_over(&root, Arc::new(CutMock::default()), events).await;
        assert_eq!(sessions.get(&chat.id).unwrap().time.archived, None);
        assert_eq!(sessions.recent(50).len(), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn agentrouter_upstream_busy_keeps_retrying_on_its_own_schedule() {
        let mut model = ModelRef {
            provider_id: "agentrouter".into(),
            model_id: "gpt-6.1-sol".into(),
            variant: None,
            fast: false,
        };
        let error = ncore::core::ProviderError::parse(
            ncore::Format::OpenAiResponses,
            Some(429),
            br#"{"error":{"message":"Your requests to gpt-6.1-sol for gpt-6.1-sol in eastus2 have exceeded token rate limit."}}"#[..].into(),
        );
        let stop = RunStop::Failed(Error::Provider(error));
        let delays: Vec<_> = (0..8)
            .map(|retries| retry_delay(&model, &stop, retries))
            .collect();
        assert_eq!(delays, [1, 1, 1, 1, 1, 5, 5, 5].map(|delay| Some((delay, None))));
        assert_eq!(retry_delay(&model, &stop, u32::MAX), Some((5, None)));

        model.provider_id = "coarena".into();
        assert_eq!(retry_delay(&model, &stop, 1), Some((2, Some(6))));
        assert_eq!(retry_delay(&model, &stop, 6), None);
    }

    #[test]
    fn agentrouter_retries_azure_resource_mismatch() {
        let model = ModelRef {
            provider_id: "agentrouter".into(),
            model_id: "gpt-6.1-sol".into(),
            variant: None,
            fast: false,
        };
        let custom = ModelRef {
            provider_id: "custom".into(),
            ..model.clone()
        };
        for (prefix, resource) in [("", "Azure"), ("OpenAI Responses bad request: ", "***")] {
            let message = format!(
                "{prefix}The requested item was created under a different {resource} OpenAI resource. \
                 Use the same resource that created the item to access it. [trace_id=test]"
            );
            let body = serde_json::json!({"error": {"message": message}});
            let error = ncore::core::ProviderError::parse(
                ncore::Format::OpenAiResponses,
                Some(400),
                body.to_string().into(),
            );
            assert_eq!(error.kind, ErrorKind::InvalidRequest);
            assert_eq!(
                error_of(Error::Provider(error.clone())).data.message,
                message
            );
            let stop = RunStop::Failed(Error::Provider(error));
            assert_eq!(retry_delay(&model, &stop, 0), Some((1, None)), "{message}");
            assert_eq!(retry_delay(&model, &stop, 4), Some((1, None)));
            assert_eq!(retry_delay(&model, &stop, 5), Some((5, None)));
            assert_eq!(retry_delay(&model, &stop, u32::MAX), Some((5, None)));
            assert_eq!(retry_delay(&custom, &stop, 0), None);
        }
    }

    #[test]
    fn deepai_never_retries_an_ambiguous_billed_failure() {
        let model = ModelRef {
            provider_id: "deepai-account".into(),
            model_id: "glm-5.3-flash".into(),
            variant: None,
            fast: false,
        };
        let transport = RunStop::Failed(Error::Transport(ncore::TransportError::new(
            "lost response",
        )));
        let provider = RunStop::Failed(Error::Provider(ncore::core::ProviderError::parse(
            ncore::Format::OpenAiChat,
            Some(503),
            br#"{"error":{"type":"overloaded_error","message":"unavailable"}}"#[..].into(),
        )));
        assert_eq!(retry_delay(&model, &transport, 0), None);
        assert_eq!(retry_delay(&model, &provider, 0), None);
        let busy = RunStop::Failed(Error::Provider(ncore::core::ProviderError::parse(
            ncore::Format::OpenAiChat,
            Some(409),
            br#"{"error":{"code":"request_failed","type":"deepai_error","message":"DeepAI request failed (HTTP 409)."}}"#[..].into(),
        )));
        assert_eq!(retry_delay(&model, &busy, 0), Some((2, Some(3))));
        assert_eq!(retry_delay(&model, &busy, 3), None);
        let other = ModelRef {
            provider_id: "workbuddy-account".into(),
            ..model.clone()
        };
        assert_eq!(retry_delay(&other, &transport, 0), Some((1, Some(6))));
    }

    #[test]
    fn plain_reasoning_only_replays_within_the_same_account_provider() {
        let model = ModelRef {
            provider_id: "deepai-account".into(),
            model_id: "glm-5.3".into(),
            variant: None,
            fast: false,
        };
        assert!(preserves_reasoning(&model, "deepai-account"));
        assert!(!preserves_reasoning(&model, "workbuddy-account"));
        let other = ModelRef {
            provider_id: "workbuddy-account".into(),
            ..model.clone()
        };
        assert!(preserves_reasoning(&other, "workbuddy-account"));
        assert!(!preserves_reasoning(&other, "deepai-account"));
    }

    #[test]
    fn deepai_turn_persists_reasoning_details_and_replays_them_after_reload() {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "nabu-deepai-history-{:x}",
            u64::from_ne_bytes(random)
        ));
        let server = crate::Server::new(&root);
        let sessions = &server.sessions;
        let session = sessions.create(root.to_string_lossy().into_owned());
        let model = ModelRef {
            provider_id: "deepai-account".into(),
            model_id: "glm-5.3-flash".into(),
            variant: None,
            fast: false,
        };
        let user = UserMessage {
            id: "user-fixture".into(),
            session_id: session.id.clone(),
            model: model.clone(),
            agent: "build".into(),
            time: nabu_protocol::Created { created: id::now() },
        };
        let mut turn = Turn::new(sessions, &session.directory, &user);
        turn.stream(StreamEvent::BlockStart {
            index: 0,
            block: BlockStart::Thinking { redacted: false },
        });
        turn.stream(StreamEvent::Delta {
            index: 0,
            delta: Delta::Thinking("plain reasoning".into()),
        });
        turn.stream(StreamEvent::BlockEnd { index: 0 });
        let details = json!([{"index":0,"type":"reasoning.encrypted","data":"synthetic-signature","format":"fixture"}]);
        turn.stream(StreamEvent::BlockStart {
            index: 1,
            block: BlockStart::Thinking { redacted: false },
        });
        turn.stream(StreamEvent::Delta {
            index: 1,
            delta: Delta::Sig(ir::Opaque::new(
                ncore::Format::OpenAiChat,
                json!({"reasoning_details":details}),
            )),
        });
        turn.stream(StreamEvent::BlockEnd { index: 1 });
        turn.stream(StreamEvent::BlockStart {
            index: 2,
            block: BlockStart::Text,
        });
        turn.stream(StreamEvent::Delta {
            index: 2,
            delta: Delta::Text("reply".into()),
        });
        turn.stream(StreamEvent::BlockEnd { index: 2 });
        turn.end(RunStop::Finished(Some(StopReason::EndTurn)));
        drop(server);
        let server = crate::Server::new(&root);
        let history = server.sessions.messages(&session.id).unwrap();
        let mut messages = vec![];
        append_history(&mut messages, &history, &model, &Attachments { dir: Default::default(), images: true });
        let request = ir::Request {
            model: model.model_id,
            messages,
            ..Default::default()
        };
        let encoded = ncore::core::codec::encode_request(
            ncore::Format::OpenAiChat,
            &request,
            &ncore_deepaiacc::DeepAIAcc::encode_options(),
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&encoded.body).unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["reasoning_content"], "plain reasoning");
        assert_eq!(assistant["reasoning_details"], details);
        assert_eq!(assistant["content"], "reply");
        drop(server);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workbuddy_history_keeps_plain_reasoning_without_changing_other_providers() {
        let parts = [Part::Reasoning(TextPart {
            id: "r".into(),
            session_id: "s".into(),
            message_id: "m".into(),
            text: "previous reasoning".into(),
            time: None,
            sig: None,
            synthetic: false,
        })];
        assert!(assistant_blocks(&parts, false, &no_files()).0.is_empty());
        let (blocks, _) = assistant_blocks(&parts, true, &no_files());
        assert!(matches!(&blocks[0],Block::Thinking(t) if t.text=="previous reasoning"));
    }
}
