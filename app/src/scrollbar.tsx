import { createSignal, onCleanup, onMount } from "solid-js";

export type ScrollbarControl = { show: () => void; update: () => void };

/** A thin scrollbar over a scroller whose own one is hidden: it shows while scrolling or pointed at, and its thumb drags. */
export function Scrollbar(props: { target: () => HTMLElement; class?: string; control?: (control: ScrollbarControl) => void }) {
  let track!: HTMLDivElement;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const [thumb, setThumb] = createSignal({ top: 0, height: 0 });
  const [visible, setVisible] = createSignal(false);

  function update() {
    const target = props.target();
    if (!target || !track) return;
    const range = target.scrollHeight - target.clientHeight;
    if (range <= 1) {
      setThumb({ top: 0, height: 0 });
      setVisible(false);
      return;
    }
    const height = Math.max(12, track.clientHeight * target.clientHeight / target.scrollHeight);
    setThumb({ top: (track.clientHeight - height) * target.scrollTop / range, height });
  }

  function show() {
    update();
    if (!thumb().height) return;
    setVisible(true);
    clearTimeout(timer);
    timer = setTimeout(() => setVisible(false), 900);
  }

  function drag(event: PointerEvent) {
    event.preventDefault();
    clearTimeout(timer);
    setVisible(true);
    const target = props.target();
    const box = track.getBoundingClientRect();
    const grabbed = thumb();
    const offset = event.target === track ? grabbed.height / 2 : event.clientY - box.top - grabbed.top;
    const scrollTo = (clientY: number) => {
      const position = Math.max(0, Math.min(box.height - grabbed.height, clientY - box.top - offset));
      target.scrollTop = position / (box.height - grabbed.height) * (target.scrollHeight - target.clientHeight);
      update();
    };
    track.setPointerCapture(event.pointerId);
    scrollTo(event.clientY);
    const move = (next: PointerEvent) => scrollTo(next.clientY);
    const end = () => {
      track.removeEventListener("pointermove", move);
      track.removeEventListener("pointerup", end);
      track.removeEventListener("pointercancel", end);
      show();
    };
    track.addEventListener("pointermove", move);
    track.addEventListener("pointerup", end);
    track.addEventListener("pointercancel", end);
  }

  onMount(() => {
    const target = props.target();
    target.addEventListener("scroll", show, { passive: true });
    target.addEventListener("pointerenter", show);
    // A scroller that shrinks moves no scroll event, yet its thumb has to follow
    const resize = new ResizeObserver(update);
    resize.observe(target);
    onCleanup(() => {
      resize.disconnect();
      target.removeEventListener("scroll", show);
      target.removeEventListener("pointerenter", show);
    });
  });
  onCleanup(() => clearTimeout(timer));
  props.control?.({ show, update });

  return <div ref={track} class={`scroll-track ${props.class ?? ""}`} classList={{ visible: visible() && thumb().height > 0 }}
    onPointerDown={drag} onPointerEnter={show}>
    <div class="scroll-thumb" style={{ top: `${thumb().top}px`, height: `${thumb().height}px` }} />
  </div>;
}
