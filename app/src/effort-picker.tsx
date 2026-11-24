import { For, Show } from "solid-js";
import { Press } from "./controls";
import { tr } from "./i18n";
import { preferences } from "./preferences";

// "xhigh" reads as XHigh
export const effortName = (level: string): string =>
  /^x[a-z]{3,}/.test(level) ? `X${effortName(level.slice(1))}` : level[0].toUpperCase() + level.slice(1);

type Props = { levels: string[]; value: string; onPick: (level: string) => void };

/** The model's effort levels as a slider or a row of buttons, whichever personalization chose. */
export function EffortPicker(props: Props & { onClose: () => void }) {
  return <Show when={preferences.effortPicker === "buttons"} fallback={<EffortSlider {...props} />}>
    <EffortButtons {...props} onPick={level => { props.onPick(level); props.onClose(); }} />
  </Show>;
}

function EffortButtons(props: Props) {
  return <div class="popup panel-frame effort-buttons">
    <For each={props.levels}>{level =>
      <Press label={effortName(level)} class="effort-button" selected={level === props.value} onClick={() => props.onPick(level)}>{effortName(level)}</Press>}
    </For>
  </div>;
}

function EffortStops(props: { levels: string[]; at: (index: number) => number }) {
  return <>
    <div class="effort-rail" />
    <For each={props.levels}>{(_, i) => <span class="effort-stop" style={{ left: `${props.at(i())}%` }} />}</For>
  </>;
}

// Stays open while dragging between levels; a click outside closes it
function EffortSlider(props: Props) {
  let track!: HTMLDivElement;
  const last = () => props.levels.length - 1;
  const index = () => Math.max(0, props.levels.indexOf(props.value));
  const at = (i: number) => last() ? i / last() * 100 : 0;
  const pick = (i: number) => {
    const level = props.levels[Math.min(last(), Math.max(0, i))];
    if (level !== props.value) props.onPick(level);
  };
  const pickAt = (x: number) => {
    const rect = track.getBoundingClientRect();
    pick(Math.round((x - rect.left) / rect.width * last()));
  };
  // Without preventDefault the webview starts dragging the selection and the slider stops getting moves
  const press = (event: PointerEvent) => {
    if (event.button !== 0) return;
    event.preventDefault();
    track.focus();
    track.setPointerCapture(event.pointerId);
    pickAt(event.clientX);
  };
  const move = (event: PointerEvent) => { if (track.hasPointerCapture(event.pointerId)) pickAt(event.clientX); };
  const key = (event: KeyboardEvent) => {
    const shift = { ArrowLeft: -1, ArrowDown: -1, ArrowRight: 1, ArrowUp: 1 }[event.key];
    if (!shift) return;
    event.preventDefault();
    pick(index() + shift);
  };
  return <div class="popup panel-frame effort-slider">
    <div class="effort-line"><span>{tr().composer.effortTitle}</span><span class="effort-value">{effortName(props.value)}</span></div>
    <div ref={track} class="effort-track" role="slider" tabindex="0" aria-label={tr().composer.effortTitle}
      aria-valuemin={0} aria-valuemax={last()} aria-valuenow={index()} aria-valuetext={effortName(props.value)}
      onPointerDown={press} onPointerMove={move} onKeyDown={key} onDragStart={event => event.preventDefault()}>
      <EffortStops levels={props.levels} at={at} />
      {/* The passed part is the same line and stops in white, uncovered together so a stop turns grey as the line leaves it */}
      <div class="effort-passed" style={{ "clip-path": `inset(-8px ${100 - at(index())}% -8px -8px)` }}>
        <EffortStops levels={props.levels} at={at} />
      </div>
      <span class="effort-thumb" style={{ left: `${at(index())}%` }} />
    </div>
    <div class="effort-scale"><span>{effortName(props.levels[0])}</span><span>{effortName(props.levels[last()])}</span></div>
  </div>;
}
