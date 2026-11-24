import { createSignal, onCleanup, onMount, Show, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { tr } from "./i18n";

export function Glyph(props: { name: string; size?: number; color?: string }) {
  const size = () => props.size ?? 18;
  return props.name === "explorer"
    ? <img class="glyph" src="/icons/explorer.svg" width={size()} height={size()} alt="" draggable={false} />
    : <span class="glyph glyph-mask" style={{ width: `${size()}px`, height: `${size()}px`, color: props.color, "mask-image": `url('/icons/${props.name}.svg')`, "-webkit-mask-image": `url('/icons/${props.name}.svg')` }} aria-hidden="true" />;
}

export function Press(props: {
  label: string; children: JSX.Element; onClick?: () => void; class?: string;
  selected?: boolean; disabled?: boolean; title?: string; style?: JSX.CSSProperties; panelTrigger?: string;
  variant?: "primary"; onContextMenu?: JSX.EventHandlerUnion<HTMLButtonElement, MouseEvent>; onPointerEnter?: () => void;
}) {
  return <button type="button" class={`press ${props.class || ""}`} classList={{ selected: props.selected }}
    aria-label={props.label} title={props.title ?? props.label} disabled={props.disabled || !props.onClick}
    data-panel-trigger={props.panelTrigger} data-variant={props.variant} style={props.style} onClick={() => props.onClick?.()}
    onContextMenu={props.onContextMenu} onPointerEnter={() => props.onPointerEnter?.()}>{props.children}</button>;
}

export function IconButton(props: {
  name: string; label: string; onClick?: () => void; size?: number; width?: number; class?: string; selected?: boolean;
  onContextMenu?: JSX.EventHandlerUnion<HTMLButtonElement, MouseEvent>;
}) {
  return <Press label={props.label} class={`icon-button ${props.class ?? ""}`} onClick={props.onClick} selected={props.selected} onContextMenu={props.onContextMenu}
    style={{ width: `${props.width ?? 31}px` }}><Glyph name={props.name} size={props.size ?? 18} /></Press>;
}

export function SidebarRow(props: {
  title: string; onClick?: () => void; icon?: string; tail?: JSX.Element; selected?: boolean; large?: boolean;
  onContextMenu?: JSX.EventHandlerUnion<HTMLButtonElement, MouseEvent>;
}) {
  return <Press label={props.title} onClick={props.onClick} class={`sidebar-row ${props.large ? "large" : ""}`} selected={props.selected} onContextMenu={props.onContextMenu}>
    <Show when={props.icon}><span class="sidebar-row-icon"><Glyph name={props.icon!} size={props.large ? 20 : 18} /></span></Show>
    <span class="truncate">{props.title}</span>{props.tail}
  </Press>;
}

export function SearchField(props: { value: string; placeholder: string; onInput: (value: string) => void; onEnter?: () => void; inputRef?: (input: HTMLInputElement) => void; class?: string }) {
  const [editing, setEditing] = createSignal(false);
  return <div class={`selector-search ${props.class ?? ""}`}><Glyph name="magnifying-glass" size={16} />
    <input ref={props.inputRef} type="search" aria-label={props.placeholder} placeholder={props.placeholder}
      value={props.value} style={{ "caret-color": editing() ? "var(--strong)" : "transparent" }}
      onClick={() => setEditing(true)} onInput={event => { setEditing(true); props.onInput(event.currentTarget.value); }}
      onKeyDown={event => { if (event.key === "Enter") props.onEnter?.(); }} />
  </div>;
}

// loading: a partial ring spins clockwise as a busy indicator
export function Ring(props: { fraction: number; size?: number; loading?: boolean }) {
  const size = () => props.size ?? 17;
  const center = () => size() / 2;
  const radius = () => center() - 1;
  const circumference = () => 2 * Math.PI * radius();
  return <svg class="ring" classList={{ loading: props.loading }} width={size()} height={size()} viewBox={`0 0 ${size()} ${size()}`} aria-hidden="true">
    <circle cx={center()} cy={center()} r={radius()} fill="none" stroke="var(--input-outline)" stroke-width="2" />
    <circle cx={center()} cy={center()} r={radius()} fill="none" stroke="var(--text)" stroke-width="2"
      stroke-dasharray={`${Math.min(1, Math.max(0, props.fraction)) * circumference()} ${circumference()}`}
      transform={`rotate(-90 ${center()} ${center()})`} />
  </svg>;
}

// Call inside onMount: closes on outside click, Escape, outer scroll and resize
export function closeOnOutside(menu: HTMLElement, trigger: HTMLElement | undefined, onClose: () => void) {
  const outside = (event: PointerEvent) => { if (!menu.contains(event.target as Node) && !trigger?.contains(event.target as Node)) onClose(); };
  const escape = (event: KeyboardEvent) => { if (event.key === "Escape") { event.stopImmediatePropagation(); onClose(); } };
  const scroll = (event: Event) => { if (!menu.contains(event.target as Node)) onClose(); };
  window.addEventListener("pointerdown", outside, true);
  window.addEventListener("keydown", escape, true);
  window.addEventListener("scroll", scroll, true);
  window.addEventListener("resize", onClose);
  onCleanup(() => {
    window.removeEventListener("pointerdown", outside, true);
    window.removeEventListener("keydown", escape, true);
    window.removeEventListener("scroll", scroll, true);
    window.removeEventListener("resize", onClose);
  });
}

// Use as ref on a scroller: the bottom padding is dropped while the content fits without it, so it never scrolls by the padding alone;
// "at-end" marks the scroller when nothing is left below
export function fitScroll(scroller: HTMLElement) {
  const check = () => {
    const padding = scroller.classList.contains("fits") ? 0 : parseFloat(getComputedStyle(scroller).paddingBottom);
    scroller.classList.toggle("fits", scroller.scrollHeight - padding <= scroller.clientHeight);
    scroller.classList.toggle("at-end", scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 1);
  };
  const resizes = new ResizeObserver(check);
  const mutations = new MutationObserver(() => {
    for (const child of scroller.children) resizes.observe(child);
    check();
  });
  resizes.observe(scroller);
  for (const child of scroller.children) resizes.observe(child);
  mutations.observe(scroller, { childList: true, subtree: true, characterData: true });
  scroller.addEventListener("scroll", check, { passive: true });
  onMount(check);
  onCleanup(() => { resizes.disconnect(); mutations.disconnect(); });
}

// Opens at the cursor, shifted to stay inside the window; a press on `trigger` is left to the trigger to toggle
export function ContextMenu(props: { x: number; y: number; label: string; onClose: () => void; children: JSX.Element; trigger?: HTMLElement }) {
  const [position, setPosition] = createSignal({ left: props.x, top: props.y });
  let menu!: HTMLDivElement;
  onMount(() => {
    const margin = 8;
    setPosition({
      left: Math.max(margin, Math.min(props.x, innerWidth - menu.offsetWidth - margin)),
      top: Math.max(margin, Math.min(props.y, innerHeight - menu.offsetHeight - margin)),
    });
    closeOnOutside(menu, props.trigger, props.onClose);
  });
  return <Portal><div ref={menu} class="context-menu" role="menu" aria-label={props.label} style={{ left: `${position().left}px`, top: `${position().top}px` }}>
    {props.children}
  </div></Portal>;
}

// Opens under the anchor, right edges aligned; flips above when there is no room below
export function Popover(props: { anchor: HTMLElement; title: string; onClose: () => void; children: JSX.Element }) {
  const [position, setPosition] = createSignal({ left: 0, top: 0 });
  let card!: HTMLDivElement;
  onMount(() => {
    const margin = 8, gap = 4;
    const anchor = props.anchor.getBoundingClientRect();
    const below = anchor.bottom + gap;
    setPosition({
      left: Math.max(margin, Math.min(anchor.right - card.offsetWidth, innerWidth - card.offsetWidth - margin)),
      top: below + card.offsetHeight + margin > innerHeight ? Math.max(margin, anchor.top - gap - card.offsetHeight) : below,
    });
    closeOnOutside(card, props.anchor, props.onClose);
  });
  return <Portal><div ref={card} class="popover" role="dialog" aria-label={props.title} style={{ left: `${position().left}px`, top: `${position().top}px` }}>
    <div class="popover-head"><span class="truncate">{props.title}</span><IconButton name="close" label={tr().common.close} size={14} width={22} onClick={props.onClose} /></div>
    <div class="popover-body">{props.children}</div>
  </div></Portal>;
}

export function MenuItem(props: { icon: string; label: string; onClick: () => void }) {
  return <button type="button" role="menuitem" onClick={() => props.onClick()}><span class="context-menu-icon"><Glyph name={props.icon} size={16} /></span>{props.label}</button>;
}

export function ConfirmDialog(props: { title: string; description: string; action: string; onConfirm: () => void; onClose: () => void }) {
  let card!: HTMLDivElement;
  let confirm!: HTMLButtonElement;
  onMount(() => {
    closeOnOutside(card, undefined, props.onClose);
    confirm.focus();
  });
  return <Portal><div class="dialog-backdrop">
    <div ref={card} class="confirm-dialog panel-frame" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" aria-describedby="confirm-description">
      <h2 id="confirm-title">{props.title}</h2>
      <p id="confirm-description">{props.description}</p>
      <div class="confirm-actions">
        <Press label={tr().common.cancel} class="confirm-button" onClick={props.onClose}>{tr().common.cancel}</Press>
        <button ref={confirm} type="button" class="press confirm-button" data-variant="primary" onClick={() => { props.onConfirm(); props.onClose(); }}>{props.action}</button>
      </div>
    </div>
  </div></Portal>;
}
