import { createEffect, createMemo, createSignal, Index, onCleanup, Show } from "solid-js";
import { tr } from "../i18n";
import { DiffCounts } from "./diff";
import { Markdown } from "./markdown";
import { CANCELLED } from "./engine";
import { isRunning, type Message, type Part, type RetryPart, type ToolPart } from "./model";
import { paced } from "./pace";
import { changedFiles, Chevron, Collapse, diffTotal, isOpen, Output, Shimmer, toggleOpen, toolBody, toolLabel, ToolTitle, type ToolLabel } from "./parts";

export type Item = { part: Part; message: Message };

type StepKind = "read" | "search" | "edit" | "shell" | "browser" | "other";

const stepKinds: Record<string, StepKind> = {
  read: "read", list: "read", grep: "search", glob: "search", search: "search", fetch: "search",
  edit: "edit", write: "edit", bash: "shell", powershell: "shell", task: "shell",
  browser_open: "browser", browser_state: "browser", browser_act: "browser", browser_read: "browser", browser_js: "browser", browser_logs: "browser", browser_tabs: "browser",
};

const kindOf = (part: ToolPart) => stepKinds[part.tool] ?? "other";
// A call stopped by the user is not an error of the model
const failed = (part: ToolPart) => part.state.status === "error" && part.state.error !== CANCELLED;

// "Посмотрел 2 файла, искал 2 раза, изменил 1 файл, переподключался 3 раза"; edits count unique files
function stepsPhrase(tools: ToolPart[], retries: number) {
  const counts: Record<StepKind, number> = { read: 0, search: 0, edit: 0, shell: 0, browser: 0, other: 0 };
  const edited = new Set<string>();
  for (const part of tools) {
    const kind = kindOf(part);
    if (kind !== "edit") counts[kind]++;
    else {
      const files = changedFiles(part);
      if (files.length) files.forEach(file => edited.add(file.relativePath));
      else edited.add(part.state.input.path ?? part.id);
    }
  }
  counts.edit = edited.size;
  const parts = (Object.keys(counts) as StepKind[]).filter(kind => counts[kind] > 0).map(kind => tr().chat.steps[kind](counts[kind]));
  if (retries) parts.push(tr().chat.steps.retried(retries));
  const phrase = parts.join(", ");
  return phrase.charAt(0).toUpperCase() + phrase.slice(1);
}

function stepLabel(part: Part): ToolLabel {
  return part.type === "tool" ? toolLabel(part) : { title: tr().chat.steps.reasoning };
}

// While waiting the seconds to the next attempt tick down; once it went through the row stays as a count
export function RetryTitle(props: { part: RetryPart; message: Message }) {
  const live = () => !props.message.completed;
  const [now, setNow] = createSignal(Date.now());
  createEffect(() => {
    if (!live() || !props.part.next) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 250);
    onCleanup(() => clearInterval(timer));
  });
  const left = () => Math.ceil(((props.part.next ?? 0) - now()) / 1000);
  const text = () => {
    const retry = tr().chat.retry;
    if (!live() && props.message.error?.kind !== "failed") return retry.done(props.part.attempt);
    const attempt = retry.attempt(props.part.attempt, props.part.max);
    return live() && left() > 0 ? `${attempt} · ${retry.seconds(left())}` : attempt;
  };
  const redness = () => props.part.max ? 0 : Math.min(Math.max((props.part.attempt - 10) / 20, 0), 1) * 92;
  return <span class="tool-label retry-title" style={{ "--redness": `${redness()}%` }}>
    <span class="tool-title"><Shimmer text={text()} active={live()} /></span>
  </span>;
}

export function RetryReason(props: { text: string }) {
  return <div class="tool-output"><div class="retry-reason">{props.text}</div></div>;
}

function StepRow(props: { item: Item; live: boolean }) {
  const key = () => `row:${props.item.part.id}`;
  const tool = () => props.item.part.type === "tool" ? props.item.part : undefined;
  const retry = () => props.item.part.type === "retry" ? props.item.part : undefined;
  const thought = () => props.item.part.type === "reasoning" ? props.item.part.text.trim() : "";
  const body = createMemo(() => {
    const reason = retry()?.error;
    if (reason) return () => <RetryReason text={reason} />;
    const part = tool();
    if (!part) return undefined;
    if (part.state.status === "error") return failed(part) ? () => <Output text={part.state.error ?? ""} /> : undefined;
    return toolBody(part);
  });
  const files = () => tool() && tool()!.state.status !== "error" ? changedFiles(tool()!) : [];
  const diff = createMemo(() => diffTotal(files()));
  const expandable = () => !!(thought() || body());
  const open = () => expandable() && isOpen(key());
  const shownThought = paced(thought, () => props.live);

  return <div class="step">
    <div class="step-row" classList={{ expandable: expandable(), error: !!tool() && failed(tool()!) }} onClick={() => expandable() && toggleOpen(key())}>
      <Show when={retry()} fallback={<ToolTitle label={stepLabel(props.item.part)} running={!!tool() && isRunning(tool()!)} />}>
        {part => <RetryTitle part={part()} message={props.item.message} />}
      </Show>
      <Show when={expandable()}><Chevron open={open()} /></Show>
      <Show when={files().length}><DiffCounts additions={diff().additions} deletions={diff().deletions} /></Show>
    </div>
    <Show when={open()}>
      <Show when={thought()} fallback={<div class="step-body">{body()?.()}</div>}><Markdown class="step-thought" text={shownThought()} live={props.live} /></Show>
    </Show>
  </div>;
}

// Tools and reasoning between two texts, folded into one line
export function StepsGroup(props: { id: string; items: Item[]; busy: boolean }) {
  const parts = createMemo(() => props.items.map(item => item.part));
  const tools = createMemo(() => parts().filter((part): part is ToolPart => part.type === "tool"));
  const retries = () => parts().reduce((sum, part) => sum + (part.type === "retry" ? part.attempt : 0), 0);
  const phrase = createMemo(() => stepsPhrase(tools(), retries()));
  const errors = createMemo(() => tools().filter(failed).length);
  const latest = () => props.busy ? props.items.at(-1) : undefined;
  const open = () => isOpen(props.id);
  return <div class="steps">
    <div class="steps-head" role="button" aria-expanded={open()} onClick={() => toggleOpen(props.id)}>
      <Show when={latest()} fallback={<span class="steps-phrase">{phrase()}<Show when={errors()}>, <span class="steps-errors">{tr().chat.steps.error(errors())}</span></Show></span>}>
        {item => <Show when={item().part.type === "retry" && item().part as RetryPart} fallback={<ToolTitle label={stepLabel(item().part)} running />}>
          {part => <RetryTitle part={part()} message={item().message} />}
        </Show>}
      </Show>
      <Chevron open={open()} />
    </div>
    <Show when={open()}>
      <div class="steps-list">
        <Index each={props.items}>{item => <StepRow item={item()} live={props.busy} />}</Index>
      </div>
    </Show>
  </div>;
}

// Reads and searches in a row, as one "Посмотрел" line
export function ContextGroup(props: { id: string; parts: ToolPart[]; busy: boolean }) {
  const active = () => props.busy || props.parts.some(isRunning);
  const summary = createMemo(() => {
    const count = (...tools: string[]) => props.parts.filter(part => tools.includes(part.tool)).length;
    const items: [number, (n: number) => string][] = [
      [count("read"), tr().chat.context.read],
      [count("glob", "grep"), tr().chat.context.search],
      [count("list"), tr().chat.context.list],
    ];
    return items.filter(([n]) => n > 0).map(([n, text]) => text(n)).join(", ");
  });
  return <Collapse id={props.id} class="tool context-group"
    head={<span class="tool-label"><span class="tool-title"><Shimmer text={active() ? tr().chat.context.active : tr().chat.context.done} active={active()} /></span><span class="tool-subtitle">{summary()}</span></span>}>
    <div class="context-list">
      <Index each={props.parts}>{part => <Collapse id={`ctx:${part().id}`} class="context-item" head={<ToolTitle label={toolLabel(part())} running={isRunning(part())} />} enabled={!!toolBody(part())}>
        {toolBody(part())?.()}
      </Collapse>}</Index>
    </div>
  </Collapse>;
}
