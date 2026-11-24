import * as engine from "./engine";
import * as remote from "./remote";

/** The scripted demo runs only in the browser preview of a dev build; the app always talks to its server. */
export const demoMode = import.meta.env.DEV && !("__TAURI_INTERNALS__" in window) && !directServer();

/** Dev only: `?server=ws://127.0.0.1:<port>/?token=<token>` connects the browser preview to a running server. */
export function directServer() {
  return import.meta.env.DEV ? new URLSearchParams(location.search).get("server") ?? undefined : undefined;
}

export const stopChat = (id: string) => demoMode ? engine.stop(id) : remote.abort(id);
export const answerAsk = (id: string, answers: string[][]) => demoMode ? engine.answer(id, answers) : remote.answer(id, answers);
export const dismissAsk = (id: string) => demoMode ? engine.dismiss(id) : remote.dismiss(id);
