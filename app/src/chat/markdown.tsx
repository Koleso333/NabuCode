import { createMemo, createRenderEffect, createSignal, For } from "solid-js";
import { Marked, type Token, type Tokens } from "marked";
import { escapeHtml, highlight, isLoaded, languageOf } from "./highlight";
import { copyText } from "./copy";


// Set while the unfinished block renders: it changes on every chunk, so it skips highlighting
let plainCode = false;

const marked = new Marked({
  gfm: true,
  renderer: {
    // Model output is shown as text, never as markup
    html: token => escapeHtml(token.text),
    code: token => {
      const lang = languageOf(token.lang);
      const html = !plainCode && lang && highlight(token.text, lang) || `<pre><code>${escapeHtml(token.text)}</code></pre>`;
      return `<div class="md-code"><button type="button" class="md-copy" aria-label="copy"></button>${html}</div>`;
    },
    link(token) {
      return `<a href="${escapeHtml(token.href)}" title="${escapeHtml(token.title ?? token.href)}">${this.parser.parseInline(token.tokens)}</a>`;
    },
  },
});

type Block = { raw: string; tokens: Token[]; langs: string[] };

const cache = new Map<string, string>();
const cacheLimit = 600;

function remember(key: string, html: string) {
  cache.delete(key);
  cache.set(key, html);
  if (cache.size > cacheLimit) cache.delete(cache.keys().next().value!);
  return html;
}

function codeLanguages(tokens: Token[]) {
  return tokens.flatMap(token => token.type === "code" ? [languageOf((token as Tokens.Code).lang)].filter((lang): lang is string => !!lang) : []);
}

function renderBlock(block: Block) {
  const highlighted = block.langs.length > 0 && block.langs.every(isLoaded);
  const key = highlighted ? `h:${block.raw}` : block.raw;
  const cached = cache.get(key);
  if (cached !== undefined) return remember(key, cached);
  return remember(key, marked.parser(block.tokens));
}

// Index of the first token that may still change when more text arrives
function liveFrom(tokens: Token[]) {
  let last = tokens.length - 1;
  while (last >= 0 && tokens[last].type === "space") last--;
  if (last <= 0) return 0;
  let previous = last - 1;
  while (previous >= 0 && tokens[previous].type === "space") previous--;
  // Two lists in a row may still merge into one
  if (previous >= 0 && tokens[previous].type === "list" && tokens[last].type === "list") return previous;
  return last;
}

// Finished blocks render once; each new chunk only re-renders the unfinished last block
export function Markdown(props: { text: string; live?: boolean; class?: string }) {
  const [blocks, setBlocks] = createSignal<Block[]>([], { equals: false });
  const [tail, setTail] = createSignal("");
  let frozen: Block[] = [];
  let offset = 0;

  createRenderEffect(() => {
    const text = props.text;
    const live = !!props.live;
    if (text.length < offset) {
      frozen = [];
      offset = 0;
    }
    const tokens = marked.lexer(text.slice(offset));
    const cut = live ? liveFrom(tokens) : tokens.length;
    if (cut > 0) {
      const done = tokens.slice(0, cut);
      offset += done.reduce((sum, token) => sum + token.raw.length, 0);
      for (const token of done) {
        if (token.type !== "space") frozen.push({ raw: token.raw, tokens: [token], langs: codeLanguages([token]) });
      }
      setBlocks([...frozen]);
    }
    plainCode = true;
    setTail(cut < tokens.length ? marked.parser(tokens.slice(cut)) : "");
    plainCode = false;
  });

  function copyCode(event: MouseEvent) {
    const button = (event.target as HTMLElement).closest(".md-copy");
    const code = button?.parentElement?.querySelector("pre");
    if (!button || !code) {
      if ((event.target as HTMLElement).closest("a")) event.preventDefault();
      return;
    }
    void copyText(code.textContent ?? "").then(() => {
      button.classList.add("copied");
      setTimeout(() => button.classList.remove("copied"), 2000);
    });
  }

  return <div class={`markdown ${props.class ?? ""}`} onClick={copyCode}>
    <For each={blocks()}>{block => {
      const html = createMemo(() => renderBlock(block));
      return <div class="md-block" innerHTML={html()} />;
    }}</For>
    <div class="md-block" innerHTML={tail()} />
  </div>;
}
