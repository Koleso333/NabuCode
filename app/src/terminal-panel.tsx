import { createEffect, createSignal, For, on, onMount, Show } from "solid-js";
import { tr } from "./i18n";
import { ContextMenu, Glyph, IconButton, MenuItem } from "./controls";
import { showFailure } from "./errors";
import { activeTerminal, closeTerminal, openTerminal, setTerminalScreen, shells, showTerminal, terminalsOf } from "./terminals";
import { terminalView } from "./terminal-view";

/** The bottom panel of a project's terminals: tabs over the screen of the shown one. */
export function TerminalPanel(props: { directory: string; left: number; height: number; onHeight: (height: number) => void; onHide: () => void }) {
  const text = () => tr().terminal;
  const list = () => terminalsOf(props.directory);
  const active = () => activeTerminal(props.directory);
  // The ⋮ menu, or the shells menu of a right click on «+»
  const [menu, setMenu] = createSignal<{ x: number; y: number; shells?: boolean } | null>(null);
  const [edges, setEdges] = createSignal({ start: true, end: true });
  let screen!: HTMLDivElement;
  let tabs!: HTMLDivElement;
  let bar!: HTMLDivElement;

  createEffect(on(() => active()?.id, id => {
    if (!id) return screen.replaceChildren();
    terminalView(id).show(screen);
    tabs.querySelector<HTMLElement>(`[data-id="${id}"]`)?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }));

  // The fades show only where tabs are hidden behind the edge
  const measure = () => setEdges({ start: tabs.scrollLeft < 1, end: tabs.scrollLeft + tabs.clientWidth > tabs.scrollWidth - 1 });
  onMount(() => {
    new ResizeObserver(measure).observe(tabs);
    setTerminalScreen(screen);
  });
  createEffect(on(() => list().length, () => requestAnimationFrame(measure)));

  const add = (shell?: string) => void openTerminal(props.directory, shell).catch(showFailure);
  const close = (id: string) => void closeTerminal(id).catch(showFailure);
  const closeAll = (keep?: string) => {
    setMenu(null);
    for (const item of list()) if (item.id !== keep) close(item.id);
  };
  // The panel hides when the last terminal of its project is gone, closed here or exited
  createEffect(on(() => [props.directory, list().length] as const, ([directory, count], previous) => {
    if (!count && previous?.[0] === directory && previous[1]) props.onHide();
  }));

  function startResize(event: PointerEvent) {
    event.preventDefault();
    const start = event.clientY, height = props.height;
    const move = (next: PointerEvent) => props.onHeight(height + start - next.clientY);
    const stop = () => { window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", stop); };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  }

  return <section class="terminal-panel" style={{ left: `${props.left}px`, height: `${props.height}px` }}>
    <div class="terminal-resize" onPointerDown={startResize} />
    <div ref={bar} class="terminal-bar">
      <div class="terminal-tabs-frame" classList={{ "at-start": edges().start, "at-end": edges().end }}>
        <div ref={tabs} class="terminal-tabs" role="tablist" onScroll={measure}>
          <For each={list()}>{item => <div class="terminal-tab" classList={{ active: item.id === active()?.id }} role="tab" data-id={item.id}
            aria-selected={item.id === active()?.id} onClick={() => showTerminal(props.directory, item.id)}
            onAuxClick={event => { if (event.button === 1) close(item.id); }}>
            <span>{text().title(item.number)}</span>
            <button type="button" class="terminal-tab-close" aria-label={text().close} title={text().close}
              onClick={event => { event.stopPropagation(); close(item.id); }}><Glyph name="close" size={11} /></button>
          </div>}</For>
        </div>
        <span class="terminal-fade start" /><span class="terminal-fade end" />
      </div>
      <IconButton name="plus-thin" label={text().new} size={15} width={25} class="terminal-button terminal-add" onClick={() => add()}
        onContextMenu={event => { event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY, shells: true }); }} />
      <span class="terminal-bar-space" />
      <IconButton name="dots-vertical" label={text().more} size={15} width={25} class="terminal-button terminal-more" selected={!!menu() && !menu()!.shells}
        onClick={() => {
          const box = bar.getBoundingClientRect();
          setMenu(menu() && !menu()!.shells ? null : { x: box.right - 240, y: box.bottom + 4 });
        }} />
      <IconButton name="minus" label={text().hide} size={15} width={25} class="terminal-button" onClick={props.onHide} />
    </div>
    <div ref={screen} class="terminal-screens" />
    <Show when={menu()?.shells ? menu() : null}>{at => <ContextMenu x={at().x} y={at().y} label={text().new} onClose={() => setMenu(null)}>
      <For each={shells()}>{shell => <MenuItem icon="terminal" label={shell.name} onClick={() => { setMenu(null); add(shell.id); }} />}</For>
    </ContextMenu>}</Show>
    <Show when={menu()?.shells ? null : menu()}>{at => <ContextMenu x={at().x} y={at().y} label={text().more} trigger={bar.querySelector<HTMLElement>(".terminal-more")!} onClose={() => setMenu(null)}>
      <MenuItem icon="plus" label={text().new} onClick={() => { setMenu(null); add(); }} />
      <Show when={list().length > 1}><MenuItem icon="close" label={text().closeOthers} onClick={() => closeAll(active()?.id)} /></Show>
      <MenuItem icon="trash" label={text().closeAll} onClick={() => closeAll()} />
    </ContextMenu>}</Show>
  </section>;
}
