import { batch, createSignal, type JSX } from "solid-js";

type Drag = { from: number; to: number };

/**
 * Reordering a list by pressing an item and dragging it. The item becomes a slot that moves to where it would land,
 * the others part around it, and a copy of the pressed element follows the pointer. Items are keyed, so refs survive reordering.
 */
export function createReorder(options: { keys: () => string[]; onMove: (from: number, to: number) => void }) {
  const elements = new Map<string, HTMLElement>();
  const [drag, setDrag] = createSignal<Drag | null>(null);
  let rects: DOMRect[] = [];
  let gap = 0;

  const indexOf = (key: string) => options.keys().indexOf(key);

  // Where the slot lands: past every other item whose middle is above the pointer
  function target(from: number, y: number) {
    return rects.filter((rect, index) => index !== from && rect.top + rect.height / 2 < y).length;
  }

  function begin(from: number, grab: HTMLElement, startX: number, startY: number) {
    const grabRect = grab.getBoundingClientRect();
    const ghost = grab.cloneNode(true) as HTMLElement;
    ghost.classList.add("drag-ghost");
    Object.assign(ghost.style, { left: `${grabRect.left}px`, top: `${grabRect.top}px`, width: `${grabRect.width}px` });
    document.body.append(ghost);
    document.documentElement.classList.add("reordering");
    // The slot may be shorter than the item (its own contents fold), so it is measured once it is a slot
    rects = [];
    setDrag({ from, to: from });
    rects = options.keys().map(key => elements.get(key)!.getBoundingClientRect());
    gap = rects.length > 1 ? (from < rects.length - 1 ? rects[from + 1].top - rects[from].bottom : rects[from].top - rects[from - 1].bottom) : 0;
    setDrag({ from, to: from });
    return (x: number, y: number) => {
      ghost.style.transform = `translate(${x - startX}px, ${y - startY}px)`;
      const to = target(from, y);
      if (to !== drag()?.to) setDrag({ from, to });
    };
  }

  /** Starts on a press of `grab`, the part of the item that is dragged; a press that doesn't move stays a click. */
  function press(key: string, event: PointerEvent, grab: HTMLElement) {
    if (event.button !== 0 || drag()) return;
    const startX = event.clientX;
    const startY = event.clientY;
    let follow: ((x: number, y: number) => void) | undefined;
    const move = (next: PointerEvent) => {
      if (!follow && Math.hypot(next.clientX - startX, next.clientY - startY) < 5) return;
      follow ??= begin(indexOf(key), grab, startX, startY);
      follow(next.clientX, next.clientY);
    };
    const end = (finish: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", end);
      window.removeEventListener("pointercancel", end);
      if (!follow) return;
      document.querySelector(".drag-ghost")?.remove();
      document.documentElement.classList.remove("reordering");
      // The click that ends a drag is not a click on the item
      const swallow = (click: MouseEvent) => { click.stopPropagation(); click.preventDefault(); };
      window.addEventListener("click", swallow, { capture: true, once: true });
      setTimeout(() => window.removeEventListener("click", swallow, { capture: true }));
      const done = drag();
      batch(() => {
        setDrag(null);
        if (done && done.to !== done.from && finish.type === "pointerup") options.onMove(done.from, done.to);
      });
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", end);
    window.addEventListener("pointercancel", end);
  }

  // The items between the slot's start and its target shift by the slot's size; the slot jumps over them
  function offset(index: number) {
    const current = drag();
    if (!current || !rects.length) return 0;
    const { from, to } = current;
    const size = rects[from].height + gap;
    if (index === from) return to > from ? rects[to].bottom - rects[from].bottom : to < from ? rects[to].top - rects[from].top : 0;
    if (from < index && index <= to) return -size;
    if (to <= index && index < from) return size;
    return 0;
  }

  return {
    ref: (key: string) => (element: HTMLElement) => { elements.set(key, element); },
    press,
    active: () => !!drag(),
    isDragged: (key: string) => drag()?.from === indexOf(key),
    style: (key: string): JSX.CSSProperties => drag()
      ? { transform: `translateY(${offset(indexOf(key))}px)`, transition: "transform 160ms cubic-bezier(.2, 0, 0, 1)" }
      : {},
  };
}
