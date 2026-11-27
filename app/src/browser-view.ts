import { browserUrl } from "./server";

/** Close code of a tab that is gone: reconnecting is useless. */
const GONE = 4404;

/** One browser tab on screen: its frames drawn into a canvas, the user's mouse and keys sent back. Kept while the tab lives. */
class BrowserView {
  readonly element = document.createElement("div");
  private readonly canvas = document.createElement("canvas");
  private socket?: WebSocket;
  private opened = false;
  private closed = false;
  private tries = 0;
  private retry?: ReturnType<typeof setTimeout>;
  private sizeTimer?: ReturnType<typeof setTimeout>;
  private frame?: ImageBitmap;
  // Frames decode in parallel; an older one finishing late is dropped
  private received = 0;
  private drawn = 0;
  // The page area in CSS pixels, and where it is drawn in the element
  private area = { width: 0, height: 0 };
  private box = { x: 0, y: 0, width: 0, height: 0 };
  private move?: MouseEvent;
  private readonly resizes = new ResizeObserver(() => {
    this.draw();
    clearTimeout(this.sizeTimer);
    this.sizeTimer = setTimeout(() => this.sendSize(), 120);
  });

  constructor(private readonly id: string) {
    this.element.className = "browser-screen";
    this.canvas.tabIndex = 0;
    this.element.append(this.canvas);
    this.canvas.addEventListener("mousedown", event => {
      this.canvas.focus();
      this.mouse("mousePressed", event);
    });
    this.canvas.addEventListener("mouseup", event => this.mouse("mouseReleased", event));
    this.canvas.addEventListener("mousemove", event => {
      if (!this.move) requestAnimationFrame(() => { this.mouse("mouseMoved", this.move!); this.move = undefined; });
      this.move = event;
    });
    this.canvas.addEventListener("pointerdown", event => this.canvas.setPointerCapture(event.pointerId));
    this.canvas.addEventListener("contextmenu", event => event.preventDefault());
    this.canvas.addEventListener("wheel", event => {
      event.preventDefault();
      const scale = event.deltaMode === 1 ? 40 : event.deltaMode === 2 ? this.area.height : 1;
      this.send({ do: "mouse", type: "mouseWheel", ...this.point(event), deltaX: event.deltaX * scale, deltaY: event.deltaY * scale, modifiers: modifiers(event) });
    }, { passive: false });
    this.canvas.addEventListener("keydown", event => this.key(event, true));
    this.canvas.addEventListener("keyup", event => this.key(event, false));
  }

  /** Puts the view into the panel; the first time it also connects. The shown tab goes to front, so it draws. */
  show(host: HTMLElement) {
    host.replaceChildren(this.element);
    if (!this.opened) {
      this.opened = true;
      this.resizes.observe(this.element);
      this.connect();
    } else this.send({ do: "front" });
    this.draw();
  }

  dispose() {
    this.closed = true;
    clearTimeout(this.retry);
    clearTimeout(this.sizeTimer);
    this.resizes.disconnect();
    this.socket?.close(1000);
    this.frame?.close();
    this.element.remove();
  }

  /** Text is the page area of the frames that follow, `{"width","height"}`; binary is a JPEG frame. */
  private connect() {
    const socket = new WebSocket(browserUrl(this.id));
    socket.binaryType = "blob";
    this.socket = socket;
    socket.onopen = () => {
      this.tries = 0;
      this.sendSize();
    };
    socket.onmessage = event => {
      if (typeof event.data === "string") {
        this.area = JSON.parse(event.data);
        return;
      }
      const number = ++this.received;
      void createImageBitmap(event.data).then(bitmap => {
        if (number < this.drawn || this.closed) return bitmap.close();
        this.drawn = number;
        this.frame?.close();
        this.frame = bitmap;
        this.draw();
      }, () => {});
    };
    socket.onclose = event => {
      if (this.socket === socket) this.socket = undefined;
      if (this.closed || event.code === 1000 || event.code === GONE) return;
      this.retry = setTimeout(() => this.connect(), Math.min(250 * 2 ** Math.min(this.tries++, 4), 4000));
    };
  }

  private send(message: object) {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message));
  }

  // The page takes the panel's size in CSS pixels, frames come in device pixels
  private sendSize() {
    const width = this.element.clientWidth, height = this.element.clientHeight;
    if (width > 0 && height > 0) this.send({ do: "size", width, height, scale: devicePixelRatio || 1 });
  }

  // A page size the model fixed is scaled down to fit the panel, never up
  private draw() {
    const ratio = devicePixelRatio || 1;
    const width = this.element.clientWidth, height = this.element.clientHeight;
    if (!width || !height) return;
    this.canvas.width = Math.round(width * ratio);
    this.canvas.height = Math.round(height * ratio);
    const frame = this.frame;
    const context = this.canvas.getContext("2d")!;
    context.clearRect(0, 0, this.canvas.width, this.canvas.height);
    if (!frame) return;
    const area = this.area.width > 0 ? this.area : { width: frame.width / ratio, height: frame.height / ratio };
    const scale = Math.min(1, width / area.width, height / area.height);
    this.box = { x: Math.round((width - area.width * scale) / 2), y: 0, width: area.width * scale, height: area.height * scale };
    context.imageSmoothingQuality = "high";
    context.drawImage(frame, this.box.x * ratio, this.box.y * ratio, this.box.width * ratio, this.box.height * ratio);
  }

  // A point of the panel in the page's CSS pixels
  private point(event: MouseEvent) {
    const bounds = this.canvas.getBoundingClientRect();
    const scale = this.box.width ? this.area.width / this.box.width : 1;
    return { x: (event.clientX - bounds.left - this.box.x) * scale, y: (event.clientY - bounds.top - this.box.y) * scale };
  }

  private mouse(type: string, event: MouseEvent) {
    const button = type === "mouseMoved" ? buttonOf(event.buttons) : (["left", "middle", "right"][event.button] ?? "none");
    this.send({ do: "mouse", type, ...this.point(event), button, buttons: event.buttons, clickCount: type === "mouseMoved" ? 0 : Math.max(1, event.detail), modifiers: modifiers(event) });
  }

  // Ctrl+V pastes the user's clipboard; app shortcuts pass through to the window
  private key(event: KeyboardEvent, down: boolean) {
    const ctrl = event.ctrlKey && !event.altKey && !event.metaKey;
    if (event.ctrlKey && event.code === "Backquote") return;
    if (event.ctrlKey && event.altKey && event.code === "KeyT") return;
    event.preventDefault();
    if (ctrl && event.code === "KeyV") {
      if (down) void navigator.clipboard.readText().then(text => { if (text) this.send({ do: "text", text }); }, () => {});
      return;
    }
    const text = down ? textOf(event) : undefined;
    const command = down && ctrl ? editCommand(event) : undefined;
    this.send({
      do: "key", type: !down ? "keyUp" : text ? "keyDown" : "rawKeyDown", modifiers: modifiers(event),
      key: event.key, code: event.code, windowsVirtualKeyCode: event.keyCode, nativeVirtualKeyCode: event.keyCode,
      location: event.location, autoRepeat: event.repeat, text, unmodifiedText: text, commands: command ? [command] : undefined,
    });
  }
}

const modifiers = (event: MouseEvent | KeyboardEvent) => (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);

const buttonOf = (buttons: number) => buttons & 1 ? "left" : buttons & 2 ? "right" : buttons & 4 ? "middle" : "none";

function textOf(event: KeyboardEvent) {
  if (event.key === "Enter") return "\r";
  if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !(event.altKey && !event.getModifierState("AltGraph"))) return event.key;
}

// A headless browser does not run these shortcuts by itself
function editCommand(event: KeyboardEvent) {
  switch (event.code) {
    case "KeyA": return "selectAll";
    case "KeyC": return "copy";
    case "KeyX": return "cut";
    case "KeyZ": return event.shiftKey ? "redo" : "undo";
    case "KeyY": return "redo";
  }
}

const views = new Map<string, BrowserView>();

export function browserView(id: string) {
  let view = views.get(id);
  if (!view) views.set(id, view = new BrowserView(id));
  return view;
}

export function dropBrowserView(id: string) {
  views.get(id)?.dispose();
  views.delete(id);
}
