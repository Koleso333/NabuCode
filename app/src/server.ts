import { createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { protocolVersion, type NabuEvent, type Requests } from "@nabu/protocol";
import { tr } from "./i18n";

type ShellStatus =
  | { state: "starting" }
  | { state: "running"; port: number; token: string }
  | { state: "failed"; message: string };

type ServerMessage =
  | { type: "reply"; id: number; result: unknown }
  | { type: "error"; id: number | null; error: { message: string } }
  | { type: "event"; event: NabuEvent };

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void };

export type Connection =
  | { state: "connecting" }
  | { state: "connected" }
  | { state: "reconnecting" }
  | { state: "failed"; message: string }
  | { state: "mismatch"; server?: number };

const [connection, setConnection] = createSignal<Connection>({ state: "connecting" });
export { connection };

const pending = new Map<number, Pending>();
const listeners = new Set<(event: NabuEvent) => void>();
const connectedListeners = new Set<() => void>();
let socket: WebSocket | undefined;
let address: string | undefined;
let everConnected = false;
let retryTimer: ReturnType<typeof setTimeout> | undefined;
let nextId = 1;

const waiting = () => setConnection({ state: everConnected ? "reconnecting" : "connecting" });

function apply(status: ShellStatus) {
  if (status.state === "running") return open(`ws://127.0.0.1:${status.port}/?token=${status.token}`);
  drop();
  if (status.state === "failed") setConnection({ state: "failed", message: status.message });
  else waiting();
}

function open(url: string) {
  if (url === address && socket) return;
  drop();
  address = url;
  connect();
}

function connect() {
  const current = new WebSocket(address!);
  socket = current;
  current.onopen = () => void greet(current);
  current.onmessage = event => receive(JSON.parse(event.data) as ServerMessage);
  // The socket can drop while the shell still reports the same server; its exit comes as a status event
  current.onclose = () => {
    if (socket !== current) return;
    socket = undefined;
    failPending();
    waiting();
    retryTimer = setTimeout(() => { if (address && !socket) connect(); }, 1000);
  };
}

// Another NabuCode may have started this server, so it has to speak the same protocol
async function greet(current: WebSocket) {
  const info = await request("serverInfo").catch(() => undefined);
  if (socket !== current || !info) return;
  if (info.protocol !== protocolVersion) {
    drop();
    return setConnection({ state: "mismatch", server: info.protocol });
  }
  everConnected = true;
  setConnection({ state: "connected" });
  connectedListeners.forEach(listener => listener());
}

function drop() {
  clearTimeout(retryTimer);
  address = undefined;
  const current = socket;
  socket = undefined;
  current?.close();
  failPending();
}

function failPending() {
  for (const request of pending.values()) request.reject(new Error(tr().server.lost));
  pending.clear();
}

/** The server answered with an error, as opposed to a request lost with the connection. */
export class ServerError extends Error {}

function receive(message: ServerMessage) {
  if (message.type === "event") return listeners.forEach(listener => listener(message.event));
  if (message.id === null) return console.error("[server]", message.type === "error" ? message.error.message : message);
  const request = pending.get(message.id);
  pending.delete(message.id);
  if (message.type === "reply") request?.resolve(message.result);
  else request?.reject(new ServerError(message.error.message));
}

export function request<M extends keyof Requests>(
  method: M,
  ...params: Requests[M]["params"] extends undefined ? [] : [Requests[M]["params"]]
) {
  return new Promise<Requests[M]["result"]>((resolve, reject) => {
    if (socket?.readyState !== WebSocket.OPEN) return reject(new Error(tr().server.offline));
    const id = nextId++;
    pending.set(id, { resolve: resolve as Pending["resolve"], reject });
    socket.send(JSON.stringify({ id, method, params: params[0] }));
  });
}

/** Subscribes to server events; returns the unsubscribe function. */
export function onServerEvent(listener: (event: NabuEvent) => void) {
  listeners.add(listener);
  return () => void listeners.delete(listener);
}

/** Runs on every (re)connection, when the state has to be loaded from the server again. */
export function onConnected(listener: () => void) {
  connectedListeners.add(listener);
  return () => void connectedListeners.delete(listener);
}

/** The socket of one terminal; `cursor` is where its output resumes, -1 for only new output. */
export function terminalUrl(id: string, cursor: number) {
  const url = new URL(address!);
  url.pathname = "/terminal";
  url.searchParams.set("id", id);
  url.searchParams.set("cursor", String(cursor));
  return url.toString();
}

/** A `/file?…` link of the server, made loadable. */
export function fileUrl(link: string) {
  if (!link.startsWith("/file?") || !address) return link;
  const url = new URL(link, address.replace(/^ws/, "http"));
  url.searchParams.set("token", new URL(address).searchParams.get("token") ?? "");
  return url.toString();
}

/** Streams the bytes of a file without a path to the server; returns the reference its part goes with. */
export async function uploadFile(session: string, part: string, file: File) {
  if (!address) throw new Error("not connected");
  const url = new URL("/upload", address.replace(/^ws/, "http"));
  const params = { token: new URL(address).searchParams.get("token") ?? "", session, part, mime: file.type || "application/octet-stream", filename: file.name };
  Object.entries(params).forEach(([key, value]) => url.searchParams.set(key, value));
  // Sent without a content type, so the browser doesn't ask the server first
  const response = await fetch(url, { method: "POST", body: file.slice(0, file.size, "") });
  if (!response.ok) throw new Error(`upload of ${file.name}: ${response.status}`);
  return (await response.json() as { url: string }).url;
}

/** Also looks for the server again, after another NabuCode version may have closed. */
export function restartServer() {
  waiting();
  void invoke("server_restart");
}

let direct = false;

/** Without the shell there is nothing to restart: the address is used as it is. */
export function connectDirectly(url: string) {
  direct = true;
  open(url);
}

/** The server runs on this machine as the shell's sidecar, so its paths can be opened here. */
export const localServer = () => !direct && "__TAURI_INTERNALS__" in window;

export async function startConnection() {
  await listen<ShellStatus>("server-status", event => apply(event.payload));
  apply(await invoke<ShellStatus>("server_status"));
}
