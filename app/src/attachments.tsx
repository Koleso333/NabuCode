import { createEffect, createSignal, For, on, Show } from "solid-js";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { Glyph } from "./controls";
import { language, tr } from "./i18n";
import { languageOf } from "./chat/highlight";
import { newId, type Attachment } from "./chat/model";

const archives = new Set(["zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst"]);
const documents = new Set(["txt", "log", "md", "csv", "pdf", "doc", "docx", "rtf", "odt"]);

export const folderMime = "inode/directory";
/** The server copies a local file up to this size and gives a bigger one by its path; previews are read up to it too. */
const copyLimit = 100 * 1024 * 1024;

export function iconOf(name: string, mime: string) {
  if (mime === folderMime) return "folder";
  const extension = name.includes(".") ? name.split(".").at(-1)!.toLowerCase() : "";
  if (mime.startsWith("audio/")) return "file-audio";
  if (mime.startsWith("video/")) return "file-video";
  if (archives.has(extension)) return "file-archive";
  if (documents.has(extension) || mime === "text/plain") return "file-text";
  if (languageOf(extension)) return "file-code";
  return "file";
}

function loadsAsImage(url: string) {
  return new Promise<boolean>(resolve => {
    const image = new Image();
    image.onload = () => resolve(true);
    image.onerror = () => resolve(false);
    image.src = url;
  });
}

function snapshot(video: HTMLVideoElement) {
  if (!video.videoWidth) return undefined;
  const scale = 214 / Math.min(video.videoWidth, video.videoHeight);
  const canvas = document.createElement("canvas");
  canvas.width = Math.round(video.videoWidth * scale);
  canvas.height = Math.round(video.videoHeight * scale);
  canvas.getContext("2d")!.drawImage(video, 0, 0, canvas.width, canvas.height);
  return canvas.toDataURL("image/jpeg", .85);
}

// Duration, plus the first frame of a video; formats the webview can't play give neither
function probeMedia(url: string, video: boolean) {
  return new Promise<{ duration?: number; frame?: string }>(resolve => {
    const media = document.createElement(video ? "video" : "audio");
    const finish = (result: { duration?: number; frame?: string }) => {
      clearTimeout(timer);
      media.removeAttribute("src");
      media.load();
      resolve(result);
    };
    const timer = setTimeout(() => finish({}), 5000);
    media.muted = true;
    media.preload = "auto";
    media.onerror = () => finish({});
    media.onloadedmetadata = () => {
      const duration = Number.isFinite(media.duration) ? media.duration : undefined;
      if (!video) return finish({ duration });
      media.onseeked = () => finish({ duration, frame: snapshot(media as HTMLVideoElement) });
      media.currentTime = Math.min(.1, (duration ?? 0) / 2);
    };
    media.src = url;
  });
}

const previewed = (mime: string) => /^(image|video|audio)\//.test(mime);

async function preview(file: File): Promise<Partial<Attachment>> {
  const url = URL.createObjectURL(file);
  if (file.type.startsWith("image/") && await loadsAsImage(url)) return { image: url };
  const video = file.type.startsWith("video/");
  const media = video || file.type.startsWith("audio/") ? await probeMedia(url, video) : {};
  URL.revokeObjectURL(url);
  return media;
}

/** A file without a path (pasted, or on a plain page): its bytes go to the server. */
export async function readAttachment(file: File): Promise<Attachment> {
  return { id: newId("file"), file, name: file.name, size: file.size, mime: file.type, icon: iconOf(file.name, file.type), ...await preview(file) };
}

type DroppedItem = { path: string; name: string; folder: boolean; mime: string; size: number };

// Only the path goes to the server; the shell reads the bytes just for a preview
async function pathAttachment(item: DroppedItem): Promise<Attachment> {
  const mime = item.folder ? folderMime : item.mime;
  const attachment: Attachment = { id: newId("file"), path: item.path, name: item.name, mime, icon: iconOf(item.name, mime) };
  if (item.folder) return attachment;
  attachment.size = item.size;
  if (!previewed(mime) || item.size > copyLimit) return attachment;
  const bytes = await invoke<ArrayBuffer>("read_dropped", { path: item.path }).catch(() => null);
  return bytes ? { ...attachment, ...await preview(new File([bytes], item.name, { type: mime })) } : attachment;
}

async function readPaths(paths: string[]): Promise<Attachment[]> {
  return Promise.all((await invoke<DroppedItem[]>("dropped_items", { paths })).map(pathAttachment));
}

/** The app picks files by path; a plain page has only the browser's picker, which gives bytes. */
export async function pickFiles(fallback: () => void): Promise<Attachment[]> {
  if (!isTauri()) {
    fallback();
    return [];
  }
  const paths = await open({ multiple: true });
  return paths ? readPaths(paths) : [];
}

const localPath = (url: string) => {
  const path = decodeURI(url.slice("file://".length));
  return /^\/[A-Za-z]:/.test(path) ? path.slice(1) : path;
};

/** A file of a queued message back in the composer: local ones come as file:// urls, the server's copies by link. */
export async function restoreAttachment(part: { mime: string; filename?: string; url: string }): Promise<Attachment> {
  const name = part.filename ?? "file";
  if (part.url.startsWith("file://")) {
    const path = localPath(part.url);
    const [found] = isTauri() ? await readPaths([path]).catch(() => []) : [];
    return found ?? { id: newId("file"), path, name, mime: part.mime, icon: iconOf(name, part.mime) };
  }
  const blob = await fetch(part.url).then(response => response.blob());
  return readAttachment(new File([blob], name, { type: part.mime }));
}

/** Files copied in Explorer go by their paths like dropped ones; a screenshot or a copied picture has none and goes as bytes. */
export async function readPasted(transfer: DataTransfer): Promise<Attachment[]> {
  // The browser empties the transfer once the paste event is over, so its files are taken before waiting
  const files = transferFiles(transfer);
  const paths = isTauri() ? await invoke<string[]>("clipboard_files").catch(() => []) : [];
  return paths.length ? readPaths(paths) : Promise.all(files.map(readAttachment));
}

export function readTransfer(transfer: DataTransfer): Promise<Attachment[]> {
  return Promise.all(transferFiles(transfer).map(readAttachment));
}

// The page alone gets no folders, so only files are taken
function transferFiles(transfer: DataTransfer) {
  return [...transfer.items]
    .filter(item => item.kind === "file" && !item.webkitGetAsEntry()?.isDirectory)
    .map(item => item.getAsFile())
    .filter(file => !!file);
}

export function releaseAttachment(attachment: Attachment) {
  if (attachment.image) URL.revokeObjectURL(attachment.image);
}

function formatSize(bytes: number) {
  const units = tr().attachments.units;
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value.toLocaleString(language(), { maximumFractionDigits: unit && value < 10 ? 1 : 0 })} ${units[unit]}`;
}

function formatDuration(seconds: number) {
  const total = Math.round(seconds);
  const pad = (value: number) => String(value).padStart(2, "0");
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor(total / 60) % 60;
  return hours ? `${hours}:${pad(minutes)}:${pad(total % 60)}` : `${minutes}:${pad(total % 60)}`;
}

/** `problem` marks a file that can't be sent and says why. */
export function AttachmentTile(props: { file: Attachment; onRemove?: () => void; problem?: string }) {
  return <div class="attachment" classList={{ framed: !!props.file.frame, unsupported: !!props.problem }}>
    <Show when={props.file.image} fallback={<>
      <Show when={props.file.frame}>{frame => <img src={frame()} alt="" draggable={false} />}</Show>
      <span class="attachment-icon"><Glyph name={props.file.icon} size={30} /></span>
      <Show when={props.file.size !== undefined}><span class="attachment-size">{formatSize(props.file.size!)}</span></Show>
      <Show when={props.file.duration !== undefined}><span class="attachment-duration">{formatDuration(props.file.duration!)}</span></Show>
      <span class="attachment-name">{props.file.name}</span>
    </>}>{image => <img src={image()} alt={props.file.name} draggable={false} />}</Show>
    <Show when={props.problem}><span class="attachment-unsupported">{props.problem}</span></Show>
    <Show when={props.onRemove}>
      <button type="button" class="attachment-remove" aria-label={tr().attachments.remove(props.file.name)} onClick={() => props.onRemove!()}>
        <Glyph name="close" size={12} />
      </button>
    </Show>
  </div>;
}

const tileSize = 107;
const tileGap = 8;
const trayEdge = 9;
const trayWidest = 392;

// The list is a vertical scroller turned by -90°, so the mouse wheel scrolls it natively; tiles are turned back
export function AttachmentTray(props: { files: Attachment[]; onRemove: (id: string) => void; problem: (file: Attachment) => string | undefined }) {
  let list!: HTMLDivElement;
  const [edges, setEdges] = createSignal({ start: true, end: true });
  const width = () => Math.min(trayWidest, trayEdge * 2 + props.files.length * (tileSize + tileGap) - tileGap);
  const check = () => setEdges({ start: list.scrollTop < 1, end: list.scrollHeight - list.scrollTop - list.clientHeight < 1 });
  // A sideways touchpad swipe or Shift+wheel would scroll across the turned list, so it is passed along by hand
  const sideways = (event: WheelEvent) => {
    const delta = event.shiftKey ? event.deltaY || event.deltaX : Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : 0;
    if (!delta) return;
    event.preventDefault();
    list.scrollTop += delta;
  };
  createEffect(on(() => props.files.length, (count, previous = 0) => requestAnimationFrame(() => {
    if (count > previous) list.scrollTop = list.scrollHeight;
    check();
  })));
  return <div class="attachment-tray panel-frame" classList={{ "at-start": edges().start, "at-end": edges().end }}
    style={{ width: `min(${width()}px, calc(var(--composer-width) - var(--chip-width) - 39px))` }}>
    <div class="attachment-view">
      <div ref={list} class="attachment-list" onScroll={check} onWheel={sideways}>
        <For each={props.files}>{file => <AttachmentTile file={file} onRemove={() => props.onRemove(file.id)} problem={props.problem(file)} />}</For>
      </div>
    </div>
    <div class="attachment-fade start" /><div class="attachment-fade end" />
  </div>;
}

type DropOptions = { enabled: () => boolean; onDragging: (dragging: boolean) => void; onDrop: (attachments: Promise<Attachment[]>) => void };

// Call inside onMount. In the app the window catches drops and gives paths; a plain page gets them as files
export function watchFileDrops(options: DropOptions) {
  return isTauri() ? watchWindowDrops(options) : watchPageDrops(options);
}

function watchWindowDrops(options: DropOptions) {
  let stop: (() => void) | undefined;
  let stopped = false;
  void getCurrentWebview().onDragDropEvent(({ payload }) => {
    if (payload.type === "enter") options.onDragging(options.enabled() && payload.paths.length > 0);
    if (payload.type === "leave") options.onDragging(false);
    if (payload.type !== "drop") return;
    options.onDragging(false);
    if (options.enabled() && payload.paths.length) options.onDrop(readPaths(payload.paths));
  }).then(unlisten => stopped ? unlisten() : stop = unlisten);
  return () => {
    stopped = true;
    stop?.();
  };
}

// The page would open a dropped file by itself, so every file drop is caught
function watchPageDrops(options: DropOptions) {
  let depth = 0;
  const carriesFiles = (event: DragEvent) => !!event.dataTransfer?.types.includes("Files");
  const enter = (event: DragEvent) => {
    if (!carriesFiles(event)) return;
    depth++;
    options.onDragging(options.enabled());
  };
  const leave = (event: DragEvent) => {
    if (!carriesFiles(event)) return;
    depth = Math.max(0, depth - 1);
    if (!depth) options.onDragging(false);
  };
  const over = (event: DragEvent) => {
    if (!carriesFiles(event)) return;
    event.preventDefault();
    event.dataTransfer!.dropEffect = options.enabled() ? "copy" : "none";
  };
  const drop = (event: DragEvent) => {
    if (!carriesFiles(event)) return;
    event.preventDefault();
    depth = 0;
    options.onDragging(false);
    if (options.enabled()) options.onDrop(readTransfer(event.dataTransfer!));
  };
  window.addEventListener("dragenter", enter);
  window.addEventListener("dragleave", leave);
  window.addEventListener("dragover", over);
  window.addEventListener("drop", drop);
  return () => {
    window.removeEventListener("dragenter", enter);
    window.removeEventListener("dragleave", leave);
    window.removeEventListener("dragover", over);
    window.removeEventListener("drop", drop);
  };
}

export function DropOverlay() {
  return <div class="drop-overlay"><span class="drop-label panel-frame">{tr().attachments.drop}</span></div>;
}
