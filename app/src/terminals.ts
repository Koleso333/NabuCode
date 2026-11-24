import { createSignal } from "solid-js";
import { createStore } from "solid-js/store";
import type { Shell, TerminalInfo } from "@nabu/protocol";
import { onConnected, onServerEvent, request } from "./server";
import { preferences } from "./preferences";
import { dropView, fitSize } from "./terminal-view";

const [shells, setShells] = createSignal<Shell[]>([]);
// Live terminals of each project folder in tab order, and the tab shown in each
const [lists, setLists] = createStore<Record<string, TerminalInfo[]>>({});
const [shown, setShown] = createStore<Record<string, string>>({});
// The panel's screen area, where a new terminal's size is fitted
let screen: HTMLElement | undefined;
export const setTerminalScreen = (element: HTMLElement) => { screen = element; };
export { shells };

export const terminalsOf = (directory: string) => lists[directory] ?? [];

export const activeTerminal = (directory: string) => {
  const list = terminalsOf(directory);
  return list.find(item => item.id === shown[directory]) ?? list.at(-1);
};

export const showTerminal = (directory: string, id: string) => setShown(directory, id);

// A loaded folder is reloaded on every reconnect
export async function loadTerminals(directory: string) {
  if (!lists[directory]) setLists(directory, []);
  const list = await request("terminalList", { directory });
  const live = new Set(list.map(item => item.id));
  for (const item of terminalsOf(directory)) if (!live.has(item.id)) dropView(item.id);
  setLists(directory, list);
}

/** Starts `shell`, or the default one from the settings, at the size of the panel's screen. */
export async function openTerminal(directory: string, choice = preferences.shell) {
  const shell = shells().find(item => item.id === choice)?.id;
  // The panel may be mounting in the same tick
  await new Promise(requestAnimationFrame);
  const size = screen?.isConnected ? await fitSize(screen) : { cols: 80, rows: 24 };
  const info = await request("terminalCreate", { directory, shell, highlight: preferences.syntax, ...size });
  setLists(directory, list => [...(list ?? []), info]);
  setShown(directory, info.id);
  return info;
}

export function closeTerminal(id: string) {
  forget(id);
  return request("terminalClose", { id });
}

// The tab to the right takes the place of a closed one, or the one to the left at the end
function forget(id: string) {
  for (const [directory, list] of Object.entries(lists)) {
    const index = list.findIndex(item => item.id === id);
    if (index < 0) continue;
    const rest = list.filter(item => item.id !== id);
    if (shown[directory] === id && rest.length) setShown(directory, rest[Math.min(index, rest.length - 1)].id);
    setLists(directory, rest);
  }
  dropView(id);
}

onServerEvent(event => { if (event.type === "terminalExited") forget(event.id); });

// A restarted server has lost its terminals
onConnected(() => {
  void request("terminalShells").then(setShells, () => {});
  for (const directory of Object.keys(lists)) void loadTerminals(directory).catch(() => {});
});
