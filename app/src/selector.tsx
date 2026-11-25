import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { tr } from "./i18n";
import type { Provider } from "./data";
import { fitScroll, Glyph, IconButton, Press, SearchField } from "./controls";
import { Scrollbar } from "./scrollbar";

// The model list opens where it was left
let modelListScroll = 0;

export function Selector<T>(props: {
  items: T[]; selected?: T; name: (item: T) => string; details: (item: T) => string;
  group?: (item: T) => Provider; icon?: string; kind: "model" | "project" | "branch";
  onSelect: (item: T) => void; onAction?: () => void; error?: string;
}) {
  const [query, setQuery] = createSignal("");
  const filtered = createMemo(() => props.items.filter(item => `${props.name(item)} ${props.details(item)}`.toLowerCase().includes(query().trim().toLowerCase())));
  let searchInput!: HTMLInputElement;
  let popup!: HTMLDivElement;
  let list!: HTMLDivElement;
  const remembers = props.kind === "model";
  // Sized by the full list once, so filtering doesn't shrink it and move the search field
  onMount(() => requestAnimationFrame(() => {
    popup.style.height = `${popup.offsetHeight}px`;
    if (remembers) list.scrollTop = modelListScroll;
    searchInput?.focus();
  }));
  const placeholder = { model: tr().selector.searchModels, project: tr().selector.searchProjects, branch: tr().selector.searchBranches }[props.kind];
  return <div ref={popup} class={`popup selector panel-frame ${props.kind === "model" ? "model" : "project"}-popup`}>
    <div class="selector-head">
      <SearchField inputRef={input => searchInput = input} value={query()} onInput={setQuery}
        onEnter={() => { if (filtered().length === 1) props.onSelect(filtered()[0]); }} placeholder={placeholder} />
      <Show when={props.onAction}>
        <IconButton name={props.kind === "model" ? "sliders" : "plus"} label={props.kind === "model" ? tr().selector.filterModels : tr().selector.addProject}
          width={props.kind === "model" ? 32 : 26} onClick={props.onAction} />
      </Show>
    </div>
    <Show when={props.error}><div class="selector-error" title={props.error}>{props.error}</div></Show>
    <div ref={element => { list = element; fitScroll(element); }} class="selector-list"
      onScroll={() => { if (remembers && !query()) modelListScroll = list.scrollTop; }}>
      <Show when={filtered().length > 0} fallback={<div class="empty-search">{tr().common.nothingFound}</div>}>
        <For each={filtered()}>{(item, index) => {
          const group = props.group?.(item);
          const showGroup = () => group && (index() === 0 || props.group?.(filtered()[index() - 1]) !== group);
          return <>
            <Show when={showGroup()}><div class="selector-group"><Glyph name={group!.icon} size={16} /><span>{group!.name}</span></div></Show>
            <Press label={props.name(item)} class="selector-row" onClick={() => props.onSelect(item)}>
              <Show when={props.icon}><Glyph name={props.icon!} /></Show>
              <span class="truncate">{props.name(item)}</span>
              <Show when={item === props.selected}><Glyph name="check-small" size={15} /></Show>
            </Press>
          </>;
        }}</For>
      </Show>
    </div>
    <Show when={props.kind === "model"}><div class="selector-fade" /><Scrollbar target={() => list} /></Show>
  </div>;
}
