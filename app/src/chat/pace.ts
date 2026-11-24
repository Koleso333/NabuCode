import { createComputed, createSignal, onCleanup, type Accessor } from "solid-js";

// One animation frame loop for every live text instead of a timer per part
const steps = new Set<() => boolean>();
let frame = 0;

function tick() {
  frame = 0;
  for (const step of steps) if (!step()) steps.delete(step);
  if (steps.size) frame = requestAnimationFrame(tick);
}

export function everyFrame(step: () => boolean) {
  steps.add(step);
  if (!frame) frame = requestAnimationFrame(tick);
  return () => { steps.delete(step); };
}

// Reveals streamed text smoothly; bursts from the provider are spread over a few frames
export function paced(text: Accessor<string>, live: Accessor<boolean>) {
  const [shown, setShown] = createSignal(text());
  let length = text().length;
  let stopLoop: (() => void) | undefined;

  const step = () => {
    const target = text();
    const backlog = target.length - length;
    if (backlog <= 0) { stopLoop = undefined; return false; }
    let next = length + Math.max(3, Math.ceil(backlog / 6));
    const space = target.slice(next, next + 8).search(/[\s.,!?;:)\]]/);
    if (space >= 0) next += space + 1;
    length = Math.min(target.length, next);
    setShown(target.slice(0, length));
    if (length < target.length) return true;
    stopLoop = undefined;
    return false;
  };

  // A computation, not an effect: the full text must be shown before anything sees `live` turn false
  createComputed(() => {
    const value = text();
    if (!live() || value.length < length) {
      stopLoop?.();
      stopLoop = undefined;
      length = value.length;
      setShown(value);
      return;
    }
    if (value.length > length) stopLoop ??= everyFrame(step);
  });
  onCleanup(() => stopLoop?.());
  return shown;
}
