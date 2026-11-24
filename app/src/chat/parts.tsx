import { createMemo, createSignal, For, Match, Show, Switch, type JSX } from "solid-js";
import { createStore } from "solid-js/store";
import { Glyph } from "../controls";
import { tr } from "../i18n";
import { preferences } from "../preferences";
import { copyText } from "./copy";
import { CodeLines, Diff, DiffCounts } from "./diff";
import { escapeHtml, highlight, isLoaded } from "./highlight";
import { CANCELLED, DISMISSED } from "./engine";
import { isRunning, type FileChange, type ToolPart } from "./model";

// Open state outlives unmounts: switching chats and back keeps what was expanded
const [opened, setOpened] = createStore<Record<string, boolean>>({});
export const isOpen = (key: string, fallback = false) => opened[key] ?? fallback;
export const toggleOpen = (key: string, fallback = false) => setOpened(key, !isOpen(key, fallback));

export function Chevron(props: { open: boolean }) {
  return <span class="chevron" classList={{ open: props.open }}><Glyph name="chevron-right" size={14} /></span>;
}

// Children mount only while open, so closed tools cost nothing
export function Collapse(props: { id: string; head: JSX.Element; children?: JSX.Element; defaultOpen?: boolean; enabled?: boolean; class?: string; classList?: Record<string, boolean> }) {
  const enabled = () => props.enabled !== false;
  const open = () => enabled() && isOpen(props.id, props.defaultOpen);
  return <div class={`collapse ${props.class ?? ""}`} classList={props.classList}>
    <button type="button" class="collapse-head" aria-expanded={open()} disabled={!enabled()} onClick={() => toggleOpen(props.id, props.defaultOpen)}>
      {props.head}
      <Show when={enabled()}><Chevron open={open()} /></Show>
    </button>
    <Show when={open()}><div class="collapse-body">{props.children}</div></Show>
  </div>;
}

export function Shimmer(props: { text: string; active?: boolean }) {
  return <span class="shimmer" classList={{ active: props.active }}>{props.text}</span>;
}

export type ToolLabel = { title: string; subtitle?: string; args?: string[]; mono?: boolean };

export function ToolTitle(props: { label: ToolLabel; running?: boolean }) {
  return <span class="tool-label">
    <span class="tool-title"><Shimmer text={props.label.title} active={props.running} /></span>
    <Show when={!props.running && props.label.subtitle}><span class="tool-subtitle" classList={{ mono: props.label.mono }}>{props.label.subtitle}</span></Show>
    <Show when={!props.running}><For each={props.label.args}>{arg => <span class="tool-arg">{arg}</span>}</For></Show>
  </span>;
}

export function CopyButton(props: { text: () => string; label: string; class?: string }) {
  const [copied, setCopied] = createSignal(false);
  const copy = async () => {
    if (!await copyText(props.text())) return;
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };
  return <button type="button" class={`copy-button ${props.class ?? ""}`} aria-label={copied() ? tr().chat.copied : props.label} title={copied() ? tr().chat.copied : props.label}
    onMouseDown={event => event.preventDefault()} onClick={() => void copy()}><Glyph name={copied() ? "check" : "copy"} size={15} /></button>;
}

export const fileName = (path: string) => path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
export const folderOf = (path: string) => path.split(/[\\/]/).slice(0, -1).join("/");
const oneLine = (value: unknown, separator = " · ") => typeof value === "string" ? value : Array.isArray(value) ? value.filter(item => typeof item === "string").join(separator) : "";

export function shellSubtitle(input: Record<string, any>) {
  if (typeof input.description === "string" && input.description) return input.description;
  return typeof input.command === "string" ? input.command.trim().split("\n")[0] : "";
}

export function toolLabel(part: ToolPart): ToolLabel {
  const input = part.state.input;
  const title = tr().chat.tool[part.tool] ?? part.tool;
  const args = (...entries: [string, unknown][]) => entries.filter(([, value]) => value !== undefined && value !== "").map(([key, value]) => `${key}=${value}`);
  switch (part.tool) {
    case "read": return { title, subtitle: input.path ? fileName(input.path) : "", args: args(["offset", input.offset], ["limit", input.limit]) };
    case "list": return { title, subtitle: input.path || "/" };
    case "glob": return { title, subtitle: input.path || "/", args: args(["pattern", input.pattern]) };
    case "grep": return { title, subtitle: input.path || "/", args: args(["pattern", input.pattern], ["include", input.include]) };
    case "fetch": return { title, subtitle: input.url };
    case "search": return { title, subtitle: oneLine(input.query) };
    case "tools": return { title, subtitle: oneLine(input.load, ", ") };
    case "bash": case "powershell": return { title: tr().chat.tool.shell, subtitle: shellSubtitle(input), mono: !input.description, args: input.background ? [tr().chat.background] : [] };
    case "task": return { title, subtitle: input.id };
    case "edit": case "write": {
      const files = changedFiles(part);
      return { title, subtitle: files.length > 1 ? tr().chat.files(files.length) : fileName(files[0]?.relativePath ?? input.path ?? "") };
    }
    case "agent": return { title: agentName(input.subagent_type), subtitle: input.description };
    default: return { title };
  }
}

export const agentName = (type: unknown) => typeof type === "string" && type ? `${tr().chat.tool.agent} ${type[0].toUpperCase()}${type.slice(1)}` : tr().chat.tool.agent;

const agentColors = ["var(--icon-agent-build-base)", "var(--icon-agent-plan-base)", "var(--icon-agent-docs-base)", "var(--icon-agent-ask-base)", "var(--syntax-type)", "var(--syntax-string)"];
export function agentColor(name: string) {
  let hash = 0;
  for (const char of name) hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  return agentColors[hash % agentColors.length];
}

export const changedFiles = (part: ToolPart): FileChange[] => Array.isArray(part.state.metadata?.files) ? part.state.metadata.files : [];
export const diffTotal = (files: FileChange[]) => files.reduce((sum, file) => ({ additions: sum.additions + file.additions, deletions: sum.deletions + file.deletions }), { additions: 0, deletions: 0 });
export const stripAnsi = (text: string) => text.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "").replace(/\r\n?/g, "\n");

export function Output(props: { text: string; copy?: boolean }) {
  return <div class="tool-output">
    <Show when={props.copy}><CopyButton text={() => props.text} label={tr().chat.copy} class="output-copy" /></Show>
    <div class="tool-scroll"><pre><code>{props.text}</code></pre></div>
  </div>;
}

export function FilePanel(props: { file: FileChange; id: string; defaultOpen?: boolean }) {
  const fallback = () => props.defaultOpen ?? props.file.type !== "delete";
  const change = () => props.file.type === "add" ? tr().chat.created : props.file.type === "delete" ? tr().chat.deleted : undefined;
  return <div class="file-panel">
    <button type="button" class="file-head" aria-expanded={isOpen(props.id, fallback())} onClick={() => toggleOpen(props.id, fallback())}>
      <span class="file-path"><Show when={folderOf(props.file.relativePath)}><span class="file-folder">{`‪${folderOf(props.file.relativePath)}/‬`}</span></Show>{fileName(props.file.relativePath)}</span>
      <Show when={change()} fallback={<DiffCounts additions={props.file.additions} deletions={props.file.deletions} />}><span class="file-change" data-type={props.file.type}>{change()}</span></Show>
    </button>
    <Show when={isOpen(props.id, fallback())}><Diff patch={props.file.patch} path={props.file.relativePath} /></Show>
  </div>;
}

function ReadPanel(props: { path: string; text: string }) {
  const range = () => {
    const numbers = [...props.text.matchAll(/^(\d+)\t/gm)].map(match => Number(match[1]));
    return numbers.length ? tr().chat.lines(numbers[0], numbers.at(-1)!) : "";
  };
  return <div class="file-panel">
    <div class="file-head static">
      <span class="file-path"><Show when={folderOf(props.path)}><span class="file-folder">{`‪${folderOf(props.path)}/‬`}</span></Show>{fileName(props.path)}</span>
      <span class="file-change">{range()}</span>
    </div>
    <CodeLines text={props.text} path={props.path} />
  </div>;
}

// The command in its own highlighted box, the output plain under it
function ShellOutput(props: { command: string; output: string; lang: string }) {
  const html = () => isLoaded(props.lang) && highlight(props.command, props.lang) || `<pre><code>${escapeHtml(props.command)}</code></pre>`;
  return <div class="tool-output">
    <CopyButton text={() => props.output ? `${props.command}\n\n${props.output}` : props.command} label={tr().chat.copy} class="output-copy" />
    <div class="tool-scroll shell-body">
      <div class="shell-command" innerHTML={html()} />
      <Show when={props.output}><pre><code>{props.output}</code></pre></Show>
    </div>
  </div>;
}

type Hit = { title: string; url: string; snippet: string };

// Search output: "1. title", then the url and the snippet indented under it
function searchHits(text: string) {
  const hits: Hit[] = [];
  for (const block of text.split(/\n(?=\d+\. )/)) {
    const [title, url, ...rest] = block.split("\n");
    const head = title?.match(/^\d+\. (.*)$/);
    if (head && url?.trim().startsWith("http")) hits.push({ title: head[1], url: url.trim(), snippet: rest.join(" ").trim() });
  }
  return hits;
}

function SearchResults(props: { text: string }) {
  const hits = () => searchHits(props.text);
  return <Show when={hits().length} fallback={<Output text={props.text} />}>
    <div class="tool-output"><div class="tool-scroll search-hits">
      <For each={hits()}>{hit => <div class="search-hit">
        <a href={hit.url} onClick={event => event.preventDefault()}>{hit.title}</a>
        <span class="search-url">{hit.url}</span>
        <Show when={hit.snippet}><span class="search-snippet">{hit.snippet}</span></Show>
      </div>}</For>
    </div></div>
  </Show>;
}

// What a finished tool shows when expanded; undefined when there is nothing to show
export function toolBody(part: ToolPart): (() => JSX.Element) | undefined {
  if (isRunning(part)) return undefined;
  const output = stripAnsi(part.state.output ?? "").trimEnd();
  const files = changedFiles(part);
  switch (part.tool) {
    case "edit": case "write":
      return files.length ? () => <div class="file-list"><For each={files}>{file => <FilePanel file={file} id={`${part.id}:${file.relativePath}`} />}</For></div> : undefined;
    case "bash": case "powershell":
      return () => <ShellOutput command={part.state.input.command ?? ""} output={output} lang={part.tool === "bash" ? "shellscript" : "powershell"} />;
    case "read": return output ? () => <ReadPanel path={part.state.input.path ?? ""} text={output} /> : undefined;
    case "search": return output ? () => <SearchResults text={output} /> : undefined;
    case "fetch": case "list": case "glob": case "grep": case "task": return output ? () => <Output text={output} /> : undefined;
    default: return undefined;
  }
}

function ToolCard(props: { part: ToolPart }) {
  const body = createMemo(() => toolBody(props.part));
  const files = () => changedFiles(props.part);
  const total = () => diffTotal(files());
  const edit = () => props.part.tool === "edit" || props.part.tool === "write";
  const shell = () => props.part.tool === "bash" || props.part.tool === "powershell";
  const head = () => <>
    <ToolTitle label={toolLabel(props.part)} running={isRunning(props.part)} />
    <Show when={!isRunning(props.part) && files().length}><DiffCounts additions={total().additions} deletions={total().deletions} /></Show>
  </>;
  return <Collapse id={props.part.id} class="tool" classList={{ "patch-tool": edit() }} head={head()} enabled={!!body()}
    defaultOpen={edit() ? preferences.chat.expandEdits : shell() ? preferences.chat.expandCommands : false}>
    {body()?.()}
  </Collapse>;
}

function AgentCard(props: { part: ToolPart; onOpen: (chatId: string) => void }) {
  const chatId = () => props.part.state.metadata?.sessionId as string | undefined;
  const type = () => props.part.state.input.subagent_type ?? "";
  return <button type="button" class="agent-card" disabled={!chatId()} onClick={() => chatId() && props.onOpen(chatId()!)} title={tr().chat.openAgent}>
    <Show when={isRunning(props.part)}><span class="agent-spinner" style={{ color: agentColor(type()) }} /></Show>
    <span class="agent-title" style={{ color: agentColor(type()) }}>{agentName(type())}</span>
    <span class="tool-subtitle agent-subtitle">{props.part.state.input.description}</span>
    <Show when={props.part.state.status === "error"}><span class="agent-status" classList={{ failed: props.part.state.error !== CANCELLED }}>{props.part.state.error === CANCELLED ? tr().chat.stopped : tr().chat.failed}</span></Show>
    <span class="agent-open"><Glyph name="open-out" size={15} /></span>
  </button>;
}

function AskTool(props: { part: ToolPart }) {
  const questions = () => (props.part.state.input.questions ?? []) as { question: string }[];
  const answers = () => (props.part.state.metadata?.answers ?? []) as string[][];
  return <Collapse id={props.part.id} class="tool" defaultOpen enabled={answers().length > 0}
    head={<ToolTitle label={{ title: tr().chat.tool.ask, subtitle: tr().chat.answered(questions().length) }} />}>
    <div class="answers">
      <For each={questions()}>{(item, index) => <div class="answer">
        <div class="answer-question">{item.question}</div>
        <div class="answer-text">{answers()[index()]?.join(", ") || tr().chat.noAnswer}</div>
      </div>}</For>
    </div>
  </Collapse>;
}

function ToolError(props: { part: ToolPart }) {
  const error = () => (props.part.state.error ?? "").replace(/^Error: /, "");
  const stopped = () => error() === CANCELLED;
  return <Show when={props.part.tool !== "ask" || (!stopped() && error() !== DISMISSED)} fallback={<div class="tool-dismissed">{tr().chat.dismissed}</div>}>
    <Collapse id={props.part.id} class="tool tool-error" enabled={!stopped()}
      head={<ToolTitle label={{ title: toolLabel(props.part).title, subtitle: stopped() ? tr().chat.stopped : error().split("\n")[0] }} />}>
      <Output text={error()} />
    </Collapse>
  </Show>;
}

export function ToolView(props: { part: ToolPart; onOpen: (chatId: string) => void }) {
  return <Switch fallback={<ToolCard part={props.part} />}>
    <Match when={props.part.tool === "agent"}><AgentCard part={props.part} onOpen={props.onOpen} /></Match>
    <Match when={props.part.state.status === "error"}><ToolError part={props.part} /></Match>
    <Match when={props.part.tool === "ask"}><AskTool part={props.part} /></Match>
  </Switch>;
}
