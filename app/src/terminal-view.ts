import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import { openUrl } from "@tauri-apps/plugin-opener";
import { request, terminalUrl } from "./server";

/** Close code of a terminal that is gone: reconnecting is useless. */
const GONE = 4404;

/** One terminal on screen: xterm with its socket. Kept while the terminal lives, so switching tabs keeps the screen. */
class TerminalView {
  readonly element = document.createElement("div");
  private readonly term: Terminal;
  private readonly fit = new FitAddon();
  private socket?: WebSocket;
  private opened = false;
  private closed = false;
  // Output position in UTF-16 units, where a reconnect resumes
  private cursor = 0;
  private tries = 0;
  private retry?: ReturnType<typeof setTimeout>;
  private sizeTimer?: ReturnType<typeof setTimeout>;
  private readonly resizes = new ResizeObserver(() => this.refit());

  constructor(private readonly id: string) {
    this.element.className = "terminal-screen";
    this.term = new Terminal({ ...look(), ...metrics, cursorBlink: true, scrollback: 10_000 });
    this.term.loadAddon(this.fit);
    this.term.loadAddon(new WebLinksAddon((event, url) => { if (event.ctrlKey) void openUrl(url).catch(() => window.open(url)); }));
    this.term.attachCustomKeyEventHandler(event => this.key(event));
    this.term.onData(data => { if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(data); });
    this.term.onResize(({ cols, rows }) => {
      clearTimeout(this.sizeTimer);
      this.sizeTimer = setTimeout(() => void request("terminalResize", { id: this.id, cols, rows }).catch(() => {}), 80);
    });
  }

  /** Puts the view into the panel; the first time it also opens the screen and connects. */
  show(host: HTMLElement) {
    host.replaceChildren(this.element);
    if (!this.opened) {
      this.opened = true;
      this.term.open(this.element);
      this.resizes.observe(this.element);
      this.connect();
    }
    this.refit();
    this.term.focus();
  }

  focus() {
    this.term.focus();
  }

  // xterm measures the cell only when the font option changes, so a font that finished loading under the same name needs a nudge
  restyle() {
    const next = look();
    this.term.options.fontFamily = "monospace";
    Object.assign(this.term.options, next);
    this.refit();
  }

  dispose() {
    this.closed = true;
    clearTimeout(this.retry);
    clearTimeout(this.sizeTimer);
    this.resizes.disconnect();
    this.socket?.close(1000);
    this.term.dispose();
    this.element.remove();
  }

  // A hidden screen has no size: fitting it would shrink the shell to one column
  private refit() {
    if (this.opened && this.element.offsetWidth > 0 && this.element.offsetHeight > 0) this.fit.fit();
  }

  /** `\0{"cursor":N}` says where the text that follows starts; text is output; input goes back as text. */
  private connect() {
    const socket = new WebSocket(terminalUrl(this.id, this.cursor));
    socket.binaryType = "arraybuffer";
    this.socket = socket;
    socket.onopen = () => {
      this.tries = 0;
      void request("terminalResize", { id: this.id, cols: this.term.cols, rows: this.term.rows }).catch(() => {});
    };
    socket.onmessage = event => {
      if (event.data instanceof ArrayBuffer) {
        const bytes = new Uint8Array(event.data);
        if (bytes[0] !== 0) return;
        const next = JSON.parse(new TextDecoder().decode(bytes.subarray(1))).cursor;
        if (Number.isSafeInteger(next)) this.cursor = next;
        return;
      }
      this.term.write(event.data);
      this.cursor += event.data.length;
    };
    socket.onclose = event => {
      if (this.socket === socket) this.socket = undefined;
      if (this.closed || event.code === 1000 || event.code === GONE) return;
      this.retry = setTimeout(() => this.connect(), Math.min(250 * 2 ** Math.min(this.tries++, 4), 4000));
    };
  }

  // Ctrl+C copies when something is selected, Ctrl+V pastes; app shortcuts pass through to the window
  private key(event: KeyboardEvent) {
    if (event.type !== "keydown") return true;
    const ctrl = event.ctrlKey && !event.altKey && !event.metaKey;
    if (ctrl && event.code === "KeyC" && (event.shiftKey || this.term.hasSelection())) {
      void navigator.clipboard.writeText(this.term.getSelection());
      if (!event.shiftKey) this.term.clearSelection();
      event.preventDefault();
      return false;
    }
    if (ctrl && event.code === "KeyV") return false;
    if (event.ctrlKey && event.code === "Backquote") return false;
    if (event.ctrlKey && event.altKey && event.code === "KeyT") return false;
    return true;
  }
}

const metrics = { fontSize: 13, lineHeight: 1.2 };

/**
 * Columns and rows that fit `host`, so the shell starts at its real size: ConPTY redraws on a resize right after start
 * and leaves a blank line above the prompt. The font is loaded first, or the cell would be measured by the fallback.
 */
export async function fitSize(host: HTMLElement) {
  const style = look();
  await document.fonts.load(`${metrics.fontSize}px ${style.fontFamily}`).catch(() => []);
  const probe = document.createElement("div");
  probe.className = "terminal-screen";
  probe.style.visibility = "hidden";
  host.append(probe);
  const term = new Terminal({ ...style, ...metrics });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(probe);
  const size = fit.proposeDimensions();
  term.dispose();
  probe.remove();
  return size && size.cols > 0 && size.rows > 0 ? { cols: size.cols, rows: size.rows } : { cols: 80, rows: 24 };
}

const views = new Map<string, TerminalView>();

export function terminalView(id: string) {
  let view = views.get(id);
  if (!view) views.set(id, view = new TerminalView(id));
  return view;
}

export function dropView(id: string) {
  views.get(id)?.dispose();
  views.delete(id);
}

// Colors and font come from the theme variables, so a theme or font change restyles every screen
function look() {
  const style = getComputedStyle(document.documentElement);
  const color = (name: string) => style.getPropertyValue(name).trim();
  return {
    fontFamily: color("--font-terminal") || "'Geist Mono', Consolas, monospace",
    theme: {
      background: color("--workspace"), foreground: color("--text"), cursor: color("--strong"), cursorAccent: color("--workspace"),
      selectionBackground: translucent(color("--text"), 0.35),
      scrollbarSliderBackground: color("--outline"), scrollbarSliderHoverBackground: color("--muted"), scrollbarSliderActiveBackground: color("--muted"),
    },
  };
}

// The selection of text fields in styles.css: the text color at 35%. xterm can't read color-mix, so the color is spelled out
function translucent(css: string, alpha: number) {
  const context = document.createElement("canvas").getContext("2d")!;
  context.fillStyle = css;
  const value = context.fillStyle;
  if (value.startsWith("#")) return `rgba(${[1, 3, 5].map(at => parseInt(value.slice(at, at + 2), 16)).join(", ")}, ${alpha})`;
  const [r, g, b, a = 1] = value.slice(value.indexOf("(") + 1, -1).split(",").map(Number);
  return `rgba(${r}, ${g}, ${b}, ${a * alpha})`;
}

let restyling = 0;
const restyleAll = () => {
  cancelAnimationFrame(restyling);
  restyling = requestAnimationFrame(() => { for (const view of views.values()) view.restyle(); });
};
new MutationObserver(restyleAll).observe(document.documentElement, { attributes: true, attributeFilter: ["style", "class"] });
document.fonts.addEventListener("loadingdone", restyleAll);
