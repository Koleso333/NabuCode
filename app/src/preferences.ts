import { createStore, type SetStoreFunction } from "solid-js/store";

export type EffortPickerKind = "slider" | "buttons";
export type MarkStyle = "classic" | "difference";

const defaults = {
  // A shell id of the server; empty or unknown means the server's default
  shell: "",
  terminalHeight: 280,
  filesWidth: 300,
  // Empty path: the program Windows has for the file
  editor: { path: "", name: "", icon: "" },
  syntax: true,
  checkOnStartup: true,
  releaseNotes: true,
  fonts: { ui: "", code: "", terminal: "" },
  effortPicker: "slider" as EffortPickerKind,
  markStyle: "classic" as MarkStyle,
  chat: { responseStages: true, reasoning: true, expandCommands: false, diffSummary: false, expandEdits: false, progressBar: true },
  notifications: { agent: true, permissions: true, errors: false },
  sounds: { agent: "Staplebops 01", permissions: "Staplebops 02", errors: "Nope 03" },
  compaction: { auto: true, threshold: 75, custom: false, model: "", effort: "" },
};

export type Preferences = typeof defaults;

const key = "everyend-settings";

// Saved values are laid over defaults so new settings appear without a migration
function merge<T>(base: T, saved: unknown): T {
  if (typeof base !== "object" || base === null) return typeof saved === typeof base ? saved as T : base;
  const result = { ...base } as Record<string, unknown>;
  const source = (typeof saved === "object" && saved !== null ? saved : {}) as Record<string, unknown>;
  for (const name of Object.keys(result)) result[name] = merge(result[name], source[name]);
  return result as T;
}

function readSaved() {
  try {
    return merge(defaults, JSON.parse(localStorage.getItem(key) || "null"));
  } catch {
    return defaults;
  }
}

const [preferences, setStore] = createStore<Preferences>(readSaved());
export { preferences };

export const setPreference: SetStoreFunction<Preferences> = ((...args: unknown[]) => {
  (setStore as (...args: unknown[]) => void)(...args);
  localStorage.setItem(key, JSON.stringify(preferences));
}) as SetStoreFunction<Preferences>;
