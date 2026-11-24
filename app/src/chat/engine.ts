import { addMessage, addPart, appendText, chats, ensureChat, newId, setChat, updateMessage, updateTool, type Attachment, type Question, type ToolState } from "./model";

// Plays scripted replies with delays, the way the server streams a real one
export class Stopped extends Error {}
export class Failure extends Error {}

export const CANCELLED = "the call was cancelled by the user";
export const DISMISSED = "the user dismissed this question and stopped the turn; wait for their next message";

export type Script = (reply: Reply) => Promise<void>;
type Tool = { part: number };

const controllers = new Map<string, AbortController>();
const asks = new Map<string, { part: number; message: number; resolve: (answers: string[][]) => void }>();

export class Reply {

  constructor(readonly chatId: string, readonly message: number, private signal: AbortSignal, readonly instant: boolean) {}

  wait(ms: number) {
    if (this.instant) return Promise.resolve();
    return new Promise<void>((resolve, reject) => {
      if (this.signal.aborted) return reject(new Stopped());
      const timer = setTimeout(() => { this.signal.removeEventListener("abort", stop); resolve(); }, ms);
      const stop = () => { clearTimeout(timer); reject(new Stopped()); };
      this.signal.addEventListener("abort", stop, { once: true });
    });
  }

  // Chunks of uneven size, like tokens arriving from a provider
  async stream(kind: "text" | "reasoning", text: string, pace = 18) {
    if (this.instant) {
      addPart(this.chatId, this.message, { id: newId("part"), type: kind, text });
      return;
    }
    let shown = Math.min(text.length, 2 + Math.floor(Math.random() * 6));
    const part = addPart(this.chatId, this.message, { id: newId("part"), type: kind, text: text.slice(0, shown) });
    while (shown < text.length) {
      await this.wait(pace + Math.random() * pace);
      const size = 2 + Math.floor(Math.random() * 10);
      appendText(this.chatId, this.message, part, text.slice(shown, shown + size));
      shown += size;
    }
  }

  say(text: string, pace?: number) { return this.stream("text", text, pace); }
  think(text: string, pace = 10) { return this.stream("reasoning", text, pace); }

  tool(name: string, input: Record<string, any>, metadata?: Record<string, any>): Tool {
    const part = addPart(this.chatId, this.message, { id: newId("part"), type: "tool", tool: name, state: { status: "running", input, metadata } });
    return { part };
  }

  done(tool: Tool, output = "", metadata?: Record<string, any>) {
    const state: Partial<ToolState> = { status: "completed", output };
    if (metadata) state.metadata = { ...this.state(tool).metadata, ...metadata };
    updateTool(this.chatId, this.message, tool.part, state);
  }

  fail(tool: Tool, error: string) {
    updateTool(this.chatId, this.message, tool.part, { status: "error", error });
  }

  async run(name: string, input: Record<string, any>, ms: number, output = "", metadata?: Record<string, any>) {
    const tool = this.tool(name, input);
    await this.wait(ms);
    this.done(tool, output, metadata);
  }

  async ask(questions: Question[]) {
    const tool = this.tool("ask", { questions });
    const part = chats[this.chatId].messages[this.message].parts[tool.part];
    if (this.instant) {
      const answers = questions.map(question => [question.options[0].label]);
      this.done(tool, "", { answers });
      return answers;
    }
    setChat(this.chatId, { ask: { partId: part.id, questions } });
    const answers = await new Promise<string[][]>(resolve => asks.set(this.chatId, { part: tool.part, message: this.message, resolve }));
    if (this.signal.aborted) throw new Stopped();
    return answers;
  }

  // Runs the script in its own chat, which the agent card opens
  async agent(input: { description: string; prompt: string; subagent_type?: string }, script: Script) {
    const childId = `${this.chatId}-${newId("sub")}`;
    ensureChat(childId, input.description, { parentId: this.chatId, agent: input.subagent_type });
    const tool = this.tool("agent", input, { sessionId: childId });
    const model = chats[this.chatId].messages[this.message].model;
    const report = await play(childId, input.prompt, script, { instant: this.instant, parent: this.signal, model });
    if (report.kind === "failed") return this.fail(tool, report.text);
    if (this.signal.aborted) throw new Stopped();
    // Only the agent was stopped: its card says so and the parent goes on
    if (report.kind === "aborted") return this.fail(tool, CANCELLED);
    this.done(tool, report.text);
  }

  private state(tool: Tool) {
    const part = chats[this.chatId].messages[this.message].parts[tool.part];
    return part.type === "tool" ? part.state : { metadata: undefined };
  }
}

type Report = { kind: "done" | "aborted" | "failed"; text: string };

export async function play(chatId: string, text: string, script: Script, options: { instant?: boolean; parent?: AbortSignal; model?: string; attachments?: Attachment[] } = {}): Promise<Report> {
  const controller = new AbortController();
  const stopChild = () => controller.abort();
  options.parent?.addEventListener("abort", stopChild, { once: true });
  controllers.set(chatId, controller);
  const created = Date.now();
  addMessage(chatId, { id: newId("msg"), role: "user", text, attachments: options.attachments, parts: [], created });
  const message = addMessage(chatId, { id: newId("msg"), role: "assistant", parts: [], created, model: options.model });
  setChat(chatId, { busy: true });
  const reply = new Reply(chatId, message, controller.signal, !!options.instant);
  let report: Report = { kind: "done", text: "" };
  try {
    await script(reply);
  } catch (cause) {
    report = cause instanceof Failure ? { kind: "failed", text: cause.message } : { kind: "aborted", text: "" };
    if (!(cause instanceof Failure) && !(cause instanceof Stopped)) console.error(cause);
  }
  finish(chatId, message, report);
  options.parent?.removeEventListener("abort", stopChild);
  if (controllers.get(chatId) === controller) controllers.delete(chatId);
  if (report.kind === "done") report.text = lastText(chatId, message);
  return report;
}

function finish(chatId: string, message: number, report: Report) {
  const parts = chats[chatId].messages[message].parts;
  parts.forEach((part, index) => {
    if (part.type === "tool" && (part.state.status === "running" || part.state.status === "pending")) updateTool(chatId, message, index, { status: "error", error: CANCELLED });
  });
  const error = report.kind === "done" ? undefined : { kind: report.kind, message: report.text || undefined } as const;
  updateMessage(chatId, message, { completed: Date.now(), error });
  setChat(chatId, { busy: false, ask: undefined });
  asks.delete(chatId);
}

function lastText(chatId: string, message: number) {
  const part = chats[chatId].messages[message].parts.findLast(item => item.type === "text");
  return part?.type === "text" ? part.text : "";
}

export function stop(chatId: string) {
  controllers.get(chatId)?.abort();
}

export function answer(chatId: string, answers: string[][]) {
  const pending = asks.get(chatId);
  if (!pending) return;
  asks.delete(chatId);
  updateTool(chatId, pending.message, pending.part, { status: "completed", output: "", metadata: { answers } });
  setChat(chatId, { ask: undefined });
  pending.resolve(answers);
}

export function dismiss(chatId: string) {
  const pending = asks.get(chatId);
  if (!pending) return;
  asks.delete(chatId);
  updateTool(chatId, pending.message, pending.part, { status: "error", error: DISMISSED });
  setChat(chatId, { ask: undefined });
  stop(chatId);
  pending.resolve([]);
}
