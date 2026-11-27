import { batch } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";
import type { AssistantMessage, FilePart, SessionStatus, Message as WireMessage, MessageWithParts, ModelRef, Part as WirePart, Prompt, QuestionRequest, Session as WireSession } from "@nabu/protocol";
import { fileUrl, onConnected, onServerEvent, request, uploadFile } from "../server";
import { iconOf } from "../attachments";
import { markOf, piecesOf, plainText, promptParts, type Piece } from "../marks";
import { appendPartText, chats, ensureChat, putMessage, putMessages, putPart, removeMessage, setChat, type Attachment, type Message, type Part } from "./model";

// Chats filled from the server: history by pages, then its events; the timeline reads the same store as the demo
const pageSize = 30;

type Sessions = Record<string, WireSession>;
const [sessions, setSessions] = createStore<Sessions>({});
const [busy, setBusy] = createStore<Record<string, boolean>>({});
/** Messages sent while their session answers, oldest first. */
const [queues, setQueues] = createStore<Record<string, Prompt[]>>({});
export { sessions, busy, queues };

const loaded = new Set<string>();
let directories: string[] = [];
let modelName: (ref: { providerID: string; modelID: string }) => string | undefined = () => undefined;

/** The names the assistant messages show come from the model catalog. */
export function nameModels(lookup: typeof modelName) {
  modelName = lookup;
}

/** The server keys sessions by the folder written with forward slashes, as the old GUI sends it. */
export const serverPath = (path: string) => path.replace(/\\/g, "/");

/** The project folders whose sessions are listed; asks the server for the new ones. */
export function watchDirectories(paths: string[]) {
  const next = paths.map(serverPath);
  const added = next.filter(directory => !directories.includes(directory));
  directories = next;
  added.forEach(directory => void loadDirectory(directory));
}

async function loadDirectory(directory: string) {
  const [list, statuses, questions] = await Promise.all([
    request("sessionList", { directory }),
    request("sessionStatuses", { directory }),
    request("questionList", { directory }),
  ]).catch(() => [[], {} as Record<string, SessionStatus>, []] as const);
  batch(() => {
    setSessions(produce(all => {
      for (const [id, session] of Object.entries(all)) if (session.directory === directory) delete all[id];
      for (const session of list) all[session.id] = session;
    }));
    for (const session of list) markBusy(session.id, !!statuses[session.id] && statuses[session.id].type !== "idle");
    for (const question of questions) showQuestion(question);
  });
}

/** Sessions of every folder for «Недавние»; events keep them current afterwards. */
export const recentCount = 50;
async function loadRecent() {
  const list = await request("sessionRecent", { limit: recentCount }).catch(() => []);
  setSessions(produce(all => { for (const session of list) all[session.id] = session; }));
}

/** Opens a chat of a session, loading its newest page the first time. */
export function openChat(id: string) {
  const session = sessions[id];
  ensureChat(id, session?.title ?? "", { parentId: session?.parentID, busy: !!busy[id] });
  if (loaded.has(id)) return;
  loaded.add(id);
  void loadPage(id);
  void loadQueue(id);
}

async function loadQueue(id: string) {
  const queue = await request("sessionQueue", { sessionID: id }).catch(() => undefined);
  if (queue) setQueues(id, reconcile(queue, { key: "messageID" }));
}

// keepView is called right before the page lands and its result right after, while nothing is painted yet
async function loadPage(id: string, before?: string, keepView?: () => () => void) {
  const page = await request("sessionPage", { sessionID: id, before, limit: pageSize }).catch(() => undefined);
  if (!page) return;
  const restore = keepView?.();
  batch(() => {
    putMessages(id, page.messages.map(toMessage));
    if (!before || !page.more) setChat(id, { more: page.more });
  });
  restore?.();
}

let olderLoading: Promise<void> | undefined;
export function loadOlder(id: string, keepView: () => () => void) {
  const first = chats[id]?.messages[0];
  if (!chats[id]?.more || !first || olderLoading) return;
  olderLoading = loadPage(id, first.id, keepView).finally(() => { olderLoading = undefined; });
}

// The server answers with `sessionUpdated` / `sessionDeleted`, which update the lists
export const renameSession = (id: string, title: string) => request("sessionRename", { sessionID: id, title });
export const deleteSession = (id: string) => request("sessionDelete", { sessionID: id });
export const archiveSession = (id: string) => request("sessionArchive", { sessionID: id });
/** The queued message goes to the model now: the reply being written is cut off, running tools wrap up; the rest keeps waiting. */
export const interruptSession = (id: string, messageId: string) => request("sessionInterrupt", { sessionID: id, messageID: messageId });

export async function createSession(directory: string, options?: { temporary?: boolean; tools?: string[]; chrome?: string }) {
  const session = await request("sessionCreate", { directory: serverPath(directory), ...options });
  setSessions(session.id, session);
  loaded.add(session.id);
  ensureChat(session.id, session.title);
  return session;
}

/** The user's message shows at once with its own id; the server's events then fill and confirm it. While the session answers it goes to the queue instead. */
export async function send(sessionId: string, input: { pieces: Piece[]; files: Attachment[]; model: ModelRef; queuedId?: string }) {
  const queued = !!busy[sessionId];
  // A message taken back from the queue keeps its id: the queue is ordered by it
  const messageId = queued && input.queuedId || ascendingId("msg");
  const marked = promptParts(input.pieces, () => ascendingId("prt"));
  const files = input.files.map(file => ({ id: ascendingId("prt"), file }));
  // Shown under the ids of their parts, so the server's copies replace them instead of adding to them
  const attachments = files.map(item => ({ ...item.file, id: item.id }));
  // A command runs before the message: what is left of it comes from the server once it waits in the queue
  const command = marked.some(part => part.type === "command");
  if (!queued) batch(() => {
    if (!command) {
      putMessage(sessionId, { id: messageId, role: "user", text: plainText(piecesOf(marked)), attachments, created: Date.now(), sent: input.model });
      for (const part of marked) if (part.type !== "file") putPart(sessionId, messageId, toPart(part));
    }
    markBusy(sessionId, true);
  });
  try {
    // Local files go by path, the rest is uploaded first
    const fileParts = await Promise.all(files.map(async item => ({
      type: "file" as const, id: item.id, mime: item.file.mime || "application/octet-stream", filename: item.file.name,
      url: item.file.path ? localUrl(item.file.path) : await uploadFile(sessionId, item.id, item.file.file!),
    })));
    const parts: Prompt["parts"] = [...marked, ...fileParts];
    await request("sessionPrompt", { sessionID: sessionId, messageID: messageId, agent: "build", model: input.model, parts });
  } catch (cause) {
    if (!queued) batch(() => {
      removeMessage(sessionId, messageId);
      markBusy(sessionId, false);
    });
    throw cause;
  }
}

/** Takes a message back out of the queue; undefined when it already went to the model. */
export async function unqueue(sessionId: string, messageId: string) {
  const prompt = queues[sessionId]?.find(item => item.messageID === messageId);
  const removed = await request("sessionDequeue", { sessionID: sessionId, messageID: messageId }).then(() => true, () => false);
  return removed ? prompt : undefined;
}

export const abort = (id: string) => void request("sessionAbort", { sessionID: id }).catch(() => {});

export function answer(id: string, answers: string[][]) {
  const ask = chats[id]?.ask;
  if (ask?.requestId) void request("questionReply", { requestID: ask.requestId, answers }).catch(() => {});
}

export function dismiss(id: string) {
  const ask = chats[id]?.ask;
  if (ask?.requestId) void request("questionReject", { requestID: ask.requestId }).catch(() => {});
}

function markBusy(id: string, value: boolean) {
  setBusy(id, value);
  if (chats[id]) setChat(id, { busy: value });
}

function showQuestion(question: QuestionRequest) {
  if (!chats[question.sessionID]) return;
  const part = chats[question.sessionID].messages.flatMap(message => message.parts).find(item => item.type === "tool" && item.callId === question.tool.callID);
  setChat(question.sessionID, { ask: { partId: part?.id ?? "", questions: question.questions, requestId: question.id } });
}

function toMessage(entry: MessageWithParts): Message {
  const message: Message = { ...messageInfo(entry.info), parts: [] };
  for (const part of entry.parts) applyPart(message, part);
  return message;
}

function messageInfo(info: WireMessage): Omit<Message, "parts"> {
  if (info.role === "user") return { id: info.id, role: "user", created: info.time.created, sent: info.model };
  const error = info.error && (info.error.name === "MessageAbortedError" ? { kind: "aborted" as const } : { kind: "failed" as const, message: info.error.data.message });
  return {
    id: info.id, role: "assistant", created: info.time.created, completed: info.time.completed,
    model: modelName(info) ?? info.modelID, error, usage: usageOf(info),
  };
}

// What the model saw on this step plus what it wrote: the context the next step starts from
function usageOf(info: AssistantMessage) {
  const { input, output, reasoning, cache } = info.tokens;
  const tokens = input + output + reasoning + cache.read + cache.write;
  return tokens > 0 ? { tokens, model: `${info.providerID}/${info.modelID}` } : undefined;
}

// A user message shows its text parts and marks as its text and its files as attachments; an assistant's parts as they are
function applyPart(message: Message, part: WirePart) {
  if (part.type === "text" && part.synthetic) return;
  if (part.type === "file") {
    const file = toAttachment(part);
    message.attachments = [...(message.attachments ?? []).filter(item => item.id !== file.id), file];
    return;
  }
  const converted = toPart(part);
  const index = message.parts.findIndex(item => item.id === converted.id);
  if (index >= 0) message.parts[index] = converted;
  else message.parts.push(converted);
  if (message.role === "user") message.text = plainText(piecesOf(message.parts));
}

function toPart(part: Exclude<WirePart, FilePart> | Exclude<Prompt["parts"][number], { type: "file" }>): Part {
  if (part.type === "skill" || part.type === "command") return { id: part.id, type: "mark", mark: markOf(part) };
  if (part.type === "retry") return { id: part.id, type: "retry", attempt: part.attempt, max: part.max, error: part.error, next: part.next };
  if (part.type !== "tool") return { id: part.id, type: part.type, text: part.text };
  const state = part.state;
  return {
    id: part.id, type: "tool", tool: part.tool, callId: part.callID,
    state: {
      status: state.status, input: state.input,
      output: state.status === "completed" ? state.output : undefined,
      images: state.status === "completed" ? state.attachments?.filter(file => file.mime.startsWith("image/")).map(file => fileUrl(file.url)) : undefined,
      error: state.status === "error" ? state.error : undefined,
      metadata: state.status === "completed" || state.status === "running" ? state.metadata : undefined,
    },
  };
}

function toAttachment(part: FilePart): Attachment {
  const name = part.filename ?? part.id;
  const url = fileUrl(part.url);
  const size = part.url.startsWith("data:") ? Math.floor((part.url.length - part.url.indexOf(",") - 1) * 3 / 4) : undefined;
  // A file too big to copy stays on the disk by its path: the page can't show it
  const shown = part.mime.startsWith("image/") && !part.url.startsWith("file://");
  return { id: part.id, name, size, mime: part.mime, icon: iconOf(name, part.mime), image: shown ? url : undefined };
}

function updatePart(sessionId: string, messageId: string, part: WirePart) {
  const message = chats[sessionId]?.messages.find(item => item.id === messageId);
  if (!message) return;
  if (part.type === "file" || message.role === "user") {
    const copy: Message = { ...message, parts: [...message.parts], attachments: message.attachments };
    applyPart(copy, part);
    putMessage(sessionId, { id: copy.id, role: copy.role, created: copy.created, text: copy.text, attachments: copy.attachments });
    if (part.type !== "file") putPart(sessionId, messageId, toPart(part));
    return;
  }
  const converted = toPart(part);
  putPart(sessionId, messageId, converted);
  // The agent card opens the subagent's chat, which takes its name and type from the call
  if (converted.type === "tool" && converted.tool === "agent" && typeof converted.state.metadata?.sessionId === "string") {
    const input = converted.state.input;
    ensureChat(converted.state.metadata.sessionId, String(input.description ?? ""), { parentId: sessionId, agent: input.subagent_type });
  }
}

onServerEvent(event => {
  switch (event.type) {
    case "sessionUpdated":
      setSessions(event.session.id, event.session);
      if (chats[event.session.id]) setChat(event.session.id, { title: event.session.title });
      break;
    case "sessionDeleted":
      setSessions(produce(all => { delete all[event.session.id]; }));
      break;
    case "sessionStatus":
      markBusy(event.sessionID, event.status.type !== "idle");
      break;
    case "messageUpdated": {
      const id = event.message.sessionID;
      if (chats[id]) putMessage(id, messageInfo(event.message));
      break;
    }
    case "queueUpdated":
      batch(() => {
        setQueues(event.sessionID, reconcile(event.queue, { key: "messageID" }));
        // A message sent just as the answer started was shown in the chat, but the server queued it
        if (chats[event.sessionID]) for (const prompt of event.queue) removeMessage(event.sessionID, prompt.messageID);
      });
      break;
    case "partUpdated":
      updatePart(event.part.sessionID, event.part.messageID, event.part);
      break;
    case "partDelta": {
      const id = Object.keys(chats).find(chat => chats[chat].messages.some(message => message.id === event.messageID));
      if (id) appendPartText(id, event.messageID, event.partID, event.delta);
      break;
    }
    case "questionAsked":
      showQuestion(event.request);
      break;
    case "questionClosed":
      if (chats[event.sessionID]?.ask?.requestId === event.requestID) setChat(event.sessionID, { ask: undefined });
      break;
  }
});

// After a reconnection anything may have changed: lists, statuses and the open chats are read again
onConnected(() => {
  void loadRecent();
  directories.forEach(directory => void loadDirectory(directory));
  for (const id of loaded) {
    void loadPage(id);
    void loadQueue(id);
  }
});

let lastStamp = 0;
let counter = 0;
// Same scheme as the server's ids: 6 bytes of time and counter, then a random tail; they sort by creation
function ascendingId(prefix: string) {
  const now = Date.now();
  if (now !== lastStamp) { lastStamp = now; counter = 0; }
  const stamp = BigInt(now) * 0x1000n + BigInt(++counter);
  const hex = (stamp & 0xffffffffffffn).toString(16).padStart(12, "0");
  const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  const tail = Array.from(crypto.getRandomValues(new Uint8Array(14)), byte => chars[byte % chars.length]).join("");
  return `${prefix}_${hex}${tail}`;
}

// Local files and folders go by path, the server copies files itself
const localUrl = (path: string) => `file://${path.startsWith("/") ? "" : "/"}${encodeURI(path.replace(/\\/g, "/"))}`;
