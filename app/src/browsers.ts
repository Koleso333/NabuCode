import { createStore } from "solid-js/store";
import type { BrowserTab } from "@nabu/protocol";
import { onConnected, onServerEvent, request } from "./server";
import { dropBrowserView } from "./browser-view";
import type { SearchEngine } from "./preferences";

// Icons in public/icons/engines
export const searchEngines: Record<SearchEngine, { name: string; url: string }> = {
  google: { name: "Google", url: "https://www.google.com/search?q=%s" },
  bing: { name: "Bing", url: "https://www.bing.com/search?q=%s" },
  duckduckgo: { name: "DuckDuckGo", url: "https://duckduckgo.com/?q=%s" },
  brave: { name: "Brave Search", url: "https://search.brave.com/search?q=%s" },
  ecosia: { name: "Ecosia", url: "https://www.ecosia.org/search?q=%s" },
  startpage: { name: "Startpage", url: "https://www.startpage.com/sp/search?query=%s" },
};

// Tabs of each chat by its session id, and the tab shown in each
const [lists, setLists] = createStore<Record<string, BrowserTab[]>>({});
const [shown, setShown] = createStore<Record<string, string>>({});

export const browserTabsOf = (session: string) => lists[session] ?? [];

export const activeBrowserTab = (session: string) => {
  const list = browserTabsOf(session);
  return list.find(item => item.id === shown[session]) ?? list.at(-1);
};

export const showBrowserTab = (session: string, id: string) => setShown(session, id);

export async function loadBrowserTabs(session: string) {
  if (!lists[session]) setLists(session, []);
  update(session, await request("browserTabs", { sessionID: session }));
}

export async function openBrowserTab(session: string, url = "") {
  const tab = await request("browserOpen", { sessionID: session, url });
  if (!browserTabsOf(session).some(item => item.id === tab.id)) setLists(session, list => [...(list ?? []), tab]);
  setShown(session, tab.id);
  return tab;
}

// The tab to the right takes the place of a closed one, or the one to the left at the end
export function closeBrowserTab(session: string, id: string) {
  const list = browserTabsOf(session);
  const index = list.findIndex(item => item.id === id);
  const rest = list.filter(item => item.id !== id);
  if (shown[session] === id && rest.length) setShown(session, rest[Math.min(index, rest.length - 1)].id);
  update(session, rest);
  return request("browserClose", { id });
}

function update(session: string, tabs: BrowserTab[]) {
  const live = new Set(tabs.map(item => item.id));
  for (const item of browserTabsOf(session)) if (!live.has(item.id)) dropBrowserView(item.id);
  setLists(session, tabs);
}

onServerEvent(event => { if (event.type === "browserTabsChanged") update(event.sessionID, event.tabs); });

// A restarted server has lost its tabs
onConnected(() => {
  for (const session of Object.keys(lists)) void loadBrowserTabs(session).catch(() => {});
});
