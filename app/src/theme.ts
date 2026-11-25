import { createSignal } from "solid-js";
import themes from "./themes.json";

export type ThemeMode = "system" | "dark" | "light";
export type ThemeChoice = { theme: string; mode: ThemeMode };
type ThemeCatalog = Record<string, { name: string; dark: Record<string, string>; light: Record<string, string> }>;

export const themeIds = Object.keys(themes);
export const themeName = (id: string) => (themes as ThemeCatalog)[id].name;

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
  for (const [name, color] of Object.entries(palette)) document.documentElement.style.setProperty(`--${name}`, color);
  document.documentElement.dataset.mode = dark ? "dark" : "light";
  document.documentElement.dataset.theme = theme;
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", palette.chrome);
  const startup = document.getElementById("startup");
  if (startup) startup.style.background = palette.chrome;
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
