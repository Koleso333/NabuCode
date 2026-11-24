import { createMemo, createSignal, For, onCleanup, onMount, type JSX } from "solid-js";

/** Extra height drawn above and below the visible part, so fast scrolling doesn't show gaps. */
const OVERSCAN = 400;

// Last offset at or above `y`
function rowAt(offsets: Float64Array, y: number) {
  let low = 0, high = offsets.length - 1;
  while (low < high) {
    const middle = (low + high + 1) >> 1;
    if (offsets[middle] <= y) low = middle;
    else high = middle - 1;
  }
  return low;
}

/** Draws only the rows near the scroller's visible part; rows taller than `rowHeight` (an open card) are measured once drawn. */
export function VirtualList<T>(props: {
  items: T[]; key: (item: T) => string; rowHeight: number; scroller: () => HTMLElement; children: (item: T) => JSX.Element;
}) {
  const [view, setView] = createSignal({ top: 0, height: 0 });
  const [heights, setHeights] = createSignal(new Map<string, number>());
  const keys = new WeakMap<Element, string>();
  const sizes = new ResizeObserver(entries => setHeights(known => {
    let next = known;
    for (const entry of entries) {
      const key = keys.get(entry.target);
      const height = (entry.target as HTMLElement).offsetHeight;
      if (key === undefined || !entry.target.isConnected || known.get(key) === height) continue;
      if (next === known) next = new Map(known);
      next.set(key, height);
    }
    return next;
  }));
  onMount(() => {
    const scroller = props.scroller();
    const update = () => setView({ top: scroller.scrollTop, height: scroller.clientHeight });
    const resizes = new ResizeObserver(update);
    resizes.observe(scroller);
    scroller.addEventListener("scroll", update, { passive: true });
    update();
    onCleanup(() => { resizes.disconnect(); scroller.removeEventListener("scroll", update); });
  });
  onCleanup(() => sizes.disconnect());

  const offsets = createMemo(() => {
    const known = heights();
    const offsets = new Float64Array(props.items.length + 1);
    props.items.forEach((item, index) => { offsets[index + 1] = offsets[index] + (known.get(props.key(item)) ?? props.rowHeight); });
    return offsets;
  });
  const range = createMemo(() => {
    const all = offsets();
    const last = all.length - 1;
    return { start: Math.min(last, rowAt(all, view().top - OVERSCAN)), end: Math.min(last, rowAt(all, view().top + view().height + OVERSCAN) + 1) };
  });
  const measure = (element: HTMLElement, key: string) => {
    keys.set(element, key);
    sizes.observe(element);
    onCleanup(() => sizes.unobserve(element));
  };

  return <div style={{ "padding-top": `${offsets()[range().start]}px`, "padding-bottom": `${offsets().at(-1)! - offsets()[range().end]}px` }}>
    <For each={props.items.slice(range().start, range().end)}>{item => <div ref={element => measure(element, props.key(item))}>{props.children(item)}</div>}</For>
  </div>;
}
