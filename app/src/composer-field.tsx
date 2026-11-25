import { createEffect, createMemo, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { preferences } from "./preferences";
import { knownChoices, loadChoices, markText, marksOf, maxMarks, type Choice, type Mark, type Piece } from "./marks";
import { Scrollbar, type ScrollbarControl } from "./scrollbar";

export type FieldControl = { focus: () => void; element: () => HTMLDivElement };

/** Where the caret types a mark: the sign and what follows it, started at the field's start or after a space. */
type Trigger = { node: Text; start: number; end: number; sign: Mark["sign"]; query: string };

const sameSpot = (a: Pick<Trigger, "node" | "start"> | undefined, b: Pick<Trigger, "node" | "start"> | undefined) => !!a && !!b && a.node === b.node && a.start === b.start;
const escape = (text: string) => text.replace(/[&<>"]/g, char => `&#${char.charCodeAt(0)};`);

/** A mark as the field holds it: one piece the caret and Backspace pass whole. */
function markNode(mark: Mark) {
  const node = document.createElement("span");
  node.className = "mark";
  node.contentEditable = "false";
  node.dataset.kind = mark.kind;
  node.dataset.name = mark.name;
  node.dataset.sign = mark.sign;
  node.textContent = markText(mark);
  return node;
}

function readMark(node: HTMLElement): Mark {
  return { kind: node.dataset.kind === "command" ? "command" : "skill", name: node.dataset.name ?? "", sign: node.dataset.sign === "@" ? "@" : "/" };
}

/** The field's text and marks; a line break is "\n" whether the editor made it a character, a <br> or a block. */
function readPieces(root: HTMLElement): Piece[] {
  const pieces: Piece[] = [];
  const add = (text: string) => {
    if (!text) return;
    const last = pieces.at(-1);
    if (typeof last === "string") pieces[pieces.length - 1] = last + text;
    else pieces.push(text);
  };
  const walk = (parent: Node) => {
    parent.childNodes.forEach((node, index) => {
      if (node instanceof Text) return add(node.data.replace(/ /g, " ").replace(/​/g, ""));
      if (!(node instanceof HTMLElement)) return;
      if (node.classList.contains("mark")) return void pieces.push(readMark(node));
      // The editor keeps a last <br> so an ending line break has a line to show
      if (node.tagName === "BR") return add(index === parent.childNodes.length - 1 && parent === root ? "" : "\n");
      if (node.tagName === "DIV" || node.tagName === "P") {
        if (pieces.length) add("\n");
        return walk(node);
      }
      walk(node);
    });
  };
  walk(root);
  return pieces;
}

function writePieces(root: HTMLElement, pieces: Piece[]) {
  root.replaceChildren(...pieces.map(piece => typeof piece === "string" ? document.createTextNode(piece) : markNode(piece)));
  if (plainEndsWithBreak(pieces)) root.append(document.createElement("br"));
}

const plainEndsWithBreak = (pieces: Piece[]) => { const last = pieces.at(-1); return typeof last === "string" && last.endsWith("\n"); };

function caretToEnd(root: HTMLElement) {
  const range = document.createRange();
  range.selectNodeContents(root);
  range.collapse(false);
  const selection = getSelection();
  selection?.removeAllRanges();
  selection?.addRange(range);
}

function findTrigger(root: HTMLElement): Trigger | undefined {
  const selection = getSelection();
  if (!selection?.isCollapsed || !(selection.anchorNode instanceof Text) || !root.contains(selection.anchorNode)) return;
  const node = selection.anchorNode;
  const end = selection.anchorOffset;
  let start = end;
  while (start > 0 && !/\s/.test(node.data[start - 1])) start--;
  const sign = node.data[start];
  if (sign !== "/" && sign !== "@") return;
  // Right after a mark or other text there is no space, so it is a path or an address rather than a mark
  if (start === 0) {
    const before = node.previousSibling;
    if (before && !(before instanceof HTMLBRElement) && !(before instanceof Text && /\s$/.test(before.data))) return;
    if (!before && node.parentNode !== root && node.parentNode?.previousSibling) return;
  }
  return { node, start, end, sign, query: node.data.slice(start + 1, end) };
}

export function ComposerField(props: {
  value: Piece[]; onChange: (pieces: Piece[]) => void; onEnter: () => void; onPasteFiles: (data: DataTransfer) => void;
  directory: string; refused: boolean; disabled: boolean; placeholder: string; label: string;
  control: (control: FieldControl) => void; onScroll?: (control: ScrollbarControl) => void;
}) {
  let field!: HTMLDivElement;
  let list: HTMLDivElement | undefined;
  // The pieces last sent out, so the value coming back doesn't rewrite the field under the caret
  let emitted = "";
  const [trigger, setTrigger] = createSignal<Trigger>();
  const [opened, setOpened] = createSignal<Pick<Trigger, "node" | "start">>();
  const [dismissed, setDismissed] = createSignal<Pick<Trigger, "node" | "start">>();
  const [choices, setChoices] = createSignal<Choice[]>([]);
  const [active, setActive] = createSignal(0);
  const [hovered, setHovered] = createSignal<{ mark: Mark; box: DOMRect }>();
  const [empty, setEmpty] = createSignal(true);

  const marked = createMemo(() => { trigger(); return marksOf(readPieces(field ?? document.createElement("div"))); });
  const items = createMemo(() => {
    const current = trigger();
    if (!current) return [];
    const query = current.query.toLowerCase();
    const taken = marked();
    const fits = choices().filter(choice =>
      (choice.kind === "skill" || current.sign === "/")
      && choice.name.toLowerCase().includes(query)
      && !taken.some(mark => mark.kind === choice.kind && mark.name === choice.name));
    // Names that start with the query first, then the ones that only hold it; commands stay ahead of skills
    const rank = (choice: Choice) => (choice.kind === "command" ? 0 : 2) + (choice.name.toLowerCase().startsWith(query) ? 0 : 1);
    return fits.map((choice, index) => ({ choice, index })).sort((a, b) => rank(a.choice) - rank(b.choice) || a.index - b.index).map(item => item.choice);
  });
  const open = createMemo(() => {
    const current = trigger();
    if (!current || props.disabled || sameSpot(current, dismissed()) || marked().length >= maxMarks) return false;
    return (!!current.query || sameSpot(current, opened())) && items().length > 0;
  });
  createEffect(on(() => trigger()?.query, () => setActive(0)));
  createEffect(on(open, shown => { if (shown) void refreshChoices(); }));
  createEffect(on(() => [active(), open()] as const, () => list?.children[active()]?.scrollIntoView({ block: "nearest" })));

  // Opening the list only asks again: showing the cached choices first could close and reopen it in a loop
  async function refreshChoices() {
    const fresh = await loadChoices(props.directory).catch(() => undefined);
    if (fresh) setChoices(fresh);
  }
  createEffect(on(() => props.directory, directory => {
    setChoices(knownChoices(directory));
    void refreshChoices();
  }));

  function emit() {
    const pieces = readPieces(field);
    emitted = JSON.stringify(pieces);
    setEmpty(!pieces.length);
    props.onChange(pieces);
  }
  createEffect(on(() => props.value, value => {
    if (JSON.stringify(value) === emitted) return;
    emitted = JSON.stringify(value);
    writePieces(field, value);
    setEmpty(!value.length);
    if (document.activeElement === field) caretToEnd(field);
    updateTrigger();
  }));

  function updateTrigger() {
    const current = document.activeElement === field ? findTrigger(field) : undefined;
    // Erased back to the bare sign, the mark may be typed anew
    if (current && !current.query && sameSpot(current, dismissed())) setDismissed(undefined);
    if (!sameSpot(current, trigger())) setOpened(undefined);
    setTrigger(current);
  }
  const selectionChanged = () => updateTrigger();
  onMount(() => document.addEventListener("selectionchange", selectionChanged));
  onCleanup(() => document.removeEventListener("selectionchange", selectionChanged));

  function choose(choice: Choice) {
    const current = trigger();
    if (!current) return;
    const range = document.createRange();
    range.setStart(current.node, current.start);
    range.setEnd(current.node, current.end);
    const selection = getSelection()!;
    selection.removeAllRanges();
    selection.addRange(range);
    const mark: Mark = { kind: choice.kind, name: choice.name, sign: current.sign };
    // Through the editor's own command, so Ctrl+Z takes the mark back
    const html = `<span class="mark" contenteditable="false" data-kind="${mark.kind}" data-name="${escape(mark.name)}" data-sign="${mark.sign}">${escape(markText(mark))}</span> `;
    document.execCommand("insertHTML", false, html);
    emit();
    updateTrigger();
  }

  function keyDown(event: KeyboardEvent) {
    if (event.isComposing) return;
    const plain = !event.ctrlKey && !event.altKey && !event.metaKey;
    const ctrl = event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey;
    const step = plain && event.key === "ArrowDown" || ctrl && event.code === "KeyS" ? 1 : plain && event.key === "ArrowUp" || ctrl && event.code === "KeyW" ? -1 : 0;
    if (open() && step) {
      const count = items().length;
      event.preventDefault();
      return setActive(index => (index + step + count) % count);
    }
    if (open() && plain) {
      if (!event.shiftKey && (event.key === "Enter" || event.key === "Tab" || event.key === "ArrowRight")) {
        event.preventDefault();
        return choose(items()[active()]);
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        return setDismissed(trigger());
      }
    }
    // Tab or a list step after the bare sign opens the full list
    if ((step || plain && !event.shiftKey && event.key === "Tab") && trigger() && !trigger()!.query) {
      event.preventDefault();
      setDismissed(undefined);
      return setOpened(trigger());
    }
    if (event.key !== "Enter" || event.ctrlKey || event.altKey || event.metaKey) return;
    event.preventDefault();
    if (event.shiftKey) document.execCommand("insertLineBreak");
    else props.onEnter();
  }

  function paste(event: ClipboardEvent) {
    event.preventDefault();
    const data = event.clipboardData;
    if (!data) return;
    // A file or folder copied in Explorer may reach the page without a trace, so only plain text is typed in
    const types = data.types;
    if (types.includes("text/plain") && !types.includes("Files")) document.execCommand("insertText", false, data.getData("text/plain"));
    else props.onPasteFiles(data);
  }

  // The classic look shows what a mark does while the pointer is over it
  function pointerOver(event: PointerEvent) {
    const node = (event.target as HTMLElement).closest<HTMLElement>(".mark");
    setHovered(node && preferences.markStyle === "classic" ? { mark: readMark(node), box: node.getBoundingClientRect() } : undefined);
  }
  const description = (mark: Mark) => choices().find(choice => choice.kind === mark.kind && choice.name === mark.name)?.description;

  // The list stands over the sign; the description of the active row beside it
  const anchor = createMemo(() => {
    const current = trigger();
    if (!open() || !current) return;
    const range = document.createRange();
    range.setStart(current.node, current.start);
    range.setEnd(current.node, current.start + 1);
    const sign = range.getBoundingClientRect();
    const frame = field.closest(".composer")!.getBoundingClientRect();
    return { left: sign.left - 9, bottom: innerHeight - frame.top + 6 };
  });

  props.control({ focus: () => { field.focus(); caretToEnd(field); }, element: () => field });

  return <>
    <div ref={field} class="composer-field" contentEditable={!props.disabled} role="textbox" aria-multiline="true" aria-label={props.label}
      classList={{ empty: empty(), refused: props.refused }} data-placeholder={props.placeholder} spellcheck={false}
      onInput={() => { emit(); updateTrigger(); }} onKeyDown={keyDown} onPaste={paste} onBlur={() => setTrigger(undefined)}
      onPointerOver={pointerOver} onPointerLeave={() => setHovered(undefined)} />
    <Scrollbar target={() => field} class="composer-scroll" control={control => props.onScroll?.(control)} />
    <Show when={anchor()}>{place => <MarkList items={items()} active={active()} left={place().left} bottom={place().bottom}
      listRef={element => list = element} onActive={setActive} onChoose={choose} />}</Show>
    <Show when={hovered()}>{hover => <Show when={description(hover().mark)}>{text =>
      <div class="mark-card panel-frame" style={{ left: `${hover().box.left - 8}px`, bottom: `${innerHeight - hover().box.top + 6}px` }}>
        <b>{markText(hover().mark)}</b><span>{text()}</span>
      </div>}</Show>}</Show>
  </>;
}

function MarkList(props: {
  items: Choice[]; active: number; left: number; bottom: number;
  listRef: (element: HTMLDivElement) => void; onActive: (index: number) => void; onChoose: (choice: Choice) => void;
}) {
  let list!: HTMLDivElement;
  let panel!: HTMLDivElement;
  let description: HTMLDivElement | undefined;
  const [row, setRow] = createSignal({ top: 0, right: true, shown: true });
  let pointer = { x: 0, y: 0 };
  let wheelAt = 0;
  // The description goes beside the active row, on the side the window has room on, and rises so it never covers the field
  function place() {
    const element = list.children[props.active] as HTMLElement | undefined;
    if (!element) return;
    const rowTop = element.offsetTop - list.scrollTop;
    const top = Math.min(rowTop, panel.clientHeight - (description?.offsetHeight ?? 0));
    const shown = rowTop + element.offsetHeight > 0 && rowTop < list.clientHeight;
    setRow({ top, right: panel.getBoundingClientRect().right + 306 <= innerWidth, shown });
  }
  // Wheel scrolling moves the choice to the row under the cursor, as hover would
  function scrolled() {
    if (performance.now() - wheelAt < 400) {
      const element = document.elementFromPoint(pointer.x, pointer.y)?.closest(".mark-row");
      const index = element ? Array.prototype.indexOf.call(list.children, element) : -1;
      if (index >= 0 && index !== props.active) props.onActive(index);
    }
    place();
  }
  createEffect(on(() => [props.active, props.items], () => requestAnimationFrame(place)));
  return <div ref={panel} class="mark-list panel-frame" style={{ left: `${Math.max(8, props.left)}px`, bottom: `${props.bottom}px` }}
    onPointerDown={event => event.preventDefault()}>
    <div ref={element => { list = element; props.listRef(element); }} class="mark-rows" onScroll={scrolled}
      onWheel={event => { pointer = { x: event.clientX, y: event.clientY }; wheelAt = performance.now(); }}>
      <For each={props.items}>{(item, index) =>
        <div class="mark-row" classList={{ active: index() === props.active }}
          onPointerMove={() => { if (index() !== props.active) props.onActive(index()); }} onClick={() => props.onChoose(item)}>
          <span class="truncate">{item.name}</span>
        </div>}</For>
    </div>
    <Scrollbar target={() => list} class="mark-scroll" />
    <Show when={row().shown && props.items[props.active]?.description}>{text =>
      <div ref={description} class="mark-description panel-frame" classList={{ left: !row().right }} style={{ top: `${row().top}px` }}>{text()}</div>}</Show>
  </div>;
}
