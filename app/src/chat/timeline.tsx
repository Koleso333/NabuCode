import { createEffect, createMemo, For, Match, on, onCleanup, onMount, Show, Switch, type Accessor } from "solid-js";
import { Glyph } from "../controls";
import { tr } from "../i18n";
import { preferences } from "../preferences";
import { Markdown } from "./markdown";
import { chats, type Chat, type FileChange, type Message, type Part, type RetryPart, type TextPart, type ToolPart } from "./model";
import { paced } from "./pace";
import { agentColor, agentName, changedFiles, Collapse, diffTotal, CopyButton, FilePanel, Shimmer, ToolView } from "./parts";
import { DiffCounts } from "./diff";
import { AttachmentTile } from "../attachments";
import { ContextGroup, RetryReason, RetryTitle, StepsGroup, type Item } from "./steps";
import { loadOlder } from "./remote";
import { markText, piecesOf } from "../marks";

type Group = { key: string; kind: "part" | "context" | "steps"; items: Item[] };
type Turn = { user: Message; assistants: Message[] };

const contextTools = new Set(["read", "glob", "grep", "list"]);
const isStep = (part: Part) => part.type === "reasoning" || part.type === "retry" || (part.type === "tool" && part.tool !== "agent" && part.tool !== "ask");
const isContext = (part: Part) => part.type === "tool" && contextTools.has(part.tool) && part.state.status !== "error";
const sameKeys = (a: string[], b: string[]) => a.length === b.length && a.every((key, index) => key === b[index]);

// Reads only ids, kinds and statuses: streamed text never regroups the turn
function visible(part: Part) {
  if (part.type === "reasoning") return preferences.chat.reasoning;
  if (part.type === "tool" && part.tool === "ask") return part.state.status !== "running" && part.state.status !== "pending";
  return true;
}

function groupContext(items: Item[]) {
  const groups: Group[] = [];
  let run: Item[] = [];
  const flush = () => {
    if (run.length) groups.push({ key: `context:${run[0].part.id}`, kind: "context", items: run });
    run = [];
  };
  for (const item of items) {
    if (isContext(item.part)) { run.push(item); continue; }
    flush();
    groups.push({ key: `part:${item.part.id}`, kind: "part", items: [item] });
  }
  flush();
  return groups;
}

// Tools, retries and reasoning between two texts fold into one steps group once there are 2+ tools or retries
function groupItems(items: Item[]) {
  const groups: Group[] = [];
  let run: Item[] = [];
  const flush = () => {
    if (run.filter(item => item.part.type === "tool" || item.part.type === "retry").length >= 2) groups.push({ key: `steps:${run[0].part.id}`, kind: "steps", items: run });
    else groups.push(...groupContext(run));
    run = [];
  };
  for (const item of items) {
    if (isStep(item.part)) { run.push(item); continue; }
    flush();
    groups.push(...groupContext([item]));
  }
  flush();
  return groups;
}

function duration(turn: Turn) {
  const first = turn.assistants[0];
  const last = turn.assistants.at(-1);
  if (!first || !last?.completed) return "";
  const total = Math.round((last.completed - first.created) / 1000);
  return total < 60 ? tr().chat.seconds(total) : tr().chat.minutes(Math.floor(total / 60), total % 60);
}

function TextView(props: { part: TextPart; live: boolean; meta?: string; interrupted?: boolean }) {
  const text = paced(() => props.part.text, () => props.live);
  return <div class="text-part">
    <Markdown text={text()} live={props.live} />
    <Show when={props.meta !== undefined}>
      <div class="text-meta" classList={{ interrupted: props.interrupted }}>
        <CopyButton text={() => props.part.text} label={tr().chat.copyResponse} />
        <span>{props.meta}</span>
      </div>
    </Show>
  </div>;
}

function ReasoningView(props: { part: TextPart; live: boolean }) {
  const text = paced(() => props.part.text, () => props.live);
  return <Markdown class="reasoning-part" text={text()} live={props.live} />;
}

function GroupView(props: { group: Accessor<Group | undefined>; busy: boolean; meta?: string; interrupted?: boolean; onOpen: (chatId: string) => void }) {
  return <Show when={props.group()}>{group => <Switch>
    <Match when={group().kind === "steps"}><StepsGroup id={group().key} items={group().items} busy={props.busy} /></Match>
    <Match when={group().kind === "context"}><ContextGroup id={group().key} parts={group().items.map(item => item.part as ToolPart)} busy={props.busy} /></Match>
    <Match when={group().items[0]}>{item => {
      const live = () => !item().message.completed;
      return <Switch>
        <Match when={item().part.type === "text"}><TextView part={item().part as TextPart} live={live()} meta={props.meta} interrupted={props.interrupted} /></Match>
        <Match when={item().part.type === "reasoning"}><ReasoningView part={item().part as TextPart} live={live()} /></Match>
        <Match when={item().part.type === "tool"}><ToolView part={item().part as ToolPart} onOpen={props.onOpen} /></Match>
        <Match when={item().part.type === "retry" && item().part as RetryPart}>{part =>
          <Collapse id={part().id} class="tool" head={<RetryTitle part={part()} message={item().message} />}><RetryReason text={part().error} /></Collapse>}</Match>
      </Switch>;
    }}</Match>
  </Switch>}</Show>;
}

function UserMessage(props: { message: Message }) {
  // A message from the demo or still on its way has only its text
  const pieces = createMemo(() => props.message.parts.some(part => part.type === "mark") ? piecesOf(props.message.parts) : [props.message.text ?? ""]);
  const time = () => new Intl.DateTimeFormat(undefined, { timeStyle: "short" }).format(props.message.created);
  return <div class="user-message">
    <Show when={props.message.attachments?.length}><div class="user-files"><For each={props.message.attachments}>{file => <AttachmentTile file={file} />}</For></div></Show>
    <Show when={props.message.text}><div class="user-text"><For each={pieces()}>{piece =>
      typeof piece === "string" ? piece : <span class="mark">{markText(piece)}</span>}</For></div></Show>
    <div class="user-meta"><span>{time()}</span><CopyButton text={() => props.message.text ?? ""} label={tr().chat.copyMessage} /></div>
  </div>;
}

// Net changes of the turn, one entry per file
function DiffSummary(props: { id: string; tools: ToolPart[] }) {
  const files = createMemo(() => {
    const byPath = new Map<string, FileChange>();
    for (const tool of props.tools) {
      for (const file of changedFiles(tool)) {
        const known = byPath.get(file.relativePath);
        byPath.set(file.relativePath, known
          ? { ...known, patch: `${known.patch}\n${file.patch}`, additions: known.additions + file.additions, deletions: known.deletions + file.deletions }
          : file);
      }
    }
    return [...byPath.values()];
  });
  const total = () => diffTotal(files());
  return <Show when={files().length}>
    <Collapse id={props.id} class="tool diff-summary" head={<span class="tool-label"><span class="tool-title">{tr().chat.changed(files().length)}</span><DiffCounts additions={total().additions} deletions={total().deletions} /></span>}>
      <div class="file-list"><For each={files()}>{file => <FilePanel file={file} id={`${props.id}:${file.relativePath}`} defaultOpen={false} />}</For></div>
    </Collapse>
  </Show>;
}

function TurnView(props: { chat: Chat; turn: Accessor<Turn | undefined>; last: boolean; height?: number; onOpen: (chatId: string) => void }) {
  const turn = () => props.turn()!;
  const busy = () => props.last && props.chat.busy;
  const items = createMemo(() => turn().assistants.flatMap(message => message.parts.filter(visible).map(part => ({ part, message }))));
  const groups = createMemo(() => groupItems(items()));
  const keys = createMemo(() => groups().map(group => group.key), [], { equals: sameKeys });
  const byKey = createMemo(() => new Map(groups().map(group => [group.key, group])));
  const failed = () => turn().assistants.find(message => message.error?.kind === "failed")?.error;
  const interrupted = () => turn().assistants.some(message => message.error?.kind === "aborted");
  const lastText = createMemo(() => items().findLast(item => item.part.type === "text")?.part.id);
  const meta = () => {
    const model = turn().assistants.at(-1)?.model;
    return [model, duration(turn()), interrupted() ? tr().chat.interrupted : ""].filter(Boolean).join(" · ");
  };
  const stage = () => {
    const last = turn().assistants.flatMap(message => message.parts).at(-1);
    if (!preferences.chat.responseStages) return tr().chat.stage.thinking;
    return tr().chat.stage[!last ? "waiting" : last.type === "reasoning" ? "thinking" : "working"];
  };
  const edits = createMemo(() => busy() || !preferences.chat.diffSummary ? [] : items().flatMap(item => item.part.type === "tool" && (item.part.tool === "edit" || item.part.tool === "write") ? [item.part] : []));

  return <div class="turn" classList={{ settled: !props.last }} data-turn={turn().user.id}
    style={props.height ? { "contain-intrinsic-size": `auto ${props.height}px` } : undefined}>
    <UserMessage message={turn().user} />
    <div class="turn-reply">
      <For each={keys()}>{key => {
        const group = () => byKey().get(key);
        const ownsMeta = () => !busy() && group()?.kind === "part" && group()?.items[0].part.id === lastText();
        return <GroupView group={group} busy={busy() && keys().at(-1) === key} meta={ownsMeta() ? meta() : undefined} interrupted={interrupted()} onOpen={props.onOpen} />;
      }}</For>
      <Show when={busy() && !props.chat.ask}><div class="turn-status"><Shimmer text={stage()} active /></div></Show>
      <DiffSummary id={`diffs:${turn().user.id}`} tools={edits()} />
      <Show when={failed()}>{error => <div class="error-block">
        <div class="error-card">{error().message}</div>
        <div class="text-meta"><CopyButton text={() => error().message ?? ""} label={tr().chat.copyError} /></div>
      </div>}</Show>
      <Show when={interrupted() && !busy()}><div class="turn-divider"><span />{tr().chat.interrupted}<span /></div></Show>
    </div>
  </div>;
}

// Where each chat was left (null: at the bottom) and how tall its turns were. Turns off screen are not laid out,
// so a chat opened again would guess their heights and shift; with the sizes it had, it opens where it was.
const views = new Map<string, { turn: string; offset: number } | null>();
const turnHeights = new Map<string, number>();

export function Timeline(props: { chatId: string; onOpen: (chatId: string) => void; onBack: () => void }) {
  const chat = () => chats[props.chatId];
  const turns = createMemo(() => {
    const list: Turn[] = [];
    for (const message of chat()?.messages ?? []) {
      if (message.role === "user") list.push({ user: message, assistants: [] });
      else list.at(-1)?.assistants.push(message);
    }
    return list;
  });
  const keys = createMemo(() => turns().map(turn => turn.user.id), [], { equals: sameKeys });
  const byKey = createMemo(() => new Map(turns().map(turn => [turn.user.id, turn])));
  let scroller!: HTMLDivElement;
  let content!: HTMLDivElement;
  let stick = true;
  let clicked = 0;

  const nearBottom = () => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 60;
  // The column never gets shorter than what is on screen: collapsing a tool can't drag the view up
  const holdBottom = () => { content.style.minHeight = `${scroller.scrollTop + scroller.clientHeight}px`; };
  // Held only from a press in the chat: turns resizing on their own must not leave empty space below
  const held = () => !!content.style.minHeight;
  const onScroll = () => {
    // Leaving the chat scrolls the detached list to its top; that is not where the user was
    if (!scroller.isConnected) return;
    stick = nearBottom();
    if (held()) holdBottom();
    remember();
    if (scroller.scrollTop < 400) showOlder();
  };
  function remember() {
    if (stick) return void views.set(props.chatId, null);
    const top = scroller.scrollTop;
    const first = [...content.querySelectorAll<HTMLElement>(".turn")].find(turn => turn.offsetTop + turn.offsetHeight > top);
    if (first) views.set(props.chatId, { turn: first.dataset.turn!, offset: first.offsetTop - top });
  }
  // Older messages go above what is on screen: the turn that was first stays where it was on screen
  const showOlder = () => loadOlder(props.chatId, () => {
    const first = content.querySelector(".turn");
    const top = first?.getBoundingClientRect().top ?? 0;
    return () => { if (first) scroller.scrollTop += first.getBoundingClientRect().top - top; };
  });

  // Follows the bottom while the user stays there; what the user expands stays where it is
  onMount(() => {
    const observer = new ResizeObserver(() => {
      if (performance.now() - clicked < 300) stick = nearBottom();
      else if (stick) scroller.scrollTop = scroller.scrollHeight;
      if (held()) holdBottom();
      // A page shorter than the screen gives nothing to scroll, so the next one comes right away
      if (scroller.scrollTop < 400) showOlder();
    });
    observer.observe(content);
    observer.observe(scroller);
    const view = views.get(props.chatId);
    const anchor = view && content.querySelector<HTMLElement>(`[data-turn="${CSS.escape(view.turn)}"]`);
    if (view && anchor) {
      stick = false;
      scroller.scrollTop = anchor.offsetTop - view.offset;
    } else scroller.scrollTop = scroller.scrollHeight;
    onCleanup(() => {
      observer.disconnect();
      for (const turn of content.querySelectorAll<HTMLElement>(".turn")) if (turn.offsetHeight) turnHeights.set(turn.dataset.turn!, turn.offsetHeight);
    });
  });
  // A new turn at the bottom brings the view down; older ones loaded above do not
  const lastKey = createMemo(() => keys().at(-1));
  createEffect(on(lastKey, () => { stick = true; }, { defer: true }));

  return <>
    <Show when={chat()?.parentId}>
      <div class="agent-head">
        <button type="button" class="chat-back" onClick={props.onBack}><Glyph name="arrow-left" size={15} /><span>{tr().chat.backToChat}</span></button>
        <span class="agent-title" style={{ color: agentColor(chat()?.agent ?? "") }}>{agentName(chat()?.agent)}</span>
      </div>
    </Show>
    <div ref={scroller} class="chat-scroll" classList={{ agent: !!chat()?.parentId }} onScroll={onScroll}
      onPointerDown={() => { clicked = performance.now(); holdBottom(); }} onClick={() => { clicked = performance.now(); }}>
      <div ref={content} class="chat-column">
        <Show when={chat()}>{current => <For each={keys()}>{(key, index) =>
          <TurnView chat={current()} turn={() => byKey().get(key)} last={index() === keys().length - 1} height={turnHeights.get(key)} onOpen={props.onOpen} />}</For>}</Show>
      </div>
    </div>
  </>;
}
