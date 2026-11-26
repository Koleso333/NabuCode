import { createEffect, createMemo, createSignal, For, on, onCleanup, onMount, Show, untrack, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { applyTheme, importTheme, previewedTheme, previewTheme, themeIds, type ThemeChoice } from "./theme";
import { tr } from "./i18n";
import { applyFonts, installFont, loadCatalog } from "./fonts";
import { preferences, setPreference } from "./preferences";
import { contextUsage, createDemo, demoContextParts, demoUsage, models, type Project, type Session } from "./data";
import type { Prompt, UsageBar } from "@nabu/protocol";
import { barFraction, Usage } from "./usage";
import { entryOf, usageOf, watchProvider } from "./providers";
import { openProviderPage } from "./settings-providers";
import { chosenEffort, chosenFast, chosenModel, contextOf, effortFor, keyOf, modelRef, pickEffort, pickFast, pickModel } from "./composer-model";
import { EffortPicker, effortName } from "./effort-picker";
import { ConfirmDialog, ContextMenu, Glyph, IconButton, MenuItem, Press, SidebarRow, Ring } from "./controls";
import { SettingsNav, SettingsPanel, type SettingsTab } from "./settings";
import { chats, ensureChat, type Attachment } from "./chat/model";
import { AttachmentTray, DropOverlay, pickFiles, readAttachment, restoreAttachment, readPasted, releaseAttachment, watchFileDrops } from "./attachments";
import { play } from "./chat/engine";
import { demoMode, directServer, stopChat } from "./chat/actions";
import { archiveSession, busy as serverBusy, createSession, deleteSession, interruptSession, openChat, renameSession, queues, recentCount, send as sendToServer, unqueue, serverPath, sessions as serverSessions, watchDirectories } from "./chat/remote";
import { addProject as addServerProject, listBranches, projectsLoaded, removeProject, renameProject, reorderProjects, samePath, serverProjects, switchBranch } from "./projects";
import { createReorder } from "./reorder";
import { Scrollbar, type ScrollbarControl } from "./scrollbar";
import { isHidden, serverModels } from "./catalog";
import { history, nextScript } from "./chat/scenarios";
import { Timeline } from "./chat/timeline";
import { QuestionDock } from "./chat/question";
import { connectDirectly, onConnected, request, startConnection } from "./server";
import { ServerBanner } from "./server-banner";
import { error, setError, showFailure } from "./errors";
import { TerminalPanel } from "./terminal-panel";
import { Selector } from "./selector";
import { ComposerField, type FieldControl } from "./composer-field";
import { hasContent, marksOf, piecesOf, plainText, prepend, type Piece } from "./marks";
import { draftOf, keepDraft, restoreFiles } from "./drafts";
import { DiffView, FilePanel, type FilesTab, type OpenedDiff } from "./file-panel";
import { loadTerminals, openTerminal, terminalsOf } from "./terminals";
import "./chat/chat.css";
import "./styles.css";

type Panel = "model" | "project" | "branch" | "usage" | "effort";

const emptyDemo = (): ReturnType<typeof createDemo> => ({ projects: [], projectChoices: [], sessions: [], recent: [] });
const folderProject = (path: string): Project => ({ path, name: path.split(/[\\/]/).filter(Boolean).at(-1) || path, branch: "" });

function DialogButton(props: { label: string; onClick: () => void; class?: string; children: JSX.Element; disabled?: boolean; variant?: "primary" }) {
  return <Press label={props.label} class={`dialog-button ${props.class || ""}`} onClick={props.onClick} disabled={props.disabled} variant={props.variant}>{props.children}</Press>;
}

function NewProjectDialog(props: { onClose: () => void; onAdd: (project: Project) => void; onError: (message: string) => void }) {
  const [name, setName] = createSignal("");
  const [path, setPath] = createSignal("");
  const [suggestedName, setSuggestedName] = createSignal("");
  const ready = createMemo(() => !!name().trim() && !!path());
  let nameInput!: HTMLInputElement;
  onMount(() => requestAnimationFrame(() => nameInput.focus()));

  async function chooseFolder() {
    try {
      const selected = await open({ directory: true, multiple: false, title: tr().newProject.folder });
      if (!selected) return;
      const folderName = selected.split(/[\\/]/).filter(Boolean).at(-1) || "";
      if (!name().trim() || name() === suggestedName()) setName(folderName);
      setSuggestedName(folderName);
      setPath(selected);
    } catch (cause) {
      props.onError(typeof cause === "string" ? cause : tr().newProject.chooseFailed);
    }
  }

  function add() {
    if (!ready()) return;
    props.onAdd({ name: name().trim(), path: path(), branch: "Main" });
  }

  return <div class="dialog-backdrop" onPointerDown={event => { if (event.target === event.currentTarget) props.onClose(); }}>
    <div class="new-project-dialog" role="dialog" aria-modal="true" aria-labelledby="new-project-title">
      <div class="new-project-card panel-frame">
        <div class="new-project-heading"><h2 id="new-project-title">{tr().newProject.title}</h2><IconButton name="close" label={tr().common.close} size={14} width={20} onClick={props.onClose} /></div>
        <label class="new-project-name"><Glyph name="pencil-line" size={26} />
          <input ref={nameInput} value={name()} placeholder={tr().newProject.name} aria-label={tr().newProject.name} onInput={event => setName(event.currentTarget.value)} onKeyDown={event => { if (event.key === "Enter") add(); }} />
        </label>
        <div class="new-project-divider" />
        <div class="new-project-folder-label">{tr().newProject.folder}</div>
        <DialogButton label={tr().newProject.chooseFolder} class="new-project-folder" onClick={() => void chooseFolder()}>
          <Show when={path()}><Glyph name="folder" size={26} /></Show>
          <span class="truncate">{path() || tr().newProject.clickToAdd}</span>
        </DialogButton>
      </div>
      <Show when={ready()}><DialogButton label={tr().newProject.addLabel} class="new-project-add" variant="primary" onClick={add}>{tr().newProject.add}</DialogButton></Show>
    </div>
  </div>;
}

function RenameProjectDialog(props: { name: string; onClose: () => void; onSave: (name: string) => void }) {
  const [name, setName] = createSignal(props.name);
  const ready = createMemo(() => !!name().trim());
  let nameInput!: HTMLInputElement;
  onMount(() => requestAnimationFrame(() => { nameInput.focus(); nameInput.select(); }));
  const save = () => { if (ready()) props.onSave(name().trim()); };

  return <div class="dialog-backdrop" onPointerDown={event => { if (event.target === event.currentTarget) props.onClose(); }}>
    <div class="rename-project-dialog" role="dialog" aria-modal="true" aria-labelledby="rename-project-title">
      <div class="new-project-card rename-project-card panel-frame">
        <div class="new-project-heading"><h2 id="rename-project-title">{tr().renameProject.title}</h2><IconButton name="close" label={tr().common.close} size={14} width={20} onClick={props.onClose} /></div>
        <label class="new-project-name"><Glyph name="pencil-line" size={26} />
          <input ref={nameInput} value={name()} placeholder={tr().newProject.name} aria-label={tr().newProject.name} onInput={event => setName(event.currentTarget.value)} onKeyDown={event => { if (event.key === "Enter") save(); }} />
        </label>
      </div>
      <DialogButton label={tr().renameProject.save} class={`rename-project-save ${ready() ? "" : "empty"}`} variant="primary" onClick={save}>{tr().renameProject.save}</DialogButton>
    </div>
  </div>;
}

type ChatList = "project" | "recent";
// Both take the chat out of the lists after a confirmation; the key also names its texts
type ChatRemoval = "deleteChat" | "archiveChat";

// The chat's row while it is renamed: Enter or a click elsewhere saves, Escape keeps the old name
function ChatNameField(props: { title: string; onDone: (title?: string) => void }) {
  let input!: HTMLInputElement;
  let done = false;
  const finish = (title?: string) => {
    if (done) return;
    done = true;
    props.onDone(title);
  };
  onMount(() => { input.focus(); input.select(); });
  return <div class="sidebar-row chat-name-row">
    <input ref={input} class="chat-name-field" value={props.title} aria-label={tr().chatMenu.rename} onBlur={() => finish(input.value)}
      onKeyDown={event => {
        if (event.key === "Enter") finish(input.value);
        else if (event.key === "Escape") finish();
        else return;
        event.preventDefault();
        event.stopPropagation();
      }} />
  </div>;
}

function Dot() { return <span class="dot" />; }


function App() {
  applyFonts();
  const [demo, setDemo] = createSignal(demoMode ? createDemo("C:\\src\\NabuCode") : emptyDemo());
  const [addedProjects, setAddedProjects] = createSignal<Project[]>([]);
  // The folder a new chat goes to: the one picked, else the last one used, else the first project, else Home
  const [chosenProject, setProject] = createSignal<Project | null>(demo().projects[0] ?? null);
  const [startFolder, setStartFolder] = createSignal<Project | null>(null);
  const lastProjectKey = "everyend-project";
  const lastProject = localStorage.getItem(lastProjectKey);
  createEffect(() => { if (!demoMode && chosenProject()) localStorage.setItem(lastProjectKey, chosenProject()!.path); });
  const project = () => chosenProject()
    ?? projectChoices().find(item => lastProject !== null && samePath(item.path, lastProject))
    ?? projectChoices()[0] ?? startFolder() ?? folderProject("");
  const availableModels = () => demoMode ? models : serverModels();
  // The server compacts by these; without a chosen model it refuses a custom compaction with a clear error
  const sendCompaction = () => {
    if (demoMode) return;
    const { auto, threshold, custom, model, effort } = preferences.compaction;
    const slash = model.indexOf("/");
    const listed = availableModels().find(item => keyOf(item) === model);
    const variant = listed ? effortFor(listed, effort) : effort || undefined;
    const ref = slash > 0 ? { providerID: model.slice(0, slash), modelID: model.slice(slash + 1), variant } : undefined;
    void request("setCompaction", { auto, threshold: Math.min(99, threshold), customModel: custom, model: ref }).catch(() => undefined);
  };
  onCleanup(onConnected(sendCompaction));
  createEffect(on(() => ({ ...preferences.compaction }), sendCompaction, { defer: true }));
  const [localSessions, setLocalSessions] = createSignal<Session[]>([]);
  const [activeChat, setActiveChat] = createSignal<{ session: Session } | null>(null);
  const session = () => activeChat()?.session ?? null;
  const [agentChat, setAgentChat] = createSignal<string | null>(null);
  const [dockHeight, setDockHeight] = createSignal(0);
  const viewedChat = () => agentChat() ?? session()?.id ?? null;
  // The chat's own title follows renames; the session picked from the list keeps the one it had then
  const viewTitle = () => chats[viewedChat() ?? ""]?.title || session()?.title || "";
  const chatBusy = () => !!session() && !!chats[session()!.id]?.busy;
  const viewedBusy = () => !!viewedChat() && !!chats[viewedChat()!]?.busy;
  const queued = () => session() && !agentChat() ? queues[session()!.id] ?? [] : [];
  // Rows of 28px with 4px between and after them, under a 5px top inset; more than three scroll
  const queueHeight = () => queued().length ? 5 + Math.min(queued().length, 3) * 32 : 0;
  // A subagent runs on its parent's model; the composer shows it read-only
  const shownModel = () => {
    const name = agentChat() && chats[agentChat()!]?.messages.findLast(message => message.model)?.model;
    return availableModels().find(item => item.name === name) ?? model();
  };
  const pendingAsk = () => agentChat() || !session() ? undefined : chats[session()!.id]?.ask;
  const [sidebarOpen, setSidebarOpen] = createSignal(false);
  const [preferredSidebar, setPreferredSidebar] = createSignal(258);
  const [windowSize, setWindowSize] = createSignal({ width: innerWidth, height: innerHeight });
  const [maximized, setMaximized] = createSignal(false);
  const [panel, setPanel] = createSignal<Panel | null>(null);
  const [branches, setBranches] = createSignal<string[]>([]);
  const [branchError, setBranchError] = createSignal("");
  const [newProjectOpen, setNewProjectOpen] = createSignal(false);
  const [renaming, setRenaming] = createSignal<Project | null>(null);
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const [terminalOpen, setTerminalOpen] = createSignal(false);
  const [filesTab, setFilesTab] = createSignal<FilesTab | null>(null);
  const [openedDiff, setOpenedDiff] = createSignal<OpenedDiff | null>(null);
  const [settingsTab, setSettingsTab] = createSignal<SettingsTab>("general");
  const chatId = () => session()?.id ?? null;
  const model = createMemo(() => chosenModel(chatId(), availableModels()) ?? models[0]);
  const effort = () => chosenEffort(chatId(), model());
  const fast = () => chosenFast(chatId(), model());
  const context = () => demoMode ? contextUsage : (session() && contextOf(session()!.id, availableModels())) || { used: 0, limit: model().context };
  const contextParts = createMemo(() => {
    if (demoMode) return () => Promise.resolve(demoContextParts);
    const id = session()?.id;
    return id ? () => request("sessionContext", { sessionID: id }) : undefined;
  });
  const usageBars = () => demoMode ? demoUsage[model().provider.id] ?? [] : usageOf(model().provider.id);
  // The ring shows the quotas too, so they are kept current while a chat is open; opening the panel asks again
  createEffect(() => { if ((session() || panel() === "usage") && !demoMode) watchProvider(model().provider.id); });
  const usageRing = () => Math.max(Math.min(1, context().used / context().limit), ...usageBars().map(barFraction));
  const providerPage = () => entryOf(model().provider.id)?.auth === "custom" ? undefined : () => {
    openSettings();
    setSettingsTab("providers");
    openProviderPage(model().provider.id);
  };
  const [draft, setDraft] = createSignal<Piece[]>([]);
  const [attachments, setAttachments] = createSignal<Attachment[]>([]);
  const [dragging, setDragging] = createSignal(false);
  const [fieldHeight, setFieldHeight] = createSignal(36);
  const [projectsVisible, setProjectsVisible] = createSignal(true);
  const [recentVisible, setRecentVisible] = createSignal(true);
  const [contextMenu, setContextMenu] = createSignal<{ x: number; y: number; project?: Project; chat?: Session; list?: ChatList } | null>(null);
  // The row being renamed: a chat is listed both in its project and in the recent ones
  const [renamingChat, setRenamingChat] = createSignal<{ id: string; list: ChatList } | null>(null);
  const [removingChat, setRemovingChat] = createSignal<{ chat: Session; removal: ChatRemoval } | null>(null);
  const [openProjects, setOpenProjects] = createSignal(new Set<string>(demo().projects.slice(0, 2).map(item => item.path)));
  const [fontReady, setFontReady] = createSignal(false);
  let localSession = 0;
  let mirror!: HTMLDivElement;
  let composerField!: FieldControl;
  let filePicker!: HTMLInputElement;
  let composerScroll: ScrollbarControl | undefined;
  let workspace!: HTMLDivElement;
  let themeKey: { text: string; target: HTMLElement | null; used: boolean } | undefined;
  const appWindow = "__TAURI_INTERNALS__" in window ? getCurrentWindow() : null;
  const limit = createMemo(() => Math.max(244, Math.min(480, windowSize().width - 450)));
  const sidebarWidth = createMemo(() => Math.min(limit(), Math.max(244, preferredSidebar())));
  const visibleSidebar = createMemo(() => sidebarOpen() ? sidebarWidth() : 0);
  const filesShown = () => !!filesTab() && !demoMode && !!activeProject().path;
  // The chat keeps at least 420px between the panels
  const fitFiles = (width: number) => Math.round(Math.max(300, Math.min(width, 640, windowSize().width - visibleSidebar() - 420)));
  const visibleFiles = createMemo(() => filesShown() ? fitFiles(preferences.filesWidth) : 0);
  const workspaceWidth = createMemo(() => windowSize().width - visibleSidebar() - visibleFiles());
  const composerWidth = createMemo(() => Math.min(662, workspaceWidth() - 50));
  const composerLeft = createMemo(() => (workspaceWidth() - composerWidth()) / 2 + (sidebarOpen() && workspaceWidth() >= 712 ? 1 : 0));
  const composerRight = createMemo(() => workspaceWidth() - composerWidth() - composerLeft());
  const projectChoices = createMemo(() => demoMode
    ? [...demo().projectChoices, ...addedProjects().filter(item => !demo().projectChoices.some(project => project.path === item.path))]
    : serverProjects());
  // The listed entry carries the branch the server last reported
  const activeProject = createMemo(() => {
    const chosen = session()?.project ?? project();
    return projectChoices().find(item => samePath(item.path, chosen.path)) ?? chosen;
  });
  // Newest first; subagents' sessions open from their cards, archived ones are not listed
  const listedSessions = createMemo(() => demoMode ? [...demo().sessions, ...localSessions()] : Object.values(serverSessions)
    .filter(item => !item.parentID && !item.time.archived)
    .sort((a, b) => b.time.updated - a.time.updated)
    .map(item => ({
      id: item.id, title: item.title, busy: !!serverBusy[item.id],
      project: projectChoices().find(choice => samePath(choice.path, item.directory)) ?? folderProject(item.directory),
    })));
  const recentSessions = createMemo(() => demoMode ? demo().recent : listedSessions().slice(0, recentCount));
  const projectSessions = createMemo(() => listedSessions().filter(entry => projectChoices().some(item => samePath(item.path, entry.project.path))));
  createEffect(() => { if (!demoMode) watchDirectories(projectChoices().map(item => item.path).filter(Boolean)); });
  const sessionsByProject = createMemo(() => {
    const groups = new Map<string, Session[]>();
    for (const entry of projectSessions()) {
      const entries = groups.get(entry.project.path);
      if (entries) entries.push(entry);
      else groups.set(entry.project.path, [entry]);
    }
    return groups;
  });
  const sidebarProjects = createMemo(() => projectChoices().filter(item => sessionsByProject().has(item.path)));
  const projectDrag = createReorder({ keys: () => sidebarProjects().map(item => item.path), onMove: moveProject });

  function measureText(value: string) {
    const canvas = document.createElement("canvas");
    const context = canvas.getContext("2d")!;
    context.font = "500 15px Geist";
    return Math.ceil(context.measureText(value).width);
  }
  const showBranch = () => !session() && !!activeProject().branch;
  createEffect(() => { document.documentElement.dataset.marks = preferences.markStyle; });
  const terminalShown = () => terminalOpen() && !demoMode && !!activeProject().path;
  // The chat keeps at least 220px under the title bar
  const fitTerminal = (height: number) => Math.round(Math.max(120, Math.min(height, windowSize().height - 35 - 220)));
  const terminalHeight = createMemo(() => fitTerminal(preferences.terminalHeight));
  createEffect(on(() => activeProject().path, path => {
    setOpenedDiff(null);
    if (!path || demoMode) return;
    // A project switched to with the panel open gets a terminal, as opening the panel would give
    void loadTerminals(path).then(() => {
      if (terminalShown() && activeProject().path === path && !terminalsOf(path).length) newTerminal();
    }).catch(() => {});
  }));
  const chipWidth = createMemo(() => { fontReady(); return Math.min(composerWidth(), 10 + measureText(activeProject().name) + 28 + (showBranch() ? 14 + measureText(activeProject().branch) + 27 : 0)); });
  const titleWidth = createMemo(() => { fontReady(); return session() ? Math.min(measureText(viewTitle()), Math.max(0, (windowSize().width - 315 - windowSize().width / 2 - 5) * 2)) : 0; });
  const titlebarStyle = createMemo(() => ({
    "--search-left": `${session() ? windowSize().width / 2 - titleWidth() / 2 - 5 - (21 + measureText(tr().window.search)) : windowSize().width / 2 - 66}px`,
    "--search-width": `${session() ? 21 + measureText(tr().window.search) : 77}px`,
    "--explorer-left": `${windowSize().width / 2 + (session() ? titleWidth() / 2 + 5 : 17)}px`,
    "--title-width": `${titleWidth()}px`,
  }));

  function resizeField() {
    if (!mirror?.isConnected || !workspace) return;
    const measured = mirror.getBoundingClientRect().height;
    setFieldHeight(16 + Math.min(measured, workspace.clientHeight - 140 - queueHeight()));
    requestAnimationFrame(() => composerScroll?.update());
  }
  createEffect(() => { draft(); composerWidth(); windowSize(); settingsOpen(); queueHeight(); requestAnimationFrame(resizeField); });

  function newSession() {
    closeSettings();
    setOpenedDiff(null);
    setAgentChat(null);
    if (!session()) {
      requestAnimationFrame(() => composerField.focus());
      return;
    }
    setProject(session()?.project ?? project());
    setActiveChat(null);
    setPanel(null);
    setError(null);
    requestAnimationFrame(() => composerField.focus());
  }
  function openSession(next: Session) {
    if (!demoMode) openChat(next.id);
    setProject(next.project);
    setAgentChat(null);
    setActiveChat({ session: next });
    setPanel(null);
    setError(null);
  }
  async function addAttachments(pending: Promise<Attachment[]>) {
    const read = await pending;
    if (read.length) setAttachments(list => [...list, ...read]);
  }
  function removeAttachment(id: string) {
    const removed = attachments().find(item => item.id === id);
    if (removed) releaseAttachment(removed);
    setAttachments(list => list.filter(item => item.id !== id));
  }
  function clearAttachments() {
    attachments().forEach(releaseAttachment);
    setAttachments([]);
  }
  const attachFiles = () => void addAttachments(pickFiles(() => filePicker.click()));
  // Each chat keeps what was typed in it, and a new chat one per project; they live through restarts
  const draftKey = createMemo(() => session()?.id ?? `new:${serverPath(project().path).toLowerCase()}`);
  let restoring: string | undefined;
  const keepCurrent = (key: string) => keepDraft(key, draft(), restoring === key ? undefined : attachments());
  createEffect(on(draftKey, (key, previous) => {
    if (previous !== undefined) keepCurrent(previous);
    clearAttachments();
    const saved = draftOf(key);
    restoring = saved?.files.length ? key : undefined;
    setDraft(saved?.pieces ?? []);
    if (!saved?.files.length) return;
    void restoreFiles(saved.files).then(files => {
      if (restoring !== key) return files.forEach(releaseAttachment);
      restoring = undefined;
      setAttachments(list => [...files, ...list]);
    });
  }));
  createEffect(on([draft, attachments], () => keepCurrent(untrack(draftKey)), { defer: true }));
  const attachmentProblem = (file: Attachment) => file.image && !model().images ? tr().attachments.unsupported : undefined;
  const canSend = () => !agentChat() && !!activeProject().path && (hasContent(draft()) || attachments().length > 0) && !attachments().some(attachmentProblem);
  // A command runs on a chat that exists and is quiet; otherwise its mark turns red and nothing is sent
  const commandsWork = () => !!session() && !viewedBusy();
  const [commandRefused, setCommandRefused] = createSignal(false);
  function send() {
    const pieces = draft();
    const text = plainText(pieces).trim();
    const files = attachments();
    // The server queues what is sent while the chat answers; the demo has no queue
    if (!canSend() || (demoMode && chatBusy())) return;
    if (!commandsWork() && marksOf(pieces).some(mark => mark.kind === "command")) return void setCommandRefused(true);
    // Taken before a new session opens: opening one releases the files left in the composer
    setAttachments([]);
    if (!demoMode) return void sendRemote(pieces, files);
    let target = session();
    if (!target) {
      const next = { id: `local-${++localSession}`, project: project(), title: text.split("\n")[0] || files[0].name };
      setLocalSessions(entries => [...entries, next]);
      setOpenProjects(paths => new Set([...paths, next.project.path]));
      setProjectsVisible(true);
      openSession(next);
      target = next;
    }
    ensureChat(target.id, target.title);
    setDraft([]);
    void play(target.id, text, nextScript(), { model: model().name, attachments: files });
  }
  async function sendRemote(pieces: Piece[], files: Attachment[]) {
    // A new chat is sent with what the composer showed before it existed
    const ref = modelRef(chatId(), model());
    let target = session();
    setDraft([]);
    try {
      if (!target) {
        const created = await createSession(project().path);
        target = { id: created.id, title: created.title, project: project() };
        setOpenProjects(paths => new Set([...paths, target!.project.path]));
        setProjectsVisible(true);
        openSession(target);
      }
      const queuedId = editedId();
      setEditedId(undefined);
      await sendToServer(target.id, { pieces, files, model: ref, queuedId });
    } catch (cause) {
      // A failed command has already let the rest of its message go on
      const sent = marksOf(pieces).some(mark => mark.kind === "command");
      if (!sent && !hasContent(draft())) setDraft(pieces);
      if (!sent && !attachments().length) setAttachments(files);
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }
  // Sent again while the chat still answers, an edited message returns to its place in the queue
  const [editedId, setEditedId] = createSignal<string>();
  createEffect(on(() => session()?.id, () => setEditedId(undefined), { defer: true }));
  // The queued message goes back to the composer ahead of whatever is typed there
  async function editQueued(prompt: Prompt) {
    const taken = await unqueue(prompt.sessionID, prompt.messageID);
    if (!taken) return;
    setEditedId(taken.messageID);
    setDraft(typed => prepend(piecesOf(taken.parts), typed));
    const files = await Promise.all(taken.parts.flatMap(part => part.type === "file" ? [restoreAttachment(part).catch(() => null)] : []));
    setAttachments(list => [...files.filter(file => !!file), ...list]);
    composerField?.focus();
  }
  function openAgentChat(id: string) {
    if (!demoMode) openChat(id);
    setAgentChat(id);
  }
  function toggleProject(path: string) {
    const next = new Set(openProjects());
    next.has(path) ? next.delete(path) : next.add(path);
    setOpenProjects(next);
  }
  // Only the projects shown in the sidebar move; the hidden ones keep their places in the full list
  function moveProject(from: number, to: number) {
    if (demoMode) return;
    const shown = sidebarProjects().map(item => item.path);
    const moved = [...shown];
    moved.splice(to, 0, ...moved.splice(from, 1));
    let next = 0;
    reorderProjects(projectChoices().map(item => shown.includes(item.path) ? moved[next++] : item.path)).catch((cause: Error) => setError(cause.message));
  }
  function saveProjectName(path: string, name: string) {
    setRenaming(null);
    if (!demoMode) renameProject(path, name).catch((cause: Error) => setError(cause.message));
  }
  function renameChat(target: Session, title?: string) {
    setRenamingChat(null);
    if (title === undefined || title.trim() === target.title || demoMode) return;
    renameSession(target.id, title).catch((cause: Error) => setError(cause.message));
  }
  function confirmRemoval(chat: Session | undefined, removal: ChatRemoval) {
    setContextMenu(null);
    if (chat) setRemovingChat({ chat, removal });
  }
  function removeChat(target: Session, removal: ChatRemoval) {
    if (session()?.id === target.id) newSession();
    const remove = removal === "deleteChat" ? deleteSession : archiveSession;
    if (!demoMode) remove(target.id).catch((cause: Error) => setError(cause.message));
  }
  function forgetProject(path: string) {
    setContextMenu(null);
    if (demoMode) return;
    if (chosenProject() && samePath(chosenProject()!.path, path)) setProject(null);
    removeProject(path).catch((cause: Error) => setError(cause.message));
  }
  // Settings show the sidebar; leaving them brings back what was there before
  let sidebarBeforeSettings = false;
  function openSettings() {
    setPanel(null);
    if (!settingsOpen()) sidebarBeforeSettings = sidebarOpen();
    setSidebarOpen(true);
    setSettingsOpen(true);
  }
  function closeSettings() {
    if (!settingsOpen()) return;
    setSettingsOpen(false);
    setSidebarOpen(sidebarBeforeSettings);
  }
  function togglePanel(next: Panel) { setPanel(panel() === next ? null : next); }
  // The effort popup is centred over its button, which moves with the model name
  const [effortCenter, setEffortCenter] = createSignal(0);
  function openEffort() {
    const trigger = document.querySelector<HTMLElement>(".effort-trigger")!;
    const box = trigger.getBoundingClientRect();
    setEffortCenter(box.left + box.width / 2 - trigger.closest(".composer-controls")!.parentElement!.getBoundingClientRect().left);
    togglePanel("effort");
  }
  // The list opens once it is loaded: the popup takes its height from the first rows it shows
  async function openBranches() {
    if (panel() === "branch") return closePanel();
    const list = await listBranches(activeProject().path).catch((cause: Error) => void setError(cause.message));
    if (!list) return;
    setBranches(list);
    setBranchError("");
    setPanel("branch");
  }
  // A failed switch leaves the branch as it was and the list open with git's message
  async function chooseBranch(branch: string) {
    if (branch === activeProject().branch) return closePanel();
    try {
      await switchBranch(activeProject().path, branch);
      closePanel();
    } catch (cause) {
      setBranchError(cause instanceof Error ? cause.message : String(cause));
    }
  }
  function openNewProject() { setPanel(null); setNewProjectOpen(true); }
  async function addProject(next: Project) {
    if (!demoMode) {
      const added = await addServerProject(next.path, next.name).then(list => list.find(item => samePath(item.path, next.path))).catch((cause: Error) => void setError(cause.message));
      if (!added) return;
      setProject(added);
    } else {
      const existing = projectChoices().find(item => samePath(item.path, next.path));
      if (!existing) setAddedProjects([...addedProjects(), next]);
      setProject(existing ?? next);
    }
    setActiveChat(null);
    setNewProjectOpen(false);
    setProjectsVisible(true);
  }
  function closePanel(restoreFocus = true) {
    const previous = panel();
    if (!previous) return;
    setPanel(null);
    if (restoreFocus) requestAnimationFrame(() => {
      document.querySelector<HTMLElement>(`[data-panel-trigger="${previous}"]`)?.focus();
    });
  }
  async function openProject(path = activeProject().path) {
    try {
      await invoke("open_project", { path });
      setError(null);
    } catch (cause) {
      setError(tr().errors.openFolder(path));
    }
  }
  function dismissOutside(event: PointerEvent) {
    if (panel() && !(event.target as HTMLElement).closest(".popup, [data-panel-trigger]")) closePanel(false);
  }
  // Opening the panel of a project without terminals starts one
  function toggleFiles(tab: FilesTab) {
    setFilesTab(filesShown() && filesTab() === tab ? null : tab);
  }
  function toggleTerminal() {
    if (terminalShown()) return setTerminalOpen(false);
    setTerminalOpen(true);
    if (!terminalsOf(activeProject().path).length) newTerminal();
  }
  function newTerminal() {
    setTerminalOpen(true);
    void openTerminal(activeProject().path).catch(showFailure);
  }
  function keyDown(event: KeyboardEvent) {
    if (event.code === "KeyT" && !event.repeat && !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && !event.isComposing && !newProjectOpen() && !renaming()) {
      const active = document.activeElement;
      const typed = active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement ? !active.readOnly && !active.disabled : active instanceof HTMLElement && active.isContentEditable;
      const target = typed ? active as HTMLElement : null;
      themeKey = { text: event.key, target, used: false };
      if (target) event.preventDefault();
      return;
    }
    if (themeKey && (event.key === "ArrowRight" || event.key === "ArrowLeft") && !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey) {
      event.preventDefault();
      themeKey.used = true;
      const current = themeIds.indexOf(previewedTheme());
      previewTheme({ theme: themeIds[(current + (event.key === "ArrowRight" ? 1 : themeIds.length - 1)) % themeIds.length] });
      return;
    }
    if (themeKey && event.code === "KeyD" && !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      themeKey.used = true;
      previewTheme({ theme: "oc-2" });
      return;
    }
    if (themeKey && !event.repeat) finishThemeKey();
    if (!demoMode && event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && event.code === "Backquote") { event.preventDefault(); toggleTerminal(); }
    if (!demoMode && event.ctrlKey && event.altKey && !event.metaKey && !event.shiftKey && event.code === "KeyT") { event.preventDefault(); newTerminal(); }
    if (event.code === "KeyU" && event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey && !agentChat()) { event.preventDefault(); attachFiles(); }
    if (event.key === "Escape" && newProjectOpen()) { setNewProjectOpen(false); event.preventDefault(); }
    if (event.key === "Escape" && renaming()) { setRenaming(null); event.preventDefault(); }
    else if (event.key === "Escape" && panel()) { closePanel(); event.preventDefault(); }
    else if (event.key === "Escape" && settingsOpen()) { closeSettings(); event.preventDefault(); }
    if (event.key === "Tab") document.documentElement.classList.add("keyboard-focus");
  }
  function finishThemeKey() {
    const pending = themeKey;
    themeKey = undefined;
    if (!pending || pending.used || !pending.target || document.activeElement !== pending.target) return;
    const input = pending.target;
    if (!(input instanceof HTMLInputElement || input instanceof HTMLTextAreaElement)) return void document.execCommand("insertText", false, pending.text);
    input.setRangeText(pending.text, input.selectionStart ?? input.value.length, input.selectionEnd ?? input.value.length, "end");
    input.dispatchEvent(new InputEvent("input", { bubbles: true, data: pending.text, inputType: "insertText" }));
  }
  function keyUp(event: KeyboardEvent) {
    if (event.code === "KeyT") finishThemeKey();
  }
  function startResize(event: PointerEvent) {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = sidebarWidth();
    const handle = event.currentTarget as HTMLElement;
    handle.setPointerCapture(event.pointerId);
    const move = (next: PointerEvent) => setPreferredSidebar(Math.min(limit(), Math.max(244, startWidth + next.clientX - startX)));
    const stop = () => { handle.removeEventListener("pointermove", move); handle.removeEventListener("pointerup", stop); handle.removeEventListener("pointercancel", stop); };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
  }
  function titlePointerDown(event: PointerEvent) {
    if ((event.target as HTMLElement).closest("button, input")) return;
    event.preventDefault();
    if (event.detail === 2) void appWindow?.toggleMaximize();
    else void appWindow?.startDragging();
  }

  if (demoMode) for (const entry of history) {
    const known = demo().sessions.find(item => item.id === entry.chatId);
    ensureChat(entry.chatId, known?.title ?? entry.text);
    void play(entry.chatId, entry.text, entry.script, { instant: true, model: models[0].name });
  }

  onMount(() => {
    applyTheme();
    const updateSize = () => setWindowSize({ width: innerWidth, height: innerHeight });
    const updateTheme = () => applyTheme();
    const dropKeyboardFocus = () => document.documentElement.classList.remove("keyboard-focus");
    const colorScheme = matchMedia("(prefers-color-scheme: dark)");
    window.addEventListener("resize", updateSize);
    window.addEventListener("keydown", keyDown);
    window.addEventListener("keyup", keyUp);
    window.addEventListener("pointerdown", dismissOutside);
    window.addEventListener("pointerdown", dropKeyboardFocus);
    colorScheme.addEventListener("change", updateTheme);
    const stopDrops = watchFileDrops({ enabled: () => !agentChat(), onDragging: setDragging, onDrop: pending => void addAttachments(pending) });
    let projectReady: Promise<void> = Promise.resolve();
    const direct = directServer();
    if (direct) connectDirectly(direct);
    if (appWindow) {
      void startConnection();
      projectReady = demoMode ? invoke<string>("project_root").then(root => {
        const next = createDemo(root);
        setDemo(next);
        setProject(next.projects[0]);
        setOpenProjects(new Set(next.projects.slice(0, 2).map(item => item.path)));
      }).catch(() => {}) : invoke<{ path: string; name: string }>("start_folder").then(folder => {
        setStartFolder({ path: serverPath(folder.path), name: folder.name, branch: "" });
      }).catch(() => {});
    }
    // A server that doesn't answer shouldn't hold the start screen: its banner says what went wrong
    if (!demoMode) {
      const listed = Promise.race([projectsLoaded, new Promise<void>(resolve => setTimeout(resolve, 4000))]);
      projectReady = Promise.all([projectReady, listed]).then(() => {});
    }
    if (appWindow) {
      void appWindow?.isMaximized().then(setMaximized);
      void appWindow?.onResized(async () => setMaximized(await appWindow.isMaximized()));
    }
    const startup = document.getElementById("startup");
    const finish = async () => {
      if (appWindow) {
        importTheme(await invoke<ThemeChoice | null>("theme_choice").catch(() => null));
      }
      void loadCatalog();
      await Promise.all([projectReady, installFont(preferences.fonts.ui), document.fonts.ready, document.fonts.load("12px Inter"), new Promise<void>(resolve => {
        const watermark = new Image();
        watermark.onload = () => resolve();
        watermark.onerror = () => resolve();
        watermark.src = "/watermark.png";
      }), new Promise(resolve => setTimeout(resolve, 300))]);
      setFontReady(true);
      requestAnimationFrame(() => requestAnimationFrame(() => {
        startup?.classList.add("leaving");
        setTimeout(() => startup?.remove(), 300);
      }));
    };
    void finish();
    onCleanup(() => {
      window.removeEventListener("resize", updateSize);
      window.removeEventListener("keydown", keyDown);
      window.removeEventListener("keyup", keyUp);
      window.removeEventListener("pointerdown", dismissOutside);
      window.removeEventListener("pointerdown", dropKeyboardFocus);
      colorScheme.removeEventListener("change", updateTheme);
      stopDrops();
    });
  });

  const row = (title: string, action: (() => void) | undefined, options?: { icon?: string; tail?: JSX.Element; selected?: () => boolean; large?: boolean; onContextMenu?: JSX.EventHandlerUnion<HTMLButtonElement, MouseEvent> }) =>
    <SidebarRow title={title} onClick={action} icon={options?.icon} tail={options?.tail} selected={options?.selected?.()} large={options?.large} onContextMenu={options?.onContextMenu} />;

  const sessionRow = (item: Session, list: ChatList) => <Show when={renamingChat()?.id === item.id && renamingChat()?.list === list} fallback={
    row(item.title, () => openSession(item), {
      selected: () => session()?.id === item.id,
      tail: <Show when={item.busy || chats[item.id]?.busy}><Ring fraction={.68} size={13} loading /></Show>,
      onContextMenu: event => {
        event.preventDefault();
        setContextMenu({ x: event.clientX, y: event.clientY, chat: item, list });
      },
    })}>
    <ChatNameField title={item.title} onDone={title => renameChat(item, title)} />
  </Show>;
  const section = (title: string, expanded: boolean, toggle: () => void, add = false) =>
    <div class="sidebar-section">
      <Press label={title} class="section-button" onClick={toggle}><span>{title}</span><Glyph name={expanded ? "chevron-down" : "chevron-right"} size={13} /></Press>
      <Show when={add}><IconButton name="plus" label={tr().sidebar.addProject} width={23} onClick={openNewProject} /></Show>
    </div>;

  return <div class="app-shell" style={{ "--sidebar-width": `${visibleSidebar()}px`, "--sidebar-content-width": `${sidebarWidth()}px` }}>
    <Show when={contextMenu()}>{menu => <Show when={menu().project} fallback={
      <ContextMenu x={menu().x} y={menu().y} label={tr().chatMenu.label} onClose={() => setContextMenu(null)}>
        <MenuItem icon="trash" label={tr().chatMenu.delete} onClick={() => confirmRemoval(menu().chat, "deleteChat")} />
        <MenuItem icon="archive" label={tr().chatMenu.archive} onClick={() => confirmRemoval(menu().chat, "archiveChat")} />
        <div class="context-menu-separator" role="separator" />
        <MenuItem icon="pencil-line" label={tr().chatMenu.rename} onClick={() => {
          const { chat, list } = menu();
          setContextMenu(null);
          if (chat && list) setRenamingChat({ id: chat.id, list });
        }} />
      </ContextMenu>}>{target => <ContextMenu x={menu().x} y={menu().y} label={tr().projectMenu.label} onClose={() => setContextMenu(null)}>
        <MenuItem icon="explorer" label={tr().projectMenu.reveal} onClick={() => { setContextMenu(null); void openProject(target().path); }} />
        <MenuItem icon="pencil-line" label={tr().projectMenu.rename} onClick={() => { setContextMenu(null); setRenaming(target()); }} />
        <div class="context-menu-separator" role="separator" />
        <MenuItem icon="trash" label={tr().projectMenu.remove} onClick={() => forgetProject(target().path)} />
      </ContextMenu>}</Show>}</Show>
    <div class="titlebar panel-frame" classList={{ "dialog-open": newProjectOpen() || !!renaming() }} style={titlebarStyle()} onPointerDown={titlePointerDown}>
      <div class="title-left">
        <IconButton name="menu" label={tr().window.menu} size={16} />
        <IconButton name={sidebarOpen() ? "sidebar-open" : "sidebar"} label={tr().window.sidebar} size={16} selected={sidebarOpen()} onClick={() => setSidebarOpen(!sidebarOpen())} />
        <IconButton name="new-session" label={tr().window.newChat} class="title-new-chat" size={16} selected={!session()} onClick={newSession} />
      </div>
      <Press label={tr().window.search} class="title-search" ><Glyph name="magnifying-glass" size={16} /><span>{tr().window.search}</span></Press>
      <Show when={session()}><div class="title-session truncate">{viewTitle()}</div></Show>
      <div class="title-explorer panel-frame">
        <IconButton name="explorer" label={tr().window.openProjectFolder} size={15} width={28} onClick={openProject} />
        <span class="explorer-divider" /><IconButton name="chevron-down" label={tr().window.fileManager} size={13} width={20} />
      </div>
      <div class="title-tools"><IconButton name="terminal" label={tr().window.terminal} selected={terminalShown()} onClick={demoMode ? undefined : toggleTerminal} /><IconButton name="file-tree" label={tr().window.fileTree} selected={filesShown() && filesTab() === "files"} onClick={demoMode ? undefined : () => toggleFiles("files")} /><IconButton name="review" label={tr().window.changes} selected={filesShown() && filesTab() === "changes"} onClick={demoMode ? undefined : () => toggleFiles("changes")} /></div>
      <div class="captions">
        <Press label={tr().window.minimize} class="caption" onClick={() => void appWindow?.minimize()}><Glyph name="window-minimize" size={12} /></Press>
        <Press label={maximized() ? tr().window.restore : tr().window.maximize} class="caption" onClick={() => void appWindow?.toggleMaximize()}><Glyph name={maximized() ? "window-restore" : "window-maximize"} size={12} /></Press>
        <Press label={tr().common.close} class="caption close-caption" onClick={() => void appWindow?.close()}><Glyph name="window-close" size={12} /></Press>
      </div>
    </div>

    <aside class="sidebar" classList={{ collapsed: !sidebarOpen() }}>
      <Show when={!settingsOpen()} fallback={<div class="sidebar-content"><SettingsNav tab={settingsTab()} onTab={setSettingsTab} onBack={closeSettings} /></div>}>
      <div class="sidebar-content">
        {row(tr().sidebar.newChat, newSession, { icon: "plus", large: true, selected: () => !session() })}
        <div class="sidebar-primary-gap" />
        {row(tr().sidebar.settings, openSettings, { icon: "settings-gear", large: true })}
        <div class="sidebar-primary-gap" />
        <div class="sidebar-scroll">
          {section(tr().sidebar.projects, projectsVisible(), () => setProjectsVisible(!projectsVisible()), true)}
          <Show when={projectsVisible()}>
            <div class="sidebar-section-gap" />
            <For each={sidebarProjects()}>{(item, index) => {
              const entries = () => sessionsByProject().get(item.path) ?? [];
              // The row is the handle: a press on the project's sessions doesn't drag the project
              const grab = (event: PointerEvent & { currentTarget: HTMLDivElement }) => {
                const handle = event.currentTarget.firstElementChild as HTMLElement;
                if (handle.contains(event.target as Node)) projectDrag.press(item.path, event, handle);
              };
              return <div ref={projectDrag.ref(item.path)} class="sidebar-project" classList={{ last: index() === sidebarProjects().length - 1, "drag-slot": projectDrag.isDragged(item.path) }}
                style={projectDrag.style(item.path)} onPointerDown={grab}>
                {row(item.name, () => toggleProject(item.path), { icon: "folder", tail: <span class="project-tail">
                  <Show when={entries().some(value => value.busy)}><Ring fraction={.35} size={11} loading /></Show>
                  <Glyph name={openProjects().has(item.path) ? "chevron-down" : "chevron-right"} size={15} />
                </span>, onContextMenu: event => {
                  event.preventDefault();
                  setContextMenu({ x: event.clientX, y: event.clientY, project: item });
                } })}
                <Show when={openProjects().has(item.path) && !projectDrag.isDragged(item.path)}><div class="project-sessions"><For each={entries()}>{entry => sessionRow(entry, "project")}</For></div></Show>
              </div>;
            }}</For>
          </Show>
          <div class="recent-gap" />
          {section(tr().sidebar.recent, recentVisible(), () => setRecentVisible(!recentVisible()))}
          <Show when={recentVisible()}><div class="sidebar-section-gap" /><div class="recent-sessions"><For each={recentSessions()}>{entry => sessionRow(entry, "recent")}</For></div></Show>
        </div>
      </div>
      </Show>
    </aside>
    <Show when={sidebarOpen()}><div class="sidebar-resize" style={{ left: `${sidebarWidth() - 3}px` }} onPointerDown={startResize} /></Show>

    <main ref={workspace} class="workspace panel-frame" classList={{ "sidebar-open": sidebarOpen(), "files-open": filesShown(), "terminal-open": terminalShown() }}
      style={{ left: `${visibleSidebar()}px`, right: `${visibleFiles()}px`, bottom: terminalShown() ? `${terminalHeight()}px` : undefined, "--composer-left": `${composerLeft()}px`, "--composer-right": `${composerRight()}px`, "--composer-width": `${composerWidth()}px`, "--field-height": `${fieldHeight() + queueHeight()}px`, "--queue-height": `${queueHeight()}px`, "--chip-width": `${chipWidth()}px`, "--dock-height": `${dockHeight()}px`, "--tray-height": attachments().length && !agentChat() ? "122px" : "0px" }}>
      <Show when={!settingsOpen()} fallback={<SettingsPanel tab={settingsTab()} />}>
      <Show when={!session()}><div class="watermark"><img src="/watermark.png" width="325" height="77" alt="" /><span>Every End</span></div></Show>
      <Show when={viewedChat()} keyed>{id => <Timeline chatId={id} onOpen={openAgentChat} onBack={() => setAgentChat(null)} />}</Show>
      <Show when={viewedChat()}><div class="chat-fade" /></Show>
      <Show when={pendingAsk()}>{ask => <div class="question-anchor"><QuestionDock chatId={session()!.id} ask={ask()} onHeight={setDockHeight} /></div>}</Show>
      <div class="composer-area" classList={{ hidden: !!pendingAsk(), agent: !!agentChat() }}>
      <div class="footer-back panel-frame" />
      <Show when={!agentChat()}>
      <div class="project-chip panel-frame" data-panel-trigger="project">
        <Press label={tr().composer.project(activeProject().name)} class="project-chip-name" selected={panel() === "project"} onClick={session() ? undefined : () => togglePanel("project")}>
          <Glyph name="folder" /><span class="truncate">{activeProject().name}</span>
        </Press>
        <Show when={showBranch()}><Dot /><Press label={tr().composer.branch(activeProject().branch)} class="project-chip-branch" panelTrigger="branch" selected={panel() === "branch"} onClick={demoMode ? undefined : () => void openBranches()}><Glyph name="branch" size={17} /><span class="truncate">{activeProject().branch}</span></Press></Show>
      </div>
      </Show>
      <Show when={attachments().length && !agentChat()}><AttachmentTray files={attachments()} onRemove={removeAttachment}
        problem={attachmentProblem} /></Show>
      <div class="composer panel-frame">
        <Show when={queued().length}><div class="queue-rows"><For each={queued()}>{prompt =>
          <div class="queue-row">
            <Glyph name="queued" size={14} />
            <span class="queue-text">{[plainText(piecesOf(prompt.parts)), ...prompt.parts.map(part => part.type === "file" ? part.filename ?? "" : "")].filter(Boolean).join(" · ")}</span>
            <Press label={tr().chat.interruptQueued} class="queue-button" onClick={() => void interruptSession(prompt.sessionID, prompt.messageID)}><Glyph name="skip-forward" size={15} /></Press>
            <Press label={tr().chat.editQueued} class="queue-button" onClick={() => void editQueued(prompt)}><Glyph name="pencil-line" size={15} /></Press>
            <Press label={tr().chat.removeQueued} class="queue-button" onClick={() => void unqueue(prompt.sessionID, prompt.messageID)}><Glyph name="close" size={15} /></Press>
          </div>}</For></div></Show>
        <Show when={!agentChat()}><IconButton name="plus" label={tr().composer.attach} width={20} onClick={attachFiles} /></Show>
        <input ref={filePicker} type="file" multiple hidden onChange={event => { void addAttachments(Promise.all([...event.currentTarget.files ?? []].map(readAttachment))); event.currentTarget.value = ""; }} />
        <div class="composer-editor">
          <ComposerField value={agentChat() ? [] : draft()} disabled={!!agentChat()} label={tr().composer.message}
            placeholder={agentChat() ? tr().chat.agentLocked : tr().composer.placeholder} directory={activeProject().path}
            refused={commandRefused() && !commandsWork()} control={control => composerField = control} onScroll={control => composerScroll = control}
            onChange={pieces => { setDraft(pieces); setCommandRefused(false); requestAnimationFrame(() => composerScroll?.show()); }}
            onEnter={send} onPasteFiles={data => void addAttachments(readPasted(data))} />
        </div>
        <Show when={viewedBusy() && (demoMode || !canSend())} fallback={<Press label={viewedBusy() ? tr().chat.enqueue : session() ? tr().chat.send : tr().composer.send} class="send-button" onClick={canSend() ? send : undefined}
          style={{ background: canSend() ? "var(--primary)" : "var(--outline)" }}>
          <Glyph name="arrow-up" size={16} color={canSend() ? "var(--on-primary)" : "var(--muted)"} />
        </Press>}>
          <Press label={tr().chat.stop} class="send-button" onClick={() => stopChat(viewedChat()!)} style={{ background: "var(--primary)" }}>
            <Glyph name="stop" size={16} color="var(--on-primary)" />
          </Press>
        </Show>
      </div>
      <div ref={mirror} class="composer-mirror" aria-hidden="true">{plainText(draft())}{"\u200b"}</div>
      <Show when={panel() === "model"}><div class="popup-anchor model-anchor"><Selector items={availableModels().filter(item => !isHidden(item))} selected={model()} name={item => item.name} details={item => item.provider.name}
        group={item => item.provider} kind="model" onSelect={item => { pickModel(chatId(), item); closePanel(); }}
        onAction={() => { openSettings(); setSettingsTab("models"); }} /></div></Show>
      <Show when={panel() === "project"}><div class="popup-anchor project-anchor"><Selector items={projectChoices()} selected={activeProject()}
        name={item => item.name} details={item => item.path} icon="folder" kind="project"
        onAction={openNewProject} onSelect={item => { if (!session()) setProject(item); closePanel(); }} /></div></Show>
      <Show when={panel() === "branch"}><div class="popup-anchor project-anchor"><Selector items={branches()} selected={activeProject().branch}
        name={item => item} details={() => ""} icon="branch" kind="branch" error={branchError()} onSelect={item => void chooseBranch(item)} /></div></Show>
      <Show when={panel() === "effort" && effort()}>{level =>
        <div class="popup-anchor effort-anchor" style={{ left: `${effortCenter()}px` }}>
          <EffortPicker levels={model().efforts} value={level()} onPick={next => pickEffort(chatId(), model(), next)} onClose={closePanel} /></div>}</Show>
      <Show when={panel() === "usage"}><div class="popup-anchor usage-anchor"><Usage bars={usageBars()} context={context()} onOpen={providerPage()} loadParts={contextParts()} /></div></Show>
      <div class="composer-controls">
        <div class="controls-left">
          <Press label={tr().composer.model(shownModel().name)} class="model-trigger" panelTrigger="model" selected={panel() === "model"} onClick={agentChat() ? undefined : () => togglePanel("model")}><Glyph name={shownModel().provider.icon} size={17} /><span class="truncate">{shownModel().name}</span></Press>
          <Show when={effort()}>{level => <>
            <Dot />
            <Press label={tr().composer.effort(effortName(level()))} class="effort-trigger" panelTrigger="effort" selected={panel() === "effort"}
              onClick={agentChat() ? undefined : openEffort}><span>{effortName(level())}</span></Press>
          </>}</Show>
          <Show when={model().fast}>
            <Dot />
            <Press label="Fast" class="fast-trigger" selected={fast()} onClick={agentChat() ? undefined : () => pickFast(chatId(), model(), !fast())}>
              <Glyph name={fast() ? "bolt-filled" : "bolt"} size={16} /></Press>
          </Show>
        </div>
        <Show when={session()}><Press label={tr().composer.usage} class="usage-trigger" panelTrigger="usage" selected={panel() === "usage"} onClick={() => togglePanel("usage")}>
          <Ring fraction={usageRing()} /></Press></Show>
      </div>
      </div>
      </Show>
      <Show when={dragging()}><DropOverlay /></Show>
      <Show when={openedDiff()}>{diff => <DiffView diff={diff()} directory={activeProject().path} onClose={() => setOpenedDiff(null)} />}</Show>
    </main>
    <Show when={filesShown()}><FilePanel directory={activeProject().path} tab={filesTab()!} onTab={setFilesTab} width={visibleFiles()}
      bottom={terminalShown() ? terminalHeight() : 0} onWidth={width => setPreference("filesWidth", fitFiles(width))}
      messages={session() ? chats[session()!.id]?.messages : undefined} onExpand={setOpenedDiff} /></Show>
    <Show when={terminalShown()}><TerminalPanel directory={activeProject().path} left={visibleSidebar()} height={terminalHeight()}
      onHeight={height => setPreference("terminalHeight", fitTerminal(height))} onHide={() => setTerminalOpen(false)} /></Show>
    <Show when={newProjectOpen()}><NewProjectDialog onClose={() => setNewProjectOpen(false)} onAdd={project => void addProject(project)} onError={setError} /></Show>
    <Show when={removingChat()}>{target => {
      const text = () => tr()[target().removal];
      return <ConfirmDialog title={text().title} description={text().description(target().chat.title)} action={text().action}
        onConfirm={() => removeChat(target().chat, target().removal)} onClose={() => setRemovingChat(null)} />;
    }}</Show>
    <Show when={renaming()}>{target => <RenameProjectDialog name={target().name} onClose={() => setRenaming(null)} onSave={name => saveProjectName(target().path, name)} />}</Show>
    <ServerBanner />
    <Show when={error()}><div class="error-toast panel-frame"><span>{error()}</span><IconButton name="close" label={tr().common.close} size={16} onClick={() => setError(null)} /></div></Show>
  </div>;
}

render(() => <App />, document.getElementById("root")!);
