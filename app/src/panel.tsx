import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { tr } from "./i18n";
import { Glyph, IconButton, Press } from "./controls";
import { connection, connectDirectly } from "./server";

/** everyend built for the side panel of the NabuCode extension in Chrome (`vite build --mode panel`). */
export const panelMode = import.meta.env.MODE === "panel";

type FoundServer = { port: number; token: string; data: string; version: string };
// The extension's API; the panel has no types for it
declare const chrome: { runtime: { sendMessage(message: unknown): Promise<any> }; storage: { local: { get(key: string): Promise<any>; set(items: object): Promise<void> } } };

const [servers, setServers] = createSignal<FoundServer[]>([]);
const [current, setCurrent] = createSignal<FoundServer>();
const [searched, setSearched] = createSignal(false);
let chromeId = "";

const urlOf = (server: FoundServer) => `ws://127.0.0.1:${server.port}/?token=${server.token}`;

function use(server: FoundServer) {
  setCurrent(server);
  connectDirectly(urlOf(server));
}

/** Asks the extension which NabuCode servers run on this computer and connects to the chosen one. */
export async function findServers() {
  const found = await chrome.runtime.sendMessage({ want: "servers" }).catch(() => undefined);
  chromeId = found?.chrome ?? chromeId;
  const list: FoundServer[] = found?.servers ?? [];
  setServers(list);
  setSearched(true);
  const { panelServer } = await chrome.storage.local.get("panelServer");
  const same = (server: FoundServer) => server.data === (current()?.data ?? panelServer);
  const next = list.find(same) ?? list[0];
  if (next) use(next);
}

function choose(server: FoundServer) {
  void chrome.storage.local.set({ panelServer: server.data });
  use(server);
}

/** A new chat of the panel lives as long as the panel and works in this Chrome with the browser tools only. */
export const panelSession = () => ({ temporary: true, tools: ["browser"], chrome: chromeId });

/** No server to talk to: none runs, the one in use went away, or it speaks another protocol. */
export const panelOffline = () => searched() && (!servers().length || ["reconnecting", "failed", "mismatch"].includes(connection().state));

/** Starts the search and repeats it while the panel is offline, so a started NabuCode is picked up. */
export function startPanel() {
  void findServers();
  setInterval(() => { if (panelOffline()) void findServers(); }, 3000);
}

export function PanelHeader(props: { intro: boolean; onNewChat: () => void }) {
  const [open, setOpen] = createSignal(false);
  // Opening the list looks again: another NabuCode may have started since
  const toggle = () => { if (!open()) void findServers(); setOpen(!open()); };
  const close = (event: PointerEvent) => { if (!(event.target as HTMLElement).closest(".panel-server")) setOpen(false); };
  onMount(() => window.addEventListener("pointerdown", close));
  onCleanup(() => window.removeEventListener("pointerdown", close));
  return <div class="titlebar panel-frame">
    <div class="title-left">
      <Show when={!panelOffline()}>
        <IconButton name="new-session" label={tr().window.newChat} class="title-new-chat" size={16} selected={props.intro} onClick={props.onNewChat} />
      </Show>
    </div>
    <Show when={props.intro && !panelOffline() && current()}>{server => <div class="panel-server">
      <Press label={tr().panel.server} class="server-chip" selected={open()} onClick={toggle}>
        <span class="server-dot" /><span>NabuCode</span><Glyph name="chevron-down" size={13} />
      </Press>
      <Show when={open()}><div class="popup panel-frame server-list">
        <For each={servers()}>{item => <Press label={`NabuCode ${item.version}`} class="selector-row server-row" onClick={() => { choose(item); setOpen(false); }}>
          <span class="server-dot" />
          <span class="server-name truncate">NabuCode <span class="server-version">{item.version}</span></span>
          <Show when={item.data === server().data} fallback={<span />}><Glyph name="check-small" size={15} /></Show>
          <span class="server-place truncate">{item.data}</span>
        </Press>}</For>
      </div></Show>
    </div>}</Show>
  </div>;
}

export function PanelOffline() {
  return <div class="panel-offline">
    {/* OpenCode's "server" icon with its "circle-x" in the corner */}
    <svg class="offline-icon" width="72" height="72" viewBox="-1 -4 24 24" fill="none" aria-hidden="true">
      <defs><mask id="offline-cut"><rect x="-1" y="-4" width="24" height="24" fill="#fff" /><circle cx="16.64" cy="1.93" r="5.88" fill="#000" /></mask></defs>
      <g mask="url(#offline-cut)" stroke="currentColor">
        <rect x="3.35547" y="1.92969" width="13.2857" height="16.1429" /><rect x="3.35547" y="11.9297" width="13.2857" height="6.14286" />
        <rect x="12.8555" y="14.2852" width="1.42857" height="1.42857" fill="currentColor" stroke="none" /><rect x="10" y="14.2852" width="1.42857" height="1.42857" fill="currentColor" stroke="none" />
      </g>
      <g transform="translate(11.14 -3.57) scale(.55)">
        <path fill-rule="evenodd" clip-rule="evenodd" fill="currentColor" d="M1.6665 10.0003C1.6665 5.39795 5.39746 1.66699 9.99984 1.66699C14.6022 1.66699 18.3332 5.39795 18.3332 10.0003C18.3332 14.6027 14.6022 18.3337 9.99984 18.3337C5.39746 18.3337 1.6665 14.6027 1.6665 10.0003ZM7.49984 6.91107L6.91058 7.50033L9.41058 10.0003L6.91058 12.5003L7.49984 13.0896L9.99984 10.5896L12.4998 13.0896L13.0891 12.5003L10.5891 10.0003L13.0891 7.50033L12.4998 6.91107L9.99984 9.41107L7.49984 6.91107Z" />
      </g>
    </svg>
    <div class="offline-title">{tr().panel.offline}</div>
    <div class="offline-text">{tr().panel.restart}</div>
    <Press label={tr().panel.retry} class="offline-retry" onClick={() => void findServers()}>{tr().panel.retry}</Press>
  </div>;
}
