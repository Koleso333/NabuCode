import { createEffect, createMemo, createResource, createSignal, on, Show } from "solid-js";
import type { FileEntry, GitChange } from "@nabu/protocol";
import { ContextMenu, Glyph, IconButton, MenuItem, Press, Ring } from "./controls";
import { VirtualList } from "./virtual-list";
import { tr } from "./i18n";
import { iconOf } from "./attachments";
import { Scrollbar } from "./scrollbar";
import { localServer, request } from "./server";
import { Diff, DiffCounts } from "./chat/diff";
import { changedFiles } from "./chat/parts";
import type { Message } from "./chat/model";
import { childPath, collapseAll, fileTree, fullPath, nativePath, openInEditor, revealInExplorer, showFiles, toggleFolder } from "./files";

export type FilesTab = "changes" | "files";
export type ChangesSource = "git" | "session";
/** A file's diff opened over the chat. */
export type OpenedDiff = { path: string; patch: string; change: Change };

/** A changed file in either source; a session file's patch comes with it, a git one is asked for when opened. */
type Change = { path: string; status: GitChange["status"]; additions: number; deletions: number; patch?: string };

const statusColor: Record<GitChange["status"], string> = { M: "var(--warning)", R: "var(--warning)", A: "var(--success)", U: "var(--success)", D: "var(--critical)" };
const fileName = (path: string) => path.split("/").at(-1)!;
const folderOf = (path: string) => path.includes("/") ? path.slice(0, path.lastIndexOf("/") + 1) : "";
/** Diff lines an inline card shows; the expanded view has all of them. */
const CARD_LINES = 200;
const ROW_HEIGHT = 23;

/** A line of the flattened tree: an entry, or the note in an open empty folder. */
type TreeRow = { path: string; depth: number; dim: boolean; entry?: FileEntry };

/** What the session's edit and write calls changed, one entry per file with their patches in order. */
function sessionChanges(messages: Message[]) {
  const byPath = new Map<string, Change>();
  for (const message of messages) for (const part of message.parts) {
    if (part.type !== "tool") continue;
    for (const file of changedFiles(part)) {
      const known = byPath.get(file.relativePath);
      if (known) Object.assign(known, { additions: known.additions + file.additions, deletions: known.deletions + file.deletions, patch: `${known.patch}\n${file.patch}` });
      else byPath.set(file.relativePath, { path: file.relativePath, status: file.type === "add" ? "A" : file.type === "delete" ? "D" : "M", additions: file.additions, deletions: file.deletions, patch: file.patch });
    }
  }
  return [...byPath.values()];
}

export function FilePanel(props: {
  directory: string; tab: FilesTab; onTab: (tab: FilesTab) => void; width: number; bottom: number;
  onWidth: (width: number) => void; messages?: Message[]; onExpand: (diff: OpenedDiff) => void;
}) {
  const text = () => tr().files;
  const [source, setSource] = createSignal<ChangesSource>("git");
  const [menu, setMenu] = createSignal<{ x: number; y: number; path: string; dir?: boolean; deleted?: boolean } | null>(null);
  const [more, setMore] = createSignal<{ x: number; y: number } | null>(null);
  // Paths, so a card stays open when git reports the file again
  const [expanded, setExpanded] = createSignal(new Set<string>());
  let list!: HTMLDivElement;
  createEffect(() => void showFiles(props.directory));

  const tree = () => fileTree(props.directory);
  const gitChanges = createMemo((): Change[] => tree()?.changes ?? []);
  const changes = createMemo(() => source() === "git" ? gitChanges() : sessionChanges(props.messages ?? []));
  const changeOf = createMemo(() => new Map(gitChanges().map(change => [change.path, change])));
  const changedFolders = createMemo(() => {
    const folders = new Set<string>();
    for (const change of gitChanges()) for (let end = change.path.indexOf("/"); end > 0; end = change.path.indexOf("/", end + 1)) folders.add(change.path.slice(0, end));
    return folders;
  });
  const changedFolder = (path: string) => changedFolders().has(path);
  // Open folders laid out as one list, so only the visible rows are drawn
  const treeRows = createMemo(() => {
    const current = tree();
    const rows: TreeRow[] = [];
    const walk = (folder: string, depth: number, dim: boolean) => {
      const entries = current?.folders[folder];
      if (!entries) return;
      if (!entries.length && folder) rows.push({ path: `${folder}/`, depth, dim });
      for (const entry of entries) {
        const path = childPath(folder, entry.name);
        const dimmed = dim || entry.ignored;
        rows.push({ path, depth, dim: dimmed, entry });
        if (entry.dir && current!.open[path]) walk(path, depth + 1, dimmed);
      }
    };
    walk("", 0, false);
    return rows;
  });
  const loading = () => props.tab === "files" ? !tree()?.folders[""] : source() === "git" && !tree()?.ready;
  createEffect(on([() => props.tab, source, () => props.directory], () => { list.scrollTop = 0; }, { defer: true }));

  const full = (path: string) => fullPath(props.directory, path);
  const open = (path: string) => { if (localServer()) void openInEditor(full(path)).catch(() => {}); };
  const showMenu = (event: MouseEvent, path: string, dir?: boolean, deleted?: boolean) => {
    event.preventDefault();
    setMenu({ x: event.clientX, y: event.clientY, path, dir, deleted });
  };
  // The action reads the menu's target, so it runs before the menu closes
  const act = (action: () => unknown) => { const done = new Promise(resolve => resolve(action())); setMenu(null); void done.catch(() => {}); };

  function resize(event: PointerEvent) {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = props.width;
    const handle = event.currentTarget as HTMLElement;
    handle.setPointerCapture(event.pointerId);
    const move = (next: PointerEvent) => props.onWidth(startWidth - (next.clientX - startX));
    const stop = () => { handle.removeEventListener("pointermove", move); handle.removeEventListener("pointerup", stop); handle.removeEventListener("pointercancel", stop); };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }

  function TreeLine(row: { row: TreeRow }) {
    const { path, depth, dim, entry } = row.row;
    if (!entry) return <div class="file-empty" style={{ "padding-left": `${30 + depth * 14}px` }}>{text().empty}</div>;
    const opened = () => !!tree()?.open[path];
    const change = () => changeOf().get(path);
    const color = () => dim ? "var(--muted)" : change() ? statusColor[change()!.status] : undefined;
    const nameColor = () => color() ?? (entry.dir && changedFolder(path) ? "var(--strong)" : undefined);
    return <Press label={entry.name} class="file-row" style={{ "padding-left": `${6 + depth * 14}px` }}
      onClick={() => entry.dir ? void toggleFolder(props.directory, path) : open(path)} onContextMenu={event => showMenu(event, path, entry.dir)}>
      <span class="file-chevron"><Show when={entry.dir}><Glyph name={opened() ? "chevron-down" : "chevron-right"} size={16} /></Show></span>
      <Glyph name={entry.dir ? "folder" : iconOf(entry.name, "")} size={16} color={color()} />
      <span class="file-name truncate" style={{ color: nameColor() }}>{entry.name}</span>
      <Show when={!dim && change()}><span class="file-status" style={{ color: color() }}>{change()!.status}</span></Show>
      <Show when={!dim && entry.dir && changedFolder(path)}><span class="file-dot" /></Show>
    </Press>;
  }

  function ChangeRow(row: { change: Change }) {
    const open = () => expanded().has(row.change.path);
    const toggle = () => setExpanded(paths => { const next = new Set(paths); if (!next.delete(row.change.path)) next.add(row.change.path); return next; });
    // Asked again when git reports new counts, so an open diff follows the file
    const [patch] = createResource(() => open() && [row.change.path, row.change.additions, row.change.deletions] as const,
      ([path]) => row.change.patch ?? request("gitDiff", { directory: props.directory, path }));
    const color = () => statusColor[row.change.status];
    return <>
      <Press label={row.change.path} class="file-row change-row" selected={open()} onClick={toggle}
        onContextMenu={event => showMenu(event, row.change.path, false, row.change.status === "D")}>
        <Glyph name={iconOf(row.change.path, "")} size={16} color={color()} />
        <span class="file-name truncate"><span style={{ color: color(), "text-decoration": row.change.status === "D" ? "line-through" : undefined }}>{fileName(row.change.path)}</span> <span class="file-folder">{nativePath(props.directory, folderOf(row.change.path))}</span></span>
        <DiffCounts additions={row.change.additions} deletions={row.change.deletions} />
      </Press>
      <Show when={open() && patch.loading}><div class="diff-card file-loading"><Ring fraction={.68} size={16} loading /></div></Show>
      <Show when={open() && !patch.loading && patch() !== undefined}>
        <div class="diff-card">
          <Show when={patch()!.includes("@@")} fallback={<div class="diff-card-empty">{text().binary}</div>}><Diff patch={patch()!} path={row.change.path} limit={CARD_LINES} /></Show>
          <IconButton name="expand" label={text().expand} size={15} width={23} class="diff-card-expand"
            onClick={() => props.onExpand({ path: row.change.path, patch: patch()!, change: row.change })} />
        </div>
      </Show>
    </>;
  }

  return <aside class="files-panel" style={{ width: `${props.width}px`, bottom: `${props.bottom}px` }}>
    <div class="files-panel-resize" onPointerDown={resize} />
    <div class="files-panel-head">
      <Press label={text().changes} class="file-tab" selected={props.tab === "changes"} onClick={() => props.onTab("changes")}>
        {text().changes}<Show when={gitChanges().length}><span class="file-tab-count">{gitChanges().length}</span></Show>
      </Press>
      <Press label={text().all} class="file-tab" selected={props.tab === "files"} onClick={() => props.onTab("files")}>{text().all}</Press>
      <span class="files-panel-space" />
      <IconButton name="dots-vertical" label={text().more} size={15} width={23} class="files-panel-more" selected={!!more()}
        onClick={() => { const box = list.getBoundingClientRect(); setMore({ x: box.right - 4, y: box.top }); }} />
    </div>
    <Show when={props.tab === "changes"}>
      <div class="file-switch">
        <Press label={text().git} selected={source() === "git"} onClick={() => setSource("git")}>{text().git}</Press>
        <Press label={text().session} selected={source() === "session"} onClick={() => setSource("session")}>{text().session}</Press>
      </div>
    </Show>
    <div class="file-list-frame">
      <div ref={list} class="files-scroller">
        <Show when={!loading()} fallback={<div class="file-loading"><Ring fraction={.68} size={16} loading /></div>}>
          <Show when={props.tab === "files"}>
            <Show when={treeRows().length} fallback={<div class="file-empty centered">{text().empty}</div>}>
              <VirtualList items={treeRows()} key={row => row.path} rowHeight={ROW_HEIGHT} scroller={() => list}>{row => <TreeLine row={row} />}</VirtualList>
            </Show>
          </Show>
          <Show when={props.tab === "changes"}>
            <Show when={changes().length} fallback={<div class="file-empty centered">{source() === "git" ? text().noChanges : props.messages ? text().noSessionChanges : text().noSession}</div>}>
              <VirtualList items={changes()} key={change => change.path} rowHeight={ROW_HEIGHT} scroller={() => list}>{change => <ChangeRow change={change} />}</VirtualList>
            </Show>
          </Show>
        </Show>
      </div>
      <Scrollbar target={() => list} class="file-scroll" />
    </div>
    <Show when={menu()}>{target => <ContextMenu x={target().x} y={target().y} label={target().path} onClose={() => setMenu(null)}>
      <Show when={localServer() && !target().deleted}>
        <Show when={!target().dir}><MenuItem icon="open-out" label={text().open} onClick={() => act(() => openInEditor(full(target().path)))} /></Show>
        <MenuItem icon="explorer" label={text().reveal} onClick={() => act(() => revealInExplorer(full(target().path)))} />
        <div class="context-menu-separator" role="separator" />
      </Show>
      <MenuItem icon="copy" label={text().copyPath} onClick={() => act(() => navigator.clipboard.writeText(full(target().path)))} />
      <MenuItem icon="copy" label={text().copyRelative} onClick={() => act(() => navigator.clipboard.writeText(nativePath(props.directory, target().path)))} />
    </ContextMenu>}</Show>
    <Show when={more()}>{position => <ContextMenu x={position().x - 234} y={position().y} label={text().more} onClose={() => setMore(null)}>
      <MenuItem icon="minus" label={text().collapseAll} onClick={() => { setMore(null); collapseAll(props.directory); props.onTab("files"); }} />
    </ContextMenu>}</Show>
  </aside>;
}

/** A file's diff in full size over the chat. */
export function DiffView(props: { diff: OpenedDiff; directory: string; onClose: () => void }) {
  const color = () => statusColor[props.diff.change.status];
  return <section class="diff-view">
    <div class="diff-view-head">
      <Glyph name={iconOf(props.diff.path, "")} size={16} color={color()} />
      <span class="diff-view-name">{fileName(props.diff.path)}</span>
      <span class="file-folder">{nativePath(props.directory, folderOf(props.diff.path))}</span>
      <DiffCounts additions={props.diff.change.additions} deletions={props.diff.change.deletions} />
      <span class="files-panel-space" />
      <IconButton name="close" label={tr().files.close} size={15} width={25} class="diff-view-close" onClick={props.onClose} />
    </div>
    <div class="diff-view-body"><Diff patch={props.diff.patch} path={props.diff.path} /></div>
  </section>;
}
