import { createEffect, createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { preferences, type Preferences } from "./preferences";

export type FontKind = keyof Preferences["fonts"];
type FontInfo = { family: string; mono: boolean; cyrillic: boolean; weights: number[] };
type FontMeta = { family: string; category: string; popularity: number; isOpenSource: boolean; isBrandFont: boolean; subsets: string[]; fonts: Record<string, unknown> };

const listLimit = 50;
const inTauri = "__TAURI_INTERNALS__" in window;
const fallbacks: Record<FontKind, string> = {
  ui: "Geist, sans-serif",
  code: "'Geist Mono', Consolas, monospace",
  terminal: "'Geist Mono', Consolas, monospace",
};

const [catalog, setCatalog] = createSignal<FontInfo[] | null>(null);
const [catalogFailed, setCatalogFailed] = createSignal(false);
export { catalog, catalogFailed };

function parseCatalog(text: string): FontInfo[] {
  // The response starts with an anti-JSON-hijacking prefix
  const list: FontMeta[] = JSON.parse(text.slice(text.indexOf("{"))).familyMetadataList;
  return list
    .filter(font => font.isOpenSource && !font.isBrandFont)
    .sort((a, b) => a.popularity - b.popularity)
    .map(font => ({ family: font.family, mono: font.category === "Monospace", cyrillic: font.subsets.includes("cyrillic"), weights: Object.keys(font.fonts).filter(key => /^\d+$/.test(key)).map(Number) }));
}

let catalogLoad: Promise<void> | undefined;

export function loadCatalog() {
  catalogLoad ??= (async () => {
    try {
      const text = inTauri ? await invoke<string>("font_catalog") : await fetch("/google-fonts/metadata/fonts").then(response => response.text());
      setCatalog(parseCatalog(text));
      setCatalogFailed(false);
      for (const kind of ["ui", "code"] as const) fontChoices(kind, "").forEach(font => previewFont(font.family));
    } catch (error) {
      console.error("Could not load the font catalog", error);
      setCatalogFailed(true);
      catalogLoad = undefined;
    }
  })();
  return catalogLoad;
}

export function fontChoices(kind: FontKind, query: string) {
  const text = query.trim().toLowerCase();
  return (catalog() ?? []).filter(font => (kind === "ui" || font.mono) && font.family.toLowerCase().includes(text)).slice(0, listLimit);
}

function addStyle(css: string) {
  const style = document.createElement("style");
  style.textContent = css;
  document.head.append(style);
}

const fontUrl = (family: string) => `https://fonts.googleapis.com/css2?family=${encodeURIComponent(family)}`;

// Preview faces hold only the glyphs of the name, so they get their own family name
const previewName = (family: string) => `Preview ${family}`;
const previewed = new Set<string>();

export function previewFont(family: string) {
  if (previewed.has(family)) return;
  previewed.add(family);
  fetch(`${fontUrl(family)}&text=${encodeURIComponent(family)}`)
    .then(response => response.ok ? response.text() : Promise.reject(response.status))
    .then(css => addStyle(css.replaceAll(`font-family: '${family}'`, `font-family: '${previewName(family)}'`)))
    .catch(() => previewed.delete(family));
}

const fontStack = (family: string, kind: FontKind) => family ? `"${family}", ${fallbacks[kind]}` : fallbacks[kind];

export const previewStack = (family: string, kind: FontKind) => family ? `"${previewName(family)}", ${fontStack(family, kind)}` : fallbacks[kind];

const installs = new Map<string, Promise<void>>();
const [downloading, setDownloading] = createSignal<string[]>([]);
export const isDownloading = (family: string) => downloading().includes(family);

export function installFont(family: string) {
  if (!family) return Promise.resolve();
  let install = installs.get(family);
  if (!install) {
    setDownloading(list => [...list, family]);
    install = downloadFont(family)
      .catch(error => {
        console.error(`Could not install font ${family}`, error);
        installs.delete(family);
      })
      .finally(() => setDownloading(list => list.filter(item => item !== family)));
    installs.set(family, install);
  }
  return install;
}

async function downloadFont(family: string) {
  if (!inTauri) return addStyle(`@import url("${fontUrl(family)}");`);
  // Weights matter only on first download; the installed CSS is cached on disk
  const weights = catalog()?.find(font => font.family === family)?.weights ?? [];
  addStyle(await invoke<string>("font_install", { family, weights }));
  await document.fonts.load(`16px "${family}"`);
}

// The variable changes only once the font is ready: a font face added under an unchanged variable is not repainted
export function applyFonts() {
  createEffect(() => {
    for (const kind of Object.keys(fallbacks) as FontKind[]) {
      const family = preferences.fonts[kind];
      void installFont(family).then(() => {
        if (preferences.fonts[kind] === family) document.documentElement.style.setProperty(`--font-${kind}`, fontStack(family, kind));
      });
    }
  });
}
