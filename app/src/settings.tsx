import { createMemo, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import { cancelPreview, previewTheme, saveTheme, themeChoice, themeIds, themeName, type ThemeMode } from "./theme";
import { language, languageNames, setLanguage, tr, type Dictionary, type Language } from "./i18n";
import { models } from "./data";
import { preferences, setPreference, type EffortPickerKind, type MarkStyle, type Preferences } from "./preferences";
import { catalog, catalogFailed, fontChoices, isDownloading, loadCatalog, previewFont, previewStack, type FontKind } from "./fonts";
import { closeOnOutside, fitScroll, Glyph, Press, Ring, SearchField, SidebarRow } from "./controls";
import { Button, Empty, FilterField, Page, Row, Section, TextBox, TextRow, ToggleRow } from "./settings-parts";
import { ModelsPage, ProvidersPage } from "./settings-providers";
import { shells } from "./terminals";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { describeEditor, findEditors, type Editor } from "./files";

export type SettingsTab = "general" | "personalization" | "shortcuts" | "compaction" | "providers" | "models";

const tabs: { group?: keyof Dictionary["settings"]["groups"]; items: { id: SettingsTab; icon: string }[] }[] = [
  { items: [{ id: "general", icon: "sliders" }, { id: "personalization", icon: "brush" }, { id: "shortcuts", icon: "keyboard" }] },
  { group: "agent", items: [{ id: "compaction", icon: "sliders" }] },
  { group: "server", items: [{ id: "providers", icon: "providers" }, { id: "models", icon: "models" }] },
];

export function SettingsNav(props: { tab: SettingsTab; onTab: (tab: SettingsTab) => void; onBack: () => void }) {
  return <div class="settings-nav">
    <div class="settings-nav-head">
      <Press label={tr().common.back} class="settings-back" onClick={props.onBack}><Glyph name="back" size={14} /></Press>
      <span>{tr().settings.title}</span>
    </div>
    <For each={tabs}>{block => <>
      <Show when={block.group}>{group => <div class="settings-nav-group">{tr().settings.groups[group()]}</div>}</Show>
      <For each={block.items}>{item => <>
        <div class="sidebar-primary-gap" />
        <SidebarRow title={tr().settings.tabs[item.id]} icon={item.icon} large selected={props.tab === item.id} onClick={() => props.onTab(item.id)} />
      </>}</For>
    </>}</For>
    <div class="settings-version"><span>{tr().settings.version}</span><span>v0.1.0-ev</span></div>
  </div>;
}

export function SettingsPanel(props: { tab: SettingsTab }) {
  return <div class="settings-panel">
    <Show when={props.tab === "general"}><GeneralPage /></Show>
    <Show when={props.tab === "personalization"}><PersonalizationPage /></Show>
    <Show when={props.tab === "shortcuts"}><ShortcutsPage /></Show>
    <Show when={props.tab === "compaction"}><CompactionPage /></Show>
    <Show when={props.tab === "providers"}><ProvidersPage /></Show>
    <Show when={props.tab === "models"}><ModelsPage /></Show>
  </div>;
}

type MenuPosition = { left: number; top?: number; bottom?: number; width: number; height: number };

// Opens below the trigger when it fits, otherwise on the roomier side, shrunk to the space left
function placeMenu(trigger: HTMLElement, width: number, height: number): MenuPosition {
  const gap = 4, margin = 8, titlebar = 36;
  const rect = trigger.getBoundingClientRect();
  const left = Math.max(margin, Math.min(rect.right - width, innerWidth - width - margin));
  const below = innerHeight - rect.bottom - gap - margin;
  const above = rect.top - gap - margin - titlebar;
  if (height <= below || below >= above) return { left, top: rect.bottom + gap, width, height: Math.min(height, below) };
  return { left, bottom: innerHeight - rect.top + gap, width, height: Math.min(height, above) };
}

const menuStyle = (position: MenuPosition) => ({
  left: `${position.left}px`, width: `${position.width}px`, height: `${position.height}px`,
  top: position.top === undefined ? undefined : `${position.top}px`, bottom: position.bottom === undefined ? undefined : `${position.bottom}px`,
});

function Choice<T extends string>(props: {
  options: T[]; value: T; label: string; name?: (value: T) => string;
  onChange?: (value: T) => void; onPreview?: (value: T) => void; onPreviewEnd?: () => void;
}) {
  const name = (value: T) => props.name ? props.name(value) : value;
  const [menu, setMenu] = createSignal<MenuPosition | null>(null);
  let trigger!: HTMLButtonElement;
  function open() {
    const width = Math.max(trigger.getBoundingClientRect().width, 160);
    setMenu(placeMenu(trigger, width, Math.min(props.options.length * 28 + 10, 280)));
  }
  function pick(next: T) {
    props.onChange?.(next);
    setMenu(null);
  }
  return <>
    <button ref={trigger} type="button" class="choice" classList={{ open: !!menu() }} aria-label={props.label}
      onClick={() => menu() ? setMenu(null) : open()}>
      <span class="truncate">{name(props.value)}</span><Glyph name="chevron-down" size={13} />
    </button>
    <Show when={menu()}>{position => <ChoiceMenu position={position()} options={props.options} value={props.value} name={name} onPick={pick} onClose={() => setMenu(null)}
      onPreview={props.onPreview} onPreviewEnd={props.onPreviewEnd} trigger={trigger} />}</Show>
  </>;
}

function ChoiceMenu<T extends string>(props: {
  position: MenuPosition; options: T[]; value: T; name: (value: T) => string;
  onPick: (value: T) => void; onClose: () => void; onPreview?: (value: T) => void; onPreviewEnd?: () => void; trigger: HTMLElement;
}) {
  let list!: HTMLDivElement;
  onMount(() => {
    closeOnOutside(list, props.trigger, props.onClose);
    list.querySelector<HTMLElement>(".selected")?.scrollIntoView({ block: "nearest" });
    onCleanup(() => props.onPreviewEnd?.());
  });
  return <Portal><div ref={list} class="choice-menu panel-frame" role="listbox" onPointerLeave={() => props.onPreviewEnd?.()} style={menuStyle(props.position)}>
    <For each={props.options}>{option =>
      <Press label={props.name(option)} class="choice-option" selected={option === props.value} onClick={() => props.onPick(option)}
        onPointerEnter={() => props.onPreview?.(option)}>
        <span class="truncate">{props.name(option)}</span><Show when={option === props.value}><Glyph name="check-small" size={15} /></Show>
      </Press>}</For>
  </div></Portal>;
}

function FontPicker(props: { kind: FontKind; label: string }) {
  const [menu, setMenu] = createSignal<MenuPosition | null>(null);
  const family = () => preferences.fonts[props.kind];
  let trigger!: HTMLButtonElement;
  function open() {
    void loadCatalog();
    setMenu(placeMenu(trigger, 260, 300));
  }
  function pick(next: string) {
    setPreference("fonts", props.kind, next);
    setMenu(null);
  }
  return <>
    <Show when={isDownloading(family())}><Ring fraction={.68} size={13} loading /></Show>
    <button ref={trigger} type="button" class="choice" classList={{ open: !!menu() }} aria-label={props.label} onClick={() => menu() ? setMenu(null) : open()}>
      <span class="truncate" style={{ "font-family": previewStack(family(), props.kind) }}>{family() || tr().settings.personalization.fonts.default}</span>
      <Glyph name="chevron-down" size={13} />
    </button>
    <Show when={menu()}>{position => <FontMenu position={position()} kind={props.kind} value={family()} trigger={trigger} onPick={pick} onClose={() => setMenu(null)} />}</Show>
  </>;
}

function FontMenu(props: { position: MenuPosition; kind: FontKind; value: string; trigger: HTMLElement; onPick: (family: string) => void; onClose: () => void }) {
  const text = () => tr().settings.personalization.fonts;
  const [query, setQuery] = createSignal("");
  const fonts = createMemo(() => fontChoices(props.kind, query()));
  const status = () => catalogFailed() ? text().failed : !catalog() ? text().loading : !fonts().length ? tr().common.nothingFound : "";
  let panel!: HTMLDivElement;
  let search!: HTMLInputElement;
  onMount(() => {
    closeOnOutside(panel, props.trigger, props.onClose);
    requestAnimationFrame(() => search.focus());
  });
  const row = (family: string, name: string, cyrillic = true) =>
    <Press label={name} class="selector-row" onClick={() => props.onPick(family)}>
      <span class="truncate" style={{ "font-family": previewStack(family, props.kind) }}>{name}</span>
      <Show when={!cyrillic && language() === "ru"}><span class="font-note">{text().noCyrillic}</span></Show>
      <Show when={family === props.value}><Glyph name="check-small" size={15} /></Show>
    </Press>;
  return <Portal><div ref={panel} class="popup selector font-popup panel-frame" style={menuStyle(props.position)}>
    <div class="selector-head">
      <SearchField inputRef={input => search = input} value={query()} onInput={setQuery} placeholder={text().search}
        onEnter={() => { if (fonts().length === 1) props.onPick(fonts()[0].family); }} />
    </div>
    <div ref={fitScroll} class="selector-list">
      <Show when={!query().trim()}>{row("", text().default)}</Show>
      <For each={fonts()}>{font => { previewFont(font.family); return row(font.family, font.family, font.cyrillic); }}</For>
      <Show when={status()}><div class="empty-search">{status()}</div></Show>
    </div>
    <div class="selector-fade" />
  </div></Portal>;
}

const systemEditor: Editor = { path: "", name: "", icon: "" };

function EditorIcon(props: { editor: Editor; fallback?: string }) {
  return <Show when={props.editor.icon} fallback={<span class="editor-icon"><Glyph name={props.fallback ?? "file"} size={16} /></span>}>
    <img class="editor-icon" src={props.editor.icon} width={16} height={16} alt="" draggable={false} />
  </Show>;
}

function EditorPicker(props: { label: string }) {
  const text = () => tr().settings.general;
  const [found] = createResource(() => findEditors().catch(() => [] as Editor[]));
  const [menu, setMenu] = createSignal<MenuPosition | null>(null);
  const chosen = () => preferences.editor;
  // A program picked by hand stays in the list even when the search doesn't find it
  const editors = () => {
    const list = found() ?? [];
    return !chosen().path || list.some(item => item.path === chosen().path) ? list : [...list, chosen()];
  };
  const name = (editor: Editor) => editor.path ? editor.name : text().editorDefault;
  let trigger!: HTMLButtonElement;
  let panel!: HTMLDivElement;
  function pick(editor: Editor) {
    setPreference("editor", { ...editor });
    setMenu(null);
  }
  async function pickProgram() {
    setMenu(null);
    const path = await openDialog({ multiple: false, directory: false, filters: [{ name: text().editorPick, extensions: ["exe"] }] });
    if (typeof path === "string") pick(await describeEditor(path));
  }
  const option = (editor: Editor) =>
    <Press label={name(editor)} class="choice-option editor-option" selected={editor.path === chosen().path} onClick={() => pick(editor)}>
      <EditorIcon editor={editor} /><span class="truncate">{name(editor)}</span>
      <Show when={editor.path === chosen().path}><Glyph name="check-small" size={15} /></Show>
    </Press>;
  return <>
    <button ref={trigger} type="button" class="choice" classList={{ open: !!menu() }} aria-label={props.label}
      onClick={() => menu() ? setMenu(null) : setMenu(placeMenu(trigger, 230, editors().length * 28 + 2 * 28 + 2 * 9 + 10))}>
      <EditorIcon editor={chosen()} /><span class="truncate">{name(chosen())}</span><Glyph name="chevron-down" size={13} />
    </button>
    <Show when={menu()}>{position => <Portal><div ref={element => { panel = element; onMount(() => closeOnOutside(panel, trigger, () => setMenu(null))); }}
      class="choice-menu panel-frame" role="listbox" style={menuStyle(position())}>
      {option(systemEditor)}
      <Show when={editors().length}><div class="choice-separator" /></Show>
      <For each={editors()}>{option}</For>
      <div class="choice-separator" />
      <Press label={text().editorPick} class="choice-option editor-option" onClick={() => void pickProgram().catch(() => {})}>
        <EditorIcon editor={systemEditor} fallback="folder" /><span class="truncate">{text().editorPick}</span>
      </Press>
    </div></Portal>}</Show>
  </>;
}

// Without a server there is only the default shell
const shellIds = () => shells().length ? shells().map(shell => shell.id) : [""];

function GeneralPage() {
  const text = () => tr().settings.general;
  return <Page title={tr().settings.tabs.general}>
    <Section>
      <TextRow text={text().language}>
        <Choice<Language> label={text().language.title} options={["ru", "en"]} value={language()} name={id => languageNames[id]} onChange={setLanguage} />
      </TextRow>
    </Section>
    <Show when={"__TAURI_INTERNALS__" in window}>
      <Section title={text().files}>
        <Row title={text().editor.title} description={text().editor.description} class="editor-row"><EditorPicker label={text().editor.title} /></Row>
      </Section>
    </Show>
    <Section title={text().terminal}>
      <TextRow text={text().shell}><Choice label={text().shell.title} options={shellIds()} value={shellIds().includes(preferences.shell) ? preferences.shell : shellIds()[0]}
        name={id => shells().find(shell => shell.id === id)?.name ?? "PowerShell"} onChange={shell => setPreference("shell", shell)} /></TextRow>
      <ToggleRow text={text().syntax} on={preferences.syntax} onChange={on => setPreference("syntax", on)} />
    </Section>
    <Section title={text().updates}>
      <ToggleRow text={text().checkOnStartup} on={preferences.checkOnStartup} onChange={on => setPreference("checkOnStartup", on)} />
      <ToggleRow text={text().releaseNotes} on={preferences.releaseNotes} onChange={on => setPreference("releaseNotes", on)} />
      <TextRow text={text().checkUpdates}><Button label={text().checkNow} disabled /></TextRow>
    </Section>
  </Page>;
}

const sounds = ["none", ...[1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map(n => `Alert ${String(n).padStart(2, "0")}`), ...[1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map(n => `Bip-bop ${String(n).padStart(2, "0")}`),
  ...[1, 2, 3, 4, 5, 6, 7].map(n => `Staplebops 0${n}`), ...[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12].map(n => `Nope ${String(n).padStart(2, "0")}`), ...[1, 2, 3, 4, 5, 6].map(n => `Yup 0${n}`)];

type AlertEvent = keyof Preferences["notifications"];
type ChatOption = keyof Preferences["chat"];

function PersonalizationPage() {
  const text = () => tr().settings.personalization;
  const soundName = (sound: string) => sound === "none" ? text().noSound : sound;
  const soundRow = (event: AlertEvent) =>
    <TextRow text={text().events[event]}><Choice label={text().sound(text().events[event].title)} options={sounds} value={preferences.sounds[event]} name={soundName}
      onChange={sound => setPreference("sounds", event, sound)} /></TextRow>;
  const fontRow = (font: FontKind) => <TextRow text={text()[`${font}Font`]}><FontPicker kind={font} label={text()[`${font}Font`].title} /></TextRow>;
  const chatRow = (option: ChatOption) =>
    <ToggleRow text={text()[option]} on={preferences.chat[option]} onChange={on => setPreference("chat", option, on)} />;
  const notifyRow = (event: AlertEvent) =>
    <ToggleRow text={text().events[event]} on={preferences.notifications[event]} onChange={on => setPreference("notifications", event, on)} />;
  return <Page title={tr().settings.tabs.personalization}>
    <Section title={text().appearance}>
      <TextRow text={text().colorScheme}>
        <Choice<ThemeMode> label={text().colorScheme.title} options={["system", "light", "dark"]} value={themeChoice().mode} name={mode => text().schemes[mode]}
          onChange={mode => saveTheme({ mode })} onPreview={mode => previewTheme({ mode })} onPreviewEnd={cancelPreview} />
      </TextRow>
      <TextRow text={text().theme}>
        <Choice label={text().theme.title} options={themeIds} value={themeChoice().theme} name={themeName}
          onChange={theme => saveTheme({ theme })} onPreview={theme => previewTheme({ theme })} onPreviewEnd={cancelPreview} />
      </TextRow>
      {fontRow("ui")}
      {fontRow("code")}
      {fontRow("terminal")}
    </Section>
    <Section title={text().chat}>
      {chatRow("responseStages")}
      {chatRow("reasoning")}
      {chatRow("expandCommands")}
      {chatRow("diffSummary")}
      {chatRow("expandEdits")}
      {chatRow("progressBar")}
      <TextRow text={text().effortPicker}>
        <Choice<EffortPickerKind> label={text().effortPicker.title} options={["slider", "buttons"]} value={preferences.effortPicker}
          name={kind => text().effortPickers[kind]} onChange={kind => setPreference("effortPicker", kind)} />
      </TextRow>
      <TextRow text={text().markStyle}>
        <Choice<MarkStyle> label={text().markStyle.title} options={["classic", "difference"]} value={preferences.markStyle}
          name={style => text().markStyles[style]} onChange={style => setPreference("markStyle", style)} />
      </TextRow>
    </Section>
    <Section title={text().notifications}>
      {notifyRow("agent")}
      {notifyRow("permissions")}
      {notifyRow("errors")}
    </Section>
    <Section title={text().sounds}>
      {soundRow("agent")}
      {soundRow("permissions")}
      {soundRow("errors")}
    </Section>
  </Page>;
}

type ShortcutText = Dictionary["settings"]["shortcuts"];

const shortcuts: { group: keyof ShortcutText["groups"]; items: [keyof ShortcutText["actions"], string?][] }[] = [
  { group: "general", items: [["palette", "Ctrl+Shift+P"], ["openSettings", "Ctrl+,"], ["toggleSidebar", "Ctrl+B"],
    ["openProject", "Ctrl+O"], ["previousProject", "Ctrl+Alt+↑"], ["nextProject", "Ctrl+Alt+↓"], ["nextTheme"]] },
  { group: "session", items: [["newSession", "Ctrl+Shift+S"], ["previousSession", "Alt+↑"], ["nextSession", "Alt+↓"],
    ["previousUnread", "Shift+Alt+↑"], ["nextUnread", "Shift+Alt+↓"], ["archiveSession", "Ctrl+Shift+Backspace"],
    ["previousMessage", "Ctrl+Alt+["], ["nextMessage", "Ctrl+Alt+]"], ["toggleChanges", "Ctrl+Shift+R"], ["compactSession"]] },
  { group: "navigation", items: [["openFile", "Ctrl+K, Ctrl+P"], ["closeTab", "Ctrl+W"], ["toggleFileTree", "Ctrl+\\"]] },
  { group: "modelAndAgent", items: [["chooseModel", "Ctrl+'"], ["nextEffort", "Ctrl+Shift+D"], ["toggleMcp", "Ctrl+;"]] },
  { group: "terminal", items: [["toggleTerminal", "Ctrl+`"], ["newTerminal", "Ctrl+Alt+T"]] },
  { group: "prompt", items: [["focusInput", "Ctrl+L"], ["attachFile", "Ctrl+U"]] },
];

function ShortcutsPage() {
  const text = () => tr().settings.shortcuts;
  const [filter, setFilter] = createSignal("");
  const [active, setActive] = createSignal<string | null>(null);
  const groups = createMemo(() => shortcuts
    .map(group => ({ ...group, items: group.items.filter(([action]) => text().actions[action].toLowerCase().includes(filter().trim().toLowerCase())) }))
    .filter(group => group.items.length));
  return <Page title={tr().settings.tabs.shortcuts} action={<Button label={text().reset} disabled />}
    search={<FilterField value={filter()} placeholder={text().search} onInput={setFilter} />}>
    <For each={groups()}>{group => <Section title={text().groups[group.group]}>
      <For each={group.items}>{([action, keys]) => <Row title={text().actions[action]} class="regular">
        <button type="button" class="keybind" classList={{ active: active() === action }} onClick={() => setActive(action)} onBlur={() => setActive(null)}>
          {active() === action ? text().pressKeys : keys ?? text().unassigned}
        </button>
      </Row>}</For>
    </Section>}</For>
    <Show when={!groups().length}><Empty message={tr().common.nothingFound} /></Show>
  </Page>;
}

function CompactionPage() {
  const text = () => tr().settings.compaction;
  return <Page title={tr().settings.tabs.compaction}>
    <Section>
      <ToggleRow text={text().auto} on={preferences.compaction.auto} onChange={on => setPreference("compaction", "auto", on)} />
      <Row title={text().threshold}><TextBox label={text().thresholdLabel} type="number" value={String(preferences.compaction.threshold)} width={80}
        onChange={value => setPreference("compaction", "threshold", Math.min(99, Math.max(1, Math.round(Number(value) || 75))))} /></Row>
      <Row title={text().model}>
        <Choice<"default" | "custom"> label={text().model} options={["default", "custom"]} value={preferences.compaction.custom ? "custom" : "default"}
          name={id => text().models[id]} onChange={value => setPreference("compaction", "custom", value === "custom")} />
      </Row>
      <Show when={preferences.compaction.custom}>
        <Row title={text().selected}><Choice label={text().selected} options={models.map(model => model.name)}
          value={preferences.compaction.model || models[1].name} onChange={model => setPreference("compaction", "model", model)} /></Row>
      </Show>
    </Section>
  </Page>;
}
