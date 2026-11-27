import { createSignal, For, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { tr } from "./i18n";
import { ConfirmDialog, Dialog, Press } from "./controls";
import { preferences, setPreference } from "./preferences";
import { showFailure } from "./errors";
import { demoMode } from "./chat/actions";

export type UpdateChannel = "stable" | "beta" | "nightly";
export const updateChannels: UpdateChannel[] = ["stable", "beta", "nightly"];
type Update = { version: string; notes: string | null };

// The notes of the update being installed wait here for the first start of the new version
const notesKey = "everyend-update-notes";
const demoUpdate: Update = { version: "0.1.2", notes: "- Updates: Stable, Beta or Nightly\n- Chrome extension setup in Settings → Browser" };

const [version, setVersion] = createSignal("");
const [supported, setSupported] = createSignal(false);
const [found, setFound] = createSignal<Update | null>(null);
const [offered, setOffered] = createSignal(false);
const [progress, setProgress] = createSignal<number | null>(null);
const [notes, setNotes] = createSignal<Update | null>(null);
export { found as foundUpdate, supported as updatesSupported };

export const channelName = (channel: UpdateChannel) => channel[0].toUpperCase() + channel.slice(1);
// The same rule as scripts/release.ps1
const channelOf = (version: string): UpdateChannel => version.includes("-nightly") ? "nightly" : version.includes("-") ? "beta" : "stable";
export const updateChannel = () => preferences.updateChannel || channelOf(version());
export const releaseName = (update: Update) => `v${update.version} ${channelName(channelOf(update.version))}`;

export function setUpdateChannel(channel: UpdateChannel) {
  setPreference("updateChannel", channel);
  setFound(null);
}

export async function startUpdates() {
  if (demoMode) {
    setVersion("0.1.1");
    setSupported(true);
  } else {
    // Builds from source are marked -dev; their channel is still the release they were cut from
    setVersion((await invoke<string>("app_version")).replace(/-dev$/, ""));
    setSupported(await invoke<boolean>("updates_supported"));
    void listen<{ done: number; total: number | null }>("update-progress", event => setProgress(event.payload.total ? event.payload.done / event.payload.total : 0));
    void listen<string>("update-failed", event => failInstall(event.payload));
  }
  showSavedNotes();
  // Just updated: the start shows what is new instead of looking for the next release
  if (!notes() && supported() && preferences.checkOnStartup) void checkUpdates().catch(() => {});
}

export async function checkUpdates() {
  const next = demoMode ? demoUpdate : await invoke<Update | null>("update_check", { channel: updateChannel() });
  setFound(next);
  setOffered(!!next);
  return next;
}

// Ends with NabuCode closing for the installer, which starts the new version
export async function installUpdate() {
  const update = found();
  if (!update) return;
  setOffered(false);
  setProgress(0);
  localStorage.setItem(notesKey, JSON.stringify(update));
  if (demoMode) return;
  await invoke("update_install").catch(failInstall);
}

function cancelInstall() {
  if (!demoMode) void invoke("update_cancel");
  setProgress(null);
  localStorage.removeItem(notesKey);
}

function failInstall(cause: unknown) {
  setProgress(null);
  localStorage.removeItem(notesKey);
  showFailure(cause);
}

function showSavedNotes() {
  const saved = localStorage.getItem(notesKey);
  localStorage.removeItem(notesKey);
  if (!saved || !preferences.releaseNotes) return;
  try {
    const update = JSON.parse(saved) as Update;
    if (update.version === version() && update.notes) setNotes(update);
  } catch {}
}

const points = (notes: string) => notes.split("\n").filter(line => line.startsWith("- ")).map(line => line.slice(2));

export function UpdateDialogs() {
  const text = () => tr().updates;
  const [aborting, setAborting] = createSignal(false);
  return <>
    <Show when={offered() && found()}>{update => <Dialog title={text().title(update().version)} onClose={() => setOffered(false)}>
      <p>{text().found(releaseName(update()))}</p>
      <div class="confirm-actions">
        <Press label={text().later} class="confirm-button" onClick={() => setOffered(false)}>{text().later}</Press>
        <button type="button" class="press confirm-button" data-variant="primary" onClick={() => void installUpdate()}>{text().install}</button>
      </div>
    </Dialog>}</Show>
    <Show when={progress() !== null}>
      <Dialog title={text().installing} onCloseButton={() => setAborting(true)}>
        <div class="meter update-meter"><div class="meter-fill" style={{ width: `${(progress() ?? 0) * 100}%` }} /></div>
      </Dialog>
    </Show>
    <Show when={aborting()}><ConfirmDialog title={text().abortTitle} description={text().abortDescription} cancel={text().continue} action={text().abort} danger
      onConfirm={cancelInstall} onClose={() => setAborting(false)} /></Show>
    <Show when={notes()}>{update => <Dialog title={text().notesTitle(update().version)} onClose={() => setNotes(null)}>
      <ul class="update-notes"><For each={points(update().notes ?? "")}>{point => <li>{point}</li>}</For></ul>
      <div class="confirm-actions">
        <button type="button" class="press confirm-button" data-variant="primary" onClick={() => setNotes(null)}>{text().understood}</button>
      </div>
    </Dialog>}</Show>
  </>;
}
