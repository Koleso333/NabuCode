import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { Dynamic } from "solid-js/web";
import type { ContextParts, UsageBar } from "@nabu/protocol";
import { language, tr } from "./i18n";
import { preferences } from "./preferences";
import { Glyph } from "./controls";

/** A bar; with `segments` the filled part is split into coloured shares of it. */
export function Meter(props: { fraction: number; marker?: number; segments?: { weight: number; color: string }[] }) {
  return <div class="meter" aria-label={`${Math.round(props.fraction * 100)}%`}>
    <Show when={props.segments} fallback={<div class="meter-fill" style={{ width: `${Math.min(1, props.fraction) * 100}%` }} />}>
      <div class="meter-segments" style={{ width: `${Math.min(1, props.fraction) * 100}%` }}>
        <For each={props.segments}>{s => <div style={{ flex: `${s.weight} 0 0`, background: s.color }} />}</For>
      </div>
    </Show>
    <Show when={props.marker !== undefined}><div class="meter-marker" style={{ left: `${props.marker! * 100}%` }} /></Show>
  </div>;
}

export const percent = (fraction: number) => `${Math.round(fraction * 100)}%`;

/** How much of a bar is used up; credits and money count what was spent. */
export const barFraction = (bar: UsageBar) => bar.total > 0 ? bar.used / bar.total : 0;

const [now, setNow] = createSignal(Date.now());
let watchers = 0;
let timer: ReturnType<typeof setInterval> | undefined;

// One clock for every countdown on screen, running only while one is shown
function watchClock() {
  if (watchers++ === 0) {
    setNow(Date.now());
    timer = setInterval(() => setNow(Date.now()), 30_000);
  }
  onCleanup(() => { if (--watchers === 0) clearInterval(timer); });
}

/** "1д 5ч 40м", without leading zero units. */
function countdown(iso: string) {
  const total = Math.max(0, Math.ceil((Date.parse(iso) - now()) / 60_000));
  const [days, hours, minutes] = [Math.floor(total / 1440), Math.floor(total % 1440 / 60), total % 60];
  const units = tr().usage.units;
  return [days && `${days}${units.days}`, (days || hours) && `${hours}${units.hours}`, `${minutes}${units.minutes}`].filter(Boolean).join(" ");
}

function barLabel(bar: UsageBar) {
  const text = tr().usage;
  if (bar.label) return text.windows[bar.label] ?? bar.label.charAt(0).toUpperCase() + bar.label.slice(1);
  return bar.unit === "usd" ? text.dailyQuota : text.credits;
}

function barValue(bar: UsageBar) {
  const number = (value: number, money = false) => new Intl.NumberFormat(language(), money
    ? { style: "currency", currency: "USD", maximumFractionDigits: 2 } : { maximumFractionDigits: 2 }).format(value);
  if (bar.unit === "share") return percent(bar.used);
  if (bar.unit === "usd") return tr().usage.spent(number(bar.used, true), number(bar.total, true));
  return tr().usage.left(number(Math.max(0, bar.total - bar.used)), number(bar.total));
}

/** One quota, credit balance or money bar: name and reset above, the bar below; with `onOpen` it is a button. */
export function UsageLine(props: { bar: UsageBar; onOpen?: () => void }) {
  watchClock();
  const fraction = () => barFraction(props.bar);
  const reset = () => props.bar.idle ? tr().usage.notStarted : props.bar.reset && countdown(props.bar.reset);
  return <Dynamic component={props.onOpen ? "button" : "div"} type={props.onOpen ? "button" : undefined}
    class="quota" classList={{ "usage-link": !!props.onOpen }} onClick={props.onOpen}>
    <div class="usage-line">
      <span>{barLabel(props.bar)}<Show when={reset()}> | {tr().usage.reset(reset()!)}</Show></span>
      <span classList={{ warning: fraction() >= .8 }}>{barValue(props.bar)}</span>
    </div>
    <Meter fraction={fraction()} />
  </Dynamic>;
}

export function tokenCount(tokens: number) {
  if (tokens >= 1e6) return `${Math.round(tokens / 1e5) / 10}${tr().usage.millions}`;
  return tokens >= 1e3 ? `${Math.round(tokens / 1e3)}${tr().usage.thousands}` : String(tokens);
}

const share = (fraction: number) =>
  `${new Intl.NumberFormat(language(), { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(fraction * 100)}%`;

type Share = { name: string; tokens: number; color?: string };

const colors = { messages: "var(--focus)", tools: "var(--syntax-keyword)", system: "var(--syntax-string)" };

/** The server estimates by bytes; scaled so the parts add up to what the provider counted. */
function usedShares(parts: ContextParts, used: number): { taken: Share[]; deferred: number } {
  const text = tr().usage;
  const estimated = parts.messages + parts.tools + parts.system;
  const scale = used > 0 && estimated > 0 ? used / estimated : 1;
  const tokens = (n: number) => Math.round(n * scale);
  return {
    taken: [
      { name: text.messages, tokens: tokens(parts.messages), color: colors.messages },
      { name: text.tools, tokens: tokens(parts.tools), color: colors.tools },
      { name: text.system, tokens: tokens(parts.system), color: colors.system },
    ],
    deferred: tokens(parts.deferred),
  };
}

function ShareRow(props: { share: Share; limit: number; free?: "track" | "reserve"; outside?: boolean }) {
  return <div class="context-row" classList={{ dim: !!props.free || props.outside }}>
    <span class={`context-square ${props.free ?? ""}`} style={{ background: props.share.color }} />
    <span class="context-name">{props.share.name}</span>
    <span class="context-tokens">{tokenCount(props.share.tokens)}</span>
    <span class="context-share">{props.outside ? "—" : share(props.share.tokens / props.limit)}</span>
  </div>;
}

function Breakdown(props: { parts: ContextParts; used: number; limit: number; compactAt?: number }) {
  const text = () => tr().usage;
  const shares = () => usedShares(props.parts, props.used);
  const taken = () => shares().taken.reduce((sum, s) => sum + s.tokens, 0);
  const free = () => Math.max(0, props.limit - taken());
  const untilCompact = () => props.compactAt === undefined ? free() : Math.max(0, props.compactAt * props.limit - taken());
  return <div class="context-breakdown">
    <div class="context-section">{text().taken(tokenCount(taken()))}</div>
    <For each={shares().taken}>{s => <ShareRow share={s} limit={props.limit} />}</For>
    <div class="context-section">{text().free(tokenCount(free()))}</div>
    <ShareRow share={{ name: props.compactAt === undefined ? text().untilLimit : text().untilCompactRow, tokens: untilCompact() }}
      limit={props.limit} free="track" />
    <Show when={props.compactAt !== undefined}>
      <ShareRow share={{ name: text().reserve, tokens: free() - untilCompact() }} limit={props.limit} free="reserve" />
    </Show>
    <Show when={shares().deferred > 0}>
      <div class="context-section">{text().outside}</div>
      <ShareRow share={{ name: text().deferred, tokens: shares().deferred }} limit={props.limit} outside />
    </Show>
  </div>;
}

/** The context window; with `loadParts` it opens into what the next request holds. */
function ContextBlock(props: { used: number; limit: number; loadParts?: () => Promise<ContextParts> }) {
  const filled = () => Math.min(1, props.used / props.limit);
  const compactAt = () => preferences.compaction.auto ? preferences.compaction.threshold / 100 : undefined;
  const [breakdownOpen, setBreakdownOpen] = createSignal(false);
  const open = () => breakdownOpen() && !!props.loadParts;
  const [parts] = createResource(() => open() && { load: props.loadParts!, used: props.used }, ({ load }) => load());
  const segments = () => {
    const loaded = open() && parts.latest;
    return loaded ? usedShares(loaded, props.used).taken.map(s => ({ weight: s.tokens, color: s.color! })) : undefined;
  };
  return <>
    <Dynamic component={props.loadParts ? "button" : "div"} type={props.loadParts ? "button" : undefined}
      class="context-block" classList={{ "usage-link": !!props.loadParts }}
      onClick={() => setBreakdownOpen(!breakdownOpen())}>
      <div class="usage-line">
        <span class="context-title">{tr().usage.contextWindow}
          <Show when={props.loadParts}><span class="chevron" classList={{ open: open() }}><Glyph name="chevron-right" size={12} /></span></Show>
        </span>
        <span>{percent(filled())} | {tokenCount(props.used)}</span>
      </div>
      <Meter fraction={filled()} marker={compactAt()} segments={segments()} />
      <div class="usage-scale"><span>0</span>
        <span>{compactAt() !== undefined ? tr().usage.untilCompact(percent(Math.max(0, compactAt()! - filled()))) : ""}</span>
        <span>{tokenCount(props.limit)}</span></div>
    </Dynamic>
    <Show when={open() && parts.latest}>{loaded =>
      <Breakdown parts={loaded()} used={props.used} limit={props.limit} compactAt={compactAt()} />}</Show>
  </>;
}

/** The usage popup: provider quotas, then the context window. */
export function Usage(props: {
  bars: UsageBar[]; context: { used: number; limit: number }; onOpen?: () => void; loadParts?: () => Promise<ContextParts>;
}) {
  return <div class="popup usage-popup panel-frame">
    <For each={props.bars}>{bar => <UsageLine bar={bar} onOpen={props.onOpen} />}</For>
    <Show when={props.bars.length}><div class="usage-divider" /></Show>
    <ContextBlock used={props.context.used} limit={props.context.limit} loadParts={props.loadParts} />
  </div>;
}
