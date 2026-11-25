import { readAttachment, readPaths } from "./attachments";
import type { Attachment } from "./chat/model";
import { hasContent, type Piece } from "./marks";

/** A file kept by its path, or by its bytes in IndexedDB under the attachment's id. */
type SavedFile = { id: string; path?: string; name: string };
type Draft = { pieces: Piece[]; files: SavedFile[] };

const storageKey = "everyend-drafts";
const drafts: Record<string, Draft> = (() => {
  try { return JSON.parse(localStorage.getItem(storageKey) || "{}"); } catch { return {}; }
})();

export const draftOf = (key: string): Draft | undefined => drafts[key];

/** Without files the draft keeps the ones saved before: they are still being read back. */
export function keepDraft(key: string, pieces: Piece[], files?: Attachment[]) {
  const before = drafts[key]?.files ?? [];
  const saved = files ? files.flatMap(savedFile) : before;
  if (hasContent(pieces) || saved.length) drafts[key] = { pieces, files: saved };
  else delete drafts[key];
  localStorage.setItem(storageKey, JSON.stringify(drafts));
  if (!files || sameIds(saved, before)) return;
  for (const file of files) if (!file.path && file.file) void run("readwrite", store => store.put(file.file!, file.id)).catch(() => {});
  void dropUnused();
}

export async function restoreFiles(files: SavedFile[]): Promise<Attachment[]> {
  const read = await Promise.all(files.map(async (saved): Promise<Attachment | undefined> => {
    if (saved.path) {
      const [found] = await readPaths([saved.path]).catch(() => []);
      return found && { ...found, id: saved.id };
    }
    const bytes = await run<File | undefined>("readonly", store => store.get(saved.id)).catch(() => undefined);
    return bytes && { ...await readAttachment(bytes), id: saved.id };
  }));
  return read.filter(file => !!file);
}

const savedFile = (file: Attachment): SavedFile[] =>
  file.path ? [{ id: file.id, path: file.path, name: file.name }] : file.file ? [{ id: file.id, name: file.name }] : [];
const sameIds = (a: SavedFile[], b: SavedFile[]) => a.length === b.length && a.every((file, index) => file.id === b[index].id);

const database = new Promise<IDBDatabase>((resolve, reject) => {
  const opening = indexedDB.open("everyend-drafts", 1);
  opening.onupgradeneeded = () => opening.result.createObjectStore("files");
  opening.onsuccess = () => resolve(opening.result);
  opening.onerror = () => reject(opening.error);
});

async function run<T>(mode: IDBTransactionMode, action: (store: IDBObjectStore) => IDBRequest): Promise<T> {
  const db = await database;
  return new Promise((resolve, reject) => {
    const request = action(db.transaction("files", mode).objectStore("files"));
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

// Bytes no draft holds any more
async function dropUnused() {
  const ids = await run<IDBValidKey[]>("readonly", store => store.getAllKeys()).catch(() => []);
  const used = new Set(Object.values(drafts).flatMap(draft => draft.files.map(file => file.id)));
  for (const id of ids) if (!used.has(String(id))) void run("readwrite", store => store.delete(id)).catch(() => {});
}
void dropUnused();
