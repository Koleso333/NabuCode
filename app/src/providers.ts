import { createSignal } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import type { Account, ProviderEntry, UsageBar } from "@nabu/protocol";
import { onConnected, onServerEvent, request } from "./server";

const [entries, setEntries] = createStore<ProviderEntry[]>([]);
const [accounts, setAccounts] = createStore<Record<string, Account[]>>({});
const [usage, setUsage] = createStore<Record<string, UsageBar[]>>({});
// Providers whose accounts and usage are on screen, refreshed on every change
const watched = new Set<string>();
const loginListeners = new Set<(provider: string, message: string) => void>();

// Entries keep their identity across reloads, so rows are not rebuilt
export const catalog = () => entries;
export const accountsOf = (id: string) => accounts[id] ?? [];
export const usageOf = (id: string) => usage[id] ?? [];
export const entryOf = (id: string) => catalog().find(entry => entry.id === id);

async function loadCatalog() {
  const list = await request("providerCatalog").catch(() => undefined);
  if (list) setEntries(reconcile(list, { key: "id" }));
}

async function refresh(id: string) {
  if (entryOf(id)?.auth === "custom") return;
  const [list, bars] = await Promise.all([request("providerAccounts", { provider: id }), request("providerUsage", { provider: id })]).catch(() => []);
  if (list) setAccounts(id, list);
  if (bars) setUsage(id, bars);
}

/** Loads a provider's accounts and usage and keeps them current. */
export function watchProvider(id: string) {
  watched.add(id);
  void refresh(id);
}

/** Calls back when a browser sign-in fails; returns the unsubscribe function. */
export function onLoginFailed(listener: (provider: string, message: string) => void) {
  loginListeners.add(listener);
  return () => void loginListeners.delete(listener);
}

onConnected(() => {
  void loadCatalog();
  watched.forEach(id => void refresh(id));
});
onServerEvent(event => {
  if (event.type === "providersChanged") void loadCatalog();
  if (event.type === "providerUpdated") {
    void loadCatalog();
    if (watched.has(event.provider)) void refresh(event.provider);
  }
  if (event.type === "providerLoginFailed") loginListeners.forEach(listener => listener(event.provider, event.message));
});
