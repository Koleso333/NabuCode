import { createSignal, Show, type JSX } from "solid-js";
import { tr } from "./i18n";
import { fitScroll, Glyph, Press, SearchField } from "./controls";

// Buttons without an action yet
const noop = () => {};

export function Page(props: { title: JSX.Element; action?: JSX.Element; search?: JSX.Element; children: JSX.Element }) {
  return <div ref={fitScroll} class="settings-page">
    <div class="settings-page-head">
      <div class="settings-page-title"><h2>{props.title}</h2>{props.action}</div>
      {props.search}
    </div>
    <div class="settings-page-body">{props.children}</div>
  </div>;
}

export function Section(props: { title?: string; head?: JSX.Element; children: JSX.Element; class?: string; note?: string }) {
  return <section class="settings-section">
    <Show when={props.head} fallback={<Show when={props.title}><h3>{props.title}</h3></Show>}>{props.head}</Show>
    <div class={`settings-card ${props.class ?? ""}`}>{props.children}</div>
    <Show when={props.note}><div class="settings-note">{props.note}</div></Show>
  </section>;
}

export type Text = { title: string; description?: string };

export function Row(props: { title: JSX.Element; description?: string; children?: JSX.Element; class?: string; onContextMenu?: (event: MouseEvent) => void }) {
  return <div class={`settings-row ${props.class ?? ""}`} onContextMenu={event => props.onContextMenu?.(event)}>
    <div class="settings-row-text"><span class="settings-row-title">{props.title}</span>
      <Show when={props.description}><span class="settings-row-description">{props.description}</span></Show>
    </div>
    <div class="settings-row-control">{props.children}</div>
  </div>;
}

export function TextRow(props: { text: Text; children?: JSX.Element }) {
  return <Row title={props.text.title} description={props.text.description}>{props.children}</Row>;
}

export function ToggleRow(props: { text: Text; on: boolean; onChange: (on: boolean) => void }) {
  return <TextRow text={props.text}><Toggle label={props.text.title} on={props.on} onChange={props.onChange} /></TextRow>;
}

export function Toggle(props: { on?: boolean; label: string; onChange?: (on: boolean) => void }) {
  const [local, setLocal] = createSignal(props.on ?? false);
  const on = () => props.onChange ? !!props.on : local();
  return <button type="button" role="switch" class="toggle" aria-checked={on()} aria-label={props.label}
    onClick={() => props.onChange ? props.onChange(!on()) : setLocal(!local())}><span class="toggle-track"><span class="toggle-knob" /></span></button>;
}

export function TextBox(props: {
  value?: string; placeholder?: string; label: string; width?: number; type?: "text" | "number"; class?: string;
  onChange?: (value: string) => void; onInput?: (value: string) => void; onEnter?: () => void;
}) {
  return <input class={`text-box ${props.class ?? ""}`} type={props.type ?? "text"} value={props.value ?? ""} placeholder={props.placeholder} aria-label={props.label}
    spellcheck={false} autocomplete="off" style={{ width: props.width ? `${props.width}px` : undefined }}
    onChange={event => props.onChange?.(event.currentTarget.value)} onInput={event => props.onInput?.(event.currentTarget.value)}
    onKeyDown={event => { if (event.key === "Enter") props.onEnter?.(); }} />;
}

export function Button(props: { label: string; icon?: string; variant?: "ghost"; disabled?: boolean; onClick?: () => void }) {
  return <Press label={props.label} class={`settings-button ${props.variant ?? "secondary"}`} disabled={props.disabled} onClick={props.onClick ?? noop}>
    <Show when={props.icon}><Glyph name={props.icon!} size={16} /></Show><span>{props.label}</span>
  </Press>;
}

export function Empty(props: { message: string }) {
  return <div class="settings-empty">{props.message}</div>;
}

export function FilterField(props: { value: string; placeholder: string; onInput: (value: string) => void }) {
  return <div class="settings-search">
    <SearchField value={props.value} placeholder={props.placeholder} onInput={props.onInput} />
    <Show when={props.value}><Press label={tr().common.clear} class="settings-search-clear" onClick={() => props.onInput("")}><Glyph name="circle-x" size={16} /></Press></Show>
  </div>;
}
