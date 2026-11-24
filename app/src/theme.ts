import { createSignal } from "solid-js";
import themes from "./themes.json";

export type ThemeMode = "system" | "dark" | "light";
export type ThemeChoice = { theme: string; mode: ThemeMode };
type ThemeCatalog = Record<string, { name: string; dark: Record<string, string>; light: Record<string, string> }>;

export const themeIds = Object.keys(themes);
export const themeName = (id: string) => (themes as ThemeCatalog)[id].name;

const paletteKeys: Record<string, string> = {
  "background-base": "chrome", "background-stronger": "workspace", "input-hover": "input",
  "button-secondary-hover": "search", "border-weak-base": "outline", "border-base": "input-outline",
  "text-base": "text", "text-weak": "muted", "text-strong": "strong", "icon-base": "icon",
  "button-ghost-hover": "hover", "button-ghost-hover2": "selected", "icon-interactive-base": "focus",
  "surface-raised-base-hover": "context-menu-hover", "surface-base": "card",
  "background-weak": "track", "icon-success-base": "success", "icon-warning-base": "warning", "icon-critical-base": "critical",
  "icon-invert-base": "on-primary", "button-primary-base": "primary",
  "icon-strong-hover": "primary-hover", "icon-strong-focus": "primary-focus",
  "icon-strong-active": "primary-active",
};

// Chat colors keep their palette names
const chatKeys = [
  "syntax-comment", "syntax-string", "syntax-keyword", "syntax-primitive", "syntax-operator", "syntax-variable",
  "syntax-property", "syntax-type", "syntax-constant", "syntax-punctuation",
  "syntax-diff-add", "syntax-diff-delete", "text-diff-add-base", "text-diff-delete-base",
  "text-interactive-base", "text-on-critical-base", "surface-critical-base", "border-critical-base",
  "icon-agent-plan-base", "icon-agent-docs-base", "icon-agent-ask-base", "icon-agent-build-base", "markdown-link",
];

function readSaved(): ThemeChoice | null {
  try {
    const saved = JSON.parse(localStorage.getItem("everyend-theme") || "null");
    if (!saved) return null;
    return {
      theme: Object.hasOwn(themes, saved.theme) ? saved.theme : "oc-2",
      mode: ["system", "dark", "light"].includes(saved.mode) ? saved.mode : "system",
    };
  } catch {
    return null;
  }
}

const [choice, setChoice] = createSignal<ThemeChoice>(readSaved() ?? { theme: "oc-2", mode: "system" });
export { choice as themeChoice };

// Hover try-on: overrides the saved choice until cancelled
let preview: Partial<ThemeChoice> = {};

export function applyTheme() {
  const theme = preview.theme ?? choice().theme;
  const mode = preview.mode ?? choice().mode;
  const dark = mode === "dark" || (mode === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
  const palette = (themes as ThemeCatalog)[theme][dark ? "dark" : "light"];
  for (const [key, name] of Object.entries(paletteKeys)) {
    document.documentElement.style.setProperty(`--${name}`, palette[key]);
  }
  for (const key of chatKeys) document.documentElement.style.setProperty(`--${key}`, palette[key]);
  document.documentElement.dataset.mode = dark ? "dark" : "light";
  document.documentElement.dataset.theme = theme;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", palette["background-base"]);
  const startup = document.getElementById("startup");
  if (startup) startup.style.background = palette["background-base"];
}

export function saveTheme(next: Partial<ThemeChoice>) {
  setChoice({ ...choice(), ...next });
  localStorage.setItem("everyend-theme", JSON.stringify(choice()));
  preview = {};
  applyTheme();
}

// The Flutter prototype's choice is used only until a choice is made here
export function importTheme(saved: ThemeChoice | null) {
  if (readSaved() || !saved || !Object.hasOwn(themes, saved.theme)) return;
  saveTheme(saved);
}

export function previewTheme(next: Partial<ThemeChoice>) {
  preview = { ...preview, ...next };
  applyTheme();
}

export function previewedTheme() {
  return preview.theme ?? choice().theme;
}

export function cancelPreview() {
  preview = {};
  applyTheme();
}
