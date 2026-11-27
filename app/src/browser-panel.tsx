import { createEffect, createSignal, For, on, Show } from "solid-js";
import type { BrowserTab } from "@nabu/protocol";
import { tr } from "./i18n";
import { ContextMenu, Glyph, IconButton, MenuItem, Ring } from "./controls";
import { showFailure } from "./errors";
import { request } from "./server";
import { activeBrowserTab, browserTabsOf, closeBrowserTab, openBrowserTab, searchEngines, showBrowserTab } from "./browsers";
import { preferences } from "./preferences";
import { browserView } from "./browser-view";

/** The right panel of a chat's browser tabs: the tabs, the address row and the page of the shown tab. */
export function BrowserPanel(props: { session: string | null; width: number; bottom: number; onWidth: (width: number) => void; onHide: () => void }) {
  const text = () => tr().browser;
  const list = () => props.session ? browserTabsOf(props.session) : [];
  const active = () => props.session ? activeBrowserTab(props.session) : undefined;
  const [menu, setMenu] = createSignal<{ x: number; y: number } | null>(null);
  const [editing, setEditing] = createSignal(false);
  let screen!: HTMLDivElement;
  let bar!: HTMLDivElement;
  let address: HTMLInputElement | undefined;

  createEffect(on(() => active()?.id, id => {
    setEditing(false);
    if (!id) return screen.replaceChildren();
    browserView(id).show(screen);
    bar.querySelector(".browser-tab.selected")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }));

  // The panel hides when the last tab of its chat is gone
  createEffect(on(() => [props.session, list().length] as const, ([session, count], previous) => {
    if (!count && previous?.[0] === session && previous[1]) props.onHide();
  }));

  // A new empty tab waits for an address
  const add = () => { if (props.session) void openBrowserTab(props.session).then(edit, showFailure); };
  const close = (id: string) => { if (props.session) void closeBrowserTab(props.session, id).catch(showFailure); };
  const closeAll = (keep?: string) => {
    setMenu(null);
    for (const item of list()) if (item.id !== keep) close(item.id);
  };
  const run = (work: (id: string) => Promise<unknown>) => { const id = active()?.id; if (id) void work(id).catch(showFailure); };

  function edit() {
    setEditing(true);
    requestAnimationFrame(() => { address?.focus(); address?.select(); });
  }
  function go(url: string) {
    setEditing(false);
    const search = searchEngines[preferences.searchEngine]?.url;
    if (url.trim()) run(id => request("browserGo", { id, url: url.trim(), search }));
  }

  function resize(event: PointerEvent) {
    event.preventDefault();
    const startX = event.clientX, startWidth = props.width;
    const handle = event.currentTarget as HTMLElement;
    handle.setPointerCapture(event.pointerId);
    const move = (next: PointerEvent) => props.onWidth(startWidth - (next.clientX - startX));
    const stop = () => { handle.removeEventListener("pointermove", move); handle.removeEventListener("pointerup", stop); handle.removeEventListener("pointercancel", stop); };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }

  return <aside class="browser-panel" style={{ width: `${props.width}px`, bottom: `${props.bottom}px` }}>
    <div class="files-panel-resize" onPointerDown={resize} />
    <div ref={bar} class="browser-row browser-tabs">
      <div class="browser-tab-list"><For each={list()}>{item => <div class="press browser-tab" classList={{ selected: item.id === active()?.id }} role="tab" title={titleOf(item, text().newTab)}
        aria-selected={item.id === active()?.id} onClick={() => props.session && showBrowserTab(props.session, item.id)}
        onAuxClick={event => { if (event.button === 1) close(item.id); }}>
        <Favicon tab={item} />
        <span class="truncate browser-tab-title">{titleOf(item, text().newTab)}</span>
        <button type="button" class="browser-tab-close" aria-label={text().close} title={text().close}
          onClick={event => { event.stopPropagation(); close(item.id); }}><Glyph name="close" size={11} /></button>
      </div>}</For></div>
      <Show when={props.session}><IconButton name="plus" label={text().newTab} size={17} width={23} onClick={add} /></Show>
      <span class="files-panel-space" />
      <Show when={list().length}>
        <IconButton name="dots-vertical" label={text().more} size={15} width={23} class="browser-more" selected={!!menu()}
          onClick={() => {
            const box = bar.getBoundingClientRect();
            setMenu(menu() ? null : { x: box.right - 240, y: box.bottom + 4 });
          }} />
      </Show>
    </div>
    <Show when={active()}>{tab => <div class="browser-row">
      <IconButton name="arrow-left" label={text().back} size={16} width={25} onClick={() => run(id => request("browserHistory", { id, step: -1 }))} />
      <IconButton name="arrow-right" label={text().forward} size={16} width={25} onClick={() => run(id => request("browserHistory", { id, step: 1 }))} />
      <IconButton name="reload" label={text().reload} size={16} width={25} onClick={() => run(id => request("browserReload", { id }))} />
      <Show when={editing()} fallback={<div class="browser-address" onClick={edit}><Address url={shownUrl(tab())} /></div>}>
        <input ref={address} class="browser-address browser-address-input" aria-label={text().address} value={shownUrl(tab())} spellcheck={false}
          onKeyDown={event => {
            if (event.key === "Enter") go(event.currentTarget.value);
            if (event.key === "Escape") setEditing(false);
          }}
          onBlur={() => setEditing(false)} />
      </Show>
    </div>}</Show>
    <div ref={screen} class="browser-screens" />
    <Show when={!props.session}><div class="browser-empty">{text().noSession}</div></Show>
    <Show when={menu()}>{at => <ContextMenu x={at().x} y={at().y} label={text().more} trigger={bar.querySelector<HTMLElement>(".browser-more")!} onClose={() => setMenu(null)}>
      <MenuItem icon="plus" label={text().newTab} onClick={() => { setMenu(null); add(); }} />
      <Show when={list().length > 1}><MenuItem icon="close" label={text().closeOthers} onClick={() => closeAll(active()?.id)} /></Show>
      <MenuItem icon="trash" label={text().closeAll} onClick={() => closeAll()} />
    </ContextMenu>}</Show>
  </aside>;
}

// A site without an icon of its own gets the grey globe, as in Chrome
function Favicon(props: { tab: BrowserTab }) {
  return <Show when={!props.tab.loading} fallback={<span class="browser-favicon"><Ring fraction={.68} size={14} loading /></span>}>
    <Show when={props.tab.favicon} fallback={<span class="browser-favicon browser-favicon-none"><Glyph name="globe" size={14} /></span>}>
      <img class="browser-favicon" src={props.tab.favicon!} width={14} height={14} alt="" draggable={false} />
    </Show>
  </Show>;
}

// The host stands out from the rest, as in Chrome; https:// is left out
function Address(props: { url: string }) {
  const parts = () => readable(props.url);
  return <span class="truncate"><span class="browser-host">{parts().host && `${parts().scheme === "http:" ? "http://" : ""}${parts().host}`}</span><span class="browser-path">{parts().rest}</span></span>;
}

// The address as a person reads it: a punycode host and %-escapes come decoded
function readable(link: string) {
  try {
    const url = new URL(link);
    if (!/^https?:$/.test(url.protocol)) return { scheme: "", host: "", rest: link };
    const host = url.hostname.split(".").map(label => label.startsWith("xn--") ? fromPunycode(label.slice(4)) : label).join(".");
    const rest = `${url.pathname === "/" ? "" : url.pathname}${url.search}${url.hash}`;
    return { scheme: url.protocol, host: url.port ? `${host}:${url.port}` : host, rest: safeDecode(rest) };
  } catch {
    return { scheme: "", host: "", rest: link };
  }
}

const safeDecode = (text: string) => { try { return decodeURI(text); } catch { return text; } };

// RFC 3492 decoding of one label
function fromPunycode(input: string) {
  const base = 36, tMin = 1, tMax = 26;
  const split = input.lastIndexOf("-");
  const output = split > 0 ? [...input.slice(0, split)].map(c => c.codePointAt(0)!) : [];
  let n = 128, i = 0, bias = 72;
  for (let at = split > 0 ? split + 1 : 0; at < input.length;) {
    const old = i;
    for (let w = 1, k = base; ; k += base) {
      const c = input.charCodeAt(at++);
      const digit = c - 48 < 10 ? c - 22 : c - 65 < 26 ? c - 65 : c - 97 < 26 ? c - 97 : base;
      if (digit >= base || at > input.length) return `xn--${input}`;
      i += digit * w;
      const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
      if (digit < t) break;
      w *= base - t;
    }
    const length = output.length + 1;
    let delta = old === 0 ? Math.floor((i - old) / 700) : (i - old) >> 1;
    delta += Math.floor(delta / length);
    let k = 0;
    for (; delta > ((base - tMin) * tMax) >> 1; k += base) delta = Math.floor(delta / (base - tMin));
    bias = Math.floor(k + (base - tMin + 1) * delta / (delta + 38));
    n += Math.floor(i / length);
    i %= length;
    output.splice(i++, 0, n);
  }
  return String.fromCodePoint(...output);
}

const shownUrl = (tab: BrowserTab) => {
  if (tab.url === "about:blank") return "";
  const parts = readable(tab.url);
  return parts.host ? `${parts.scheme}//${parts.host}${parts.rest || "/"}` : tab.url;
};

const titleOf = (tab: BrowserTab, empty: string) => tab.title && tab.title !== "about:blank" ? tab.title : shownUrl(tab) || empty;
