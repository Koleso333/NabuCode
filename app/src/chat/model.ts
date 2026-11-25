import { createStore, produce } from "solid-js/store";
import type { ModelRef } from "@nabu/protocol";
import type { Mark } from "../marks";

// Same shapes as the server protocol, so the timeline will not change when the server is plugged in
export type ToolStatus = "pending" | "running" | "completed" | "error";
export type ToolState = { status: ToolStatus; input: Record<string, any>; output?: string; error?: string; metadata?: Record<string, any> };
export type TextPart = { id: string; type: "text" | "reasoning"; text: string };
export type ToolPart = { id: string; type: "tool"; tool: string; state: ToolState; callId?: string };
/** Repeats of a failed request: `max` is absent when unlimited, `next` is when the next attempt goes. */
export type RetryPart = { id: string; type: "retry"; attempt: number; max?: number; error: string; next?: number };
/** A skill or command the person marked in their message. */
export type MarkPart = { id: string; type: "mark"; mark: Mark };
export type Part = TextPart | ToolPart | RetryPart | MarkPart;

// image: a URL of the picture itself; frame: the first frame of a video. Files from the server have no File and maybe no size
/** `path`: a folder, sent as its place on disk rather than copied. */
export type Attachment = { id: string; file?: File; path?: string; name: string; mime?: string; size?: number; icon: string; image?: string; frame?: string; duration?: number };

export type Message = {
  id: string;
  role: "user" | "assistant";
  text?: string;
  attachments?: Attachment[];
  parts: Part[];
  created: number;
  completed?: number;
  model?: string;
  /** A user message: the model, effort and fast mode it went with. */
  sent?: ModelRef;
  /** An answer: the context it filled, and its model as `provider/model`. */
  usage?: { tokens: number; model: string };
  error?: { kind: "aborted" | "failed"; message?: string };
};

export type Question = { question: string; header: string; options: { label: string; description: string }[]; multiple?: boolean };
export type PendingAsk = { partId: string; questions: Question[]; requestId?: string };

export type Chat = {
  id: string;
  title: string;
  parentId?: string;
  agent?: string;
  messages: Message[];
  busy: boolean;
  ask?: PendingAsk;
  /** Older messages are still on the server. */
  more?: boolean;
};

export type FileChange = { filePath: string; relativePath: string; type: "add" | "update" | "delete"; patch: string; additions: number; deletions: number };

const [chats, setChats] = createStore<Record<string, Chat>>({});
export { chats };

// Started from the clock: saved drafts keep their files' ids across restarts
let lastId = Date.now();
export const newId = (prefix: string) => `${prefix}-${++lastId}`;

export function ensureChat(id: string, title: string, extra?: Partial<Chat>) {
  if (!chats[id]) setChats(id, { id, title, messages: [], busy: false, ...extra });
}

export function addMessage(chatId: string, message: Message) {
  setChats(chatId, "messages", produce(list => { list.push(message); }));
  return chats[chatId].messages.length - 1;
}

export function addPart(chatId: string, message: number, part: Part) {
  setChats(chatId, "messages", message, "parts", produce(list => { list.push(part); }));
  return chats[chatId].messages[message].parts.length - 1;
}

export function appendText(chatId: string, message: number, part: number, chunk: string) {
  setChats(chatId, "messages", message, "parts", part, "text" as never, ((text: string) => text + chunk) as never);
}

export function updateTool(chatId: string, message: number, part: number, state: Partial<ToolState>) {
  setChats(chatId, "messages", message, "parts", part, "state" as never, ((old: ToolState) => ({ ...old, ...state })) as never);
}

export function updateMessage(chatId: string, message: number, patch: Partial<Message>) {
  setChats(chatId, "messages", message, patch);
}

// By id, for chats the server fills: a message keeps its place by id, which the server makes ascending
export function putMessage(chatId: string, message: Omit<Message, "parts">) {
  setChats(chatId, "messages", produce(list => {
    const index = list.findIndex(item => item.id === message.id);
    if (index >= 0) return void Object.assign(list[index], message);
    const after = list.findIndex(item => item.id > message.id);
    const added = { ...message, parts: [] };
    if (after < 0) list.push(added);
    else list.splice(after, 0, added);
  }));
}

export function putMessages(chatId: string, messages: Message[]) {
  setChats(chatId, "messages", produce(list => {
    for (const message of messages) {
      const index = list.findIndex(item => item.id === message.id);
      if (index >= 0) list[index] = message;
      else list.push(message);
    }
    list.sort((a, b) => a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
  }));
}

export function removeMessage(chatId: string, messageId: string) {
  setChats(chatId, "messages", list => list.filter(item => item.id !== messageId));
}

export function putPart(chatId: string, messageId: string, part: Part) {
  const message = chats[chatId]?.messages.findIndex(item => item.id === messageId) ?? -1;
  if (message < 0) return;
  setChats(chatId, "messages", message, "parts", produce(list => {
    const index = list.findIndex(item => item.id === part.id);
    if (index >= 0) list[index] = part;
    else list.push(part);
  }));
}

export function appendPartText(chatId: string, messageId: string, partId: string, delta: string) {
  const message = chats[chatId]?.messages.findIndex(item => item.id === messageId) ?? -1;
  const part = message < 0 ? -1 : chats[chatId].messages[message].parts.findIndex(item => item.id === partId);
  if (part >= 0) appendText(chatId, message, part, delta);
}

export function setChat(chatId: string, patch: Partial<Chat>) {
  setChats(chatId, patch);
}

export const isRunning = (part: Part) => part.type === "tool" && (part.state.status === "pending" || part.state.status === "running");
