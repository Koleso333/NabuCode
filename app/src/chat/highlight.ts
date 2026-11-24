import { createSignal, type Signal } from "solid-js";
import type { HighlighterCore } from "shiki/core";

// Shiki loads on the first code block, with only the languages that appear in the chat
const languages: Record<string, () => Promise<unknown>> = {
  typescript: () => import("shiki/langs/typescript.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
  powershell: () => import("shiki/langs/powershell.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
};

const aliases: Record<string, string> = {
  ts: "typescript", js: "javascript", jsx: "tsx", rs: "rust", sh: "shellscript", bash: "shellscript", shell: "shellscript",
  zsh: "shellscript", ps1: "powershell", ps: "powershell", pwsh: "powershell", py: "python", md: "markdown", yml: "yaml",
};

export const languageOf = (lang: string | undefined) => {
  const name = lang?.trim().split(/\s/)[0].toLowerCase() ?? "";
  const known = aliases[name] ?? name;
  return languages[known] ? known : undefined;
};

export const escapeHtml = (text: string) => text.replace(/[&<>"]/g, char => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[char]!);

let highlighter: Promise<HighlighterCore> | undefined;
let ready: HighlighterCore | undefined;
const states = new Map<string, Signal<boolean>>();

function state(lang: string) {
  let entry = states.get(lang);
  if (!entry) {
    entry = createSignal(false);
    states.set(lang, entry);
    whenIdle(() => void load(lang, entry!));
  }
  return entry;
}

const whenIdle = (task: () => void) => "requestIdleCallback" in window ? requestIdleCallback(task, { timeout: 500 }) : setTimeout(task, 50);

async function load(lang: string, [, setReady]: Signal<boolean>) {
  try {
    highlighter ??= createCore();
    const core = await highlighter;
    await core.loadLanguage((await languages[lang]()) as never);
    ready = core;
    setReady(true);
  } catch (error) {
    console.error(`Could not load highlighting for ${lang}`, error);
  }
}

async function createCore() {
  const [{ createHighlighterCore, createCssVariablesTheme }, { createJavaScriptRegexEngine }] = await Promise.all([import("shiki/core"), import("shiki/engine/javascript")]);
  return createHighlighterCore({
    themes: [createCssVariablesTheme({ name: "nabu", variablePrefix: "--shiki-" })],
    langs: [],
    engine: createJavaScriptRegexEngine(),
  });
}

// Reactive: reading it subscribes to the language becoming ready
export function isLoaded(lang: string) {
  return state(lang)[0]();
}

export function highlight(code: string, lang: string) {
  if (!ready || !states.get(lang)?.[0]()) return undefined;
  return ready.codeToHtml(code, { lang, theme: "nabu" });
}

// One HTML string per line, for views that lay out lines themselves
export function highlightLines(code: string, lang: string) {
  if (!ready || !states.get(lang)?.[0]()) return undefined;
  const lines = ready.codeToTokens(code, { lang, theme: "nabu" }).tokens;
  return lines.map(tokens => tokens.map(token => `<span style="color:${token.color}">${escapeHtml(token.content)}</span>`).join(""));
}
