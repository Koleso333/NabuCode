import { createEffect, onCleanup, Show } from "solid-js";
import { tr } from "../i18n";
import { escapeHtml, highlightLines, isLoaded, languageOf } from "./highlight";

// A unified patch becomes one HTML string: thousands of diff lines cost one DOM write, and the result is cached
const cache = new Map<string, string>();
const cacheLimit = 200;
/** Rows drawn per step of a big diff; the steps go one per task, so the window keeps responding. */
const CHUNK = 300;
/** Text above this many characters per step is shown without colors: highlighting it would take seconds. */
const HIGHLIGHT_LIMIT = 100_000;
/** Characters shown of one line: laying out a megabyte-long line (minified JSON) freezes the window for seconds. */
const LINE_LIMIT = 2000;

type Row = { kind: "add" | "del" | "ctx" | "skip"; number: number | ""; text: string; html?: string; cut?: number };

function parsePatch(patch: string) {
  const rows: Row[] = [];
  let old = 0;
  let next = 0;
  let hunks = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("---") || line.startsWith("+++") || line.startsWith("\\")) continue;
    const hunk = line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) {
      if (hunks++ > 0) rows.push({ kind: "skip", number: "", text: "⋯" });
      old = Number(hunk[1]);
      next = Number(hunk[2]);
      continue;
    }
    if (line[0] === "+") rows.push({ kind: "add", number: next++, text: line.slice(1) });
    else if (line[0] === "-") rows.push({ kind: "del", number: old++, text: line.slice(1) });
    else if (hunks > 0) { rows.push({ kind: "ctx", number: next++, text: line.slice(1) }); old++; }
  }
  return rows;
}

// Old and new sides are highlighted apart, so each one reads as valid code
function paint(rows: Row[], lang: string) {
  for (const side of ["del", "add"] as const) {
    const lines = rows.filter(row => row.kind === side || row.kind === "ctx");
    const html = highlightLines(lines.map(row => row.text).join("\n"), lang);
    if (!html) return;
    lines.forEach((row, index) => { if (row.kind === side || side === "add") row.html = html[index]; });
  }
}

function rowsHtml(rows: Row[], lang: string | undefined) {
  for (const row of rows) if (row.text.length > LINE_LIMIT) {
    row.cut = row.text.length - LINE_LIMIT;
    row.text = row.text.slice(0, LINE_LIMIT);
  }
  if (lang) paint(rows, lang);
  const cut = (row: Row) => row.cut ? `<span class="diff-cut">${escapeHtml(tr().files.longLine(row.cut))}</span>` : "";
  return rows.map(row => `<div class="diff-row ${row.kind}"><span class="diff-num">${row.number}</span><span class="diff-code">${row.html ?? escapeHtml(row.text)}${cut(row)}</span></div>`).join("");
}

// Lines as the read tool returns them: "number<tab>text"; its notes for the model are dropped
function parseNumbered(text: string) {
  return text.split("\n").flatMap((line): Row[] => {
    const numbered = line.match(/^(\d+)\t(.*)$/);
    return numbered ? [{ kind: "ctx", number: Number(numbered[1]), text: numbered[2] }] : [];
  });
}

function cached(kind: string, text: string, lang: string | undefined, build: () => Row[]) {
  const key = `${kind}:${lang ?? ""}\n${text}`;
  let html = cache.get(key);
  if (html !== undefined) return html;
  html = rowsHtml(build(), lang);
  cache.set(key, html);
  if (cache.size > cacheLimit) cache.delete(cache.keys().next().value!);
  return html;
}

function fileLanguage(path: string) {
  const known = languageOf(path.split(".").at(-1));
  return known && isLoaded(known) ? known : undefined;
}

/** A patch's lines; with `limit` only the first ones and a note how many there are. */
export function Diff(props: { patch: string; path: string; limit?: number }) {
  let box!: HTMLDivElement;
  createEffect(() => {
    const lang = fileLanguage(props.path);
    const rows = parsePatch(props.patch);
    const shown = props.limit && rows.length > props.limit ? rows.slice(0, props.limit) : rows;
    if (shown.length < rows.length) shown.push({ kind: "skip", number: "", text: tr().files.shownLines(shown.length, rows.length) });
    if (shown.length <= CHUNK) return void (box.innerHTML = cached(`diff${props.limit ?? ""}`, props.patch, lang, () => shown));
    box.textContent = "";
    let next = 0;
    let timer: ReturnType<typeof setTimeout>;
    const step = () => {
      const part = shown.slice(next, next += CHUNK);
      const size = part.reduce((sum, row) => sum + row.text.length, 0);
      box.insertAdjacentHTML("beforeend", rowsHtml(part, size > HIGHLIGHT_LIMIT ? undefined : lang));
      if (next < shown.length) timer = setTimeout(step);
    };
    step();
    onCleanup(() => clearTimeout(timer));
  });
  return <div ref={box} class="diff" />;
}

export function CodeLines(props: { text: string; path: string }) {
  return <div class="diff" innerHTML={cached("code", props.text, fileLanguage(props.path), () => parseNumbered(props.text))} />;
}

export function DiffCounts(props: { additions: number; deletions: number }) {
  return <span class="diff-counts">
    <Show when={props.additions}><span class="diff-add">+{props.additions}</span></Show>
    <Show when={props.deletions}><span class="diff-del">−{props.deletions}</span></Show>
  </span>;
}
