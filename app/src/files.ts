import { createStore, produce, reconcile } from "solid-js/store";
import { invoke } from "@tauri-apps/api/core";
import type { FileEntry, GitChange } from "@nabu/protocol";
import { onConnected, onServerEvent, request, ServerError } from "./server";
import { preferences } from "./preferences";
import { samePath } from "./projects";

/** One project's tree: loaded folders by path relative to the project ("" is the root), open folders, git changes and whether they came yet. */
type Tree = { directory: string; folders: Record<string, FileEntry[]>; open: Record<string, boolean>; changes: GitChange[]; ready: boolean };

const [trees, setTrees] = createStore<Record<string, Tree>>({});

const treeKey = (directory: string) => Object.keys(trees).find(key => samePath(key, directory)) ?? directory;
export const fileTree = (directory: string) => trees[treeKey(directory)] as Tree | undefined;

export const childPath = (folder: string, name: string) => folder ? `${folder}/${name}` : name;

/** Shows what is known at once and asks the server again; the server then reports changes on disk. */
export async function showFiles(directory: string) {
  if (!fileTree(directory)) {
    setTrees(directory, { directory, folders: {}, open: {}, changes: [], ready: false });
    await request("filesWatch", { directory }).catch(() => {});
  }
  await refresh(directory);
}

export async function toggleFolder(directory: string, path: string) {
  const key = treeKey(directory);
  if (trees[key]?.open[path]) return setTrees(key, "open", produce(open => { delete open[path]; }));
  setTrees(key, "open", path, true);
  if (!trees[key].folders[path]) await loadFolder(key, path);
}

export function collapseAll(directory: string) {
  setTrees(treeKey(directory), produce(tree => { tree.open = {}; }));
}

// A folder the server can't read is dropped with its open mark; a lost connection keeps what was shown
async function loadFolder(key: string, path: string) {
  try {
    const entries = await request("fileList", { directory: trees[key].directory, path });
    setTrees(key, "folders", path, reconcile(entries, { key: "name" }));
  } catch (error) {
    if (error instanceof ServerError) setTrees(key, produce(tree => { delete tree.folders[path]; delete tree.open[path]; }));
  }
}

// One refresh per tree at a time; changes that come during it are picked up by one more run
const refreshing = new Map<string, { again: boolean }>();

async function refresh(directory: string) {
  const key = treeKey(directory);
  const running = refreshing.get(key);
  if (running) return void (running.again = true);
  const state = { again: true };
  refreshing.set(key, state);
  try {
    while (state.again) {
      state.again = false;
      await reload(key);
    }
  } finally {
    refreshing.delete(key);
  }
}

async function reload(key: string) {
  const tree = trees[key];
  if (!tree) return;
  const folders = ["", ...Object.keys(tree.open).filter(path => tree.open[path])];
  const changes = request("gitChanges", { directory: tree.directory }).catch(() => [] as GitChange[]);
  await Promise.all(folders.map(path => loadFolder(key, path)));
  // Unchanged files keep their rows; a big repo would redraw thousands of them every refresh
  setTrees(key, "changes", reconcile(await changes, { key: "path" }));
  setTrees(key, "ready", true);
}

onServerEvent(event => { if (event.type === "filesChanged") void refresh(event.directory); });
// A restarted server has no watchers and may have missed changes
onConnected(() => {
  for (const tree of Object.values(trees)) {
    void request("filesWatch", { directory: tree.directory }).catch(() => {});
    void refresh(tree.directory);
  }
});

/** The project's path joined the way the project path is written, for copying and opening. */
export function fullPath(directory: string, path: string) {
  const separator = directory.includes("\\") ? "\\" : "/";
  return `${directory.replace(/[\\/]+$/, "")}${separator}${path.replaceAll("/", separator)}`;
}

export const nativePath = (directory: string, path: string) => directory.includes("\\") ? path.replaceAll("/", "\\") : path;

export type Editor = { path: string; name: string; icon: string };

export const findEditors = () => invoke<Editor[]>("editors");
export const describeEditor = (path: string) => invoke<Editor>("editor_info", { path });
export const openInEditor = (file: string) => invoke("open_in_editor", { file, editor: preferences.editor.path });
export const revealInExplorer = (path: string) => invoke("reveal_in_explorer", { path });
