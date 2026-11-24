import { createSignal, For, Index, onCleanup, onMount, Show } from "solid-js";
import { createStore } from "solid-js/store";
import { Glyph, Press } from "../controls";
import { tr } from "../i18n";
import { answerAsk, dismissAsk } from "./actions";
import type { PendingAsk } from "./model";

// Takes the composer's place while the model waits for answers
export function QuestionDock(props: { chatId: string; ask: PendingAsk; onHeight: (height: number) => void }) {
  const [index, setIndex] = createSignal(0);
  const [picked, setPicked] = createStore<string[][]>(props.ask.questions.map(() => []));
  const [custom, setCustom] = createStore<string[]>(props.ask.questions.map(() => ""));
  const question = () => props.ask.questions[index()];
  const last = () => index() === props.ask.questions.length - 1;
  const answerOf = (at: number) => [...picked[at], ...(custom[at].trim() ? [custom[at].trim()] : [])];

  function pick(label: string) {
    const at = index();
    if (question().multiple) {
      setPicked(at, list => list.includes(label) ? list.filter(item => item !== label) : [...list, label]);
      return;
    }
    setPicked(at, [label]);
    setCustom(at, "");
  }

  function type(value: string) {
    const at = index();
    setCustom(at, value);
    if (!question().multiple && value.trim()) setPicked(at, []);
  }

  function next() {
    if (!last()) return setIndex(index() + 1);
    answerAsk(props.chatId, props.ask.questions.map((_, at) => answerOf(at)));
  }

  let dock!: HTMLDivElement;
  onMount(() => {
    const observer = new ResizeObserver(() => props.onHeight(dock.offsetHeight));
    observer.observe(dock);
    onCleanup(() => { observer.disconnect(); props.onHeight(0); });
  });

  return <div ref={dock} class="question-dock panel-frame" role="dialog" aria-label={tr().chat.tool.ask}>
    <div class="question-head">
      <span class="question-progress-text">{tr().chat.question.progress(index() + 1, props.ask.questions.length)}</span>
      <div class="question-progress">
        <Index each={props.ask.questions}>{(_, at) =>
          <button type="button" class="question-segment" classList={{ active: at === index(), answered: answerOf(at).length > 0 }}
            aria-label={`${tr().chat.tool.ask} ${at + 1}`} onClick={() => setIndex(at)} />}</Index>
      </div>
    </div>
    <div class="question-text">{question().question}</div>
    <div class="question-hint">{question().multiple ? tr().chat.question.multi : tr().chat.question.single}</div>
    <div class="question-options">
      <For each={question().options}>{option => {
        const chosen = () => picked[index()].includes(option.label);
        return <button type="button" class="question-option" classList={{ picked: chosen() }} onClick={() => pick(option.label)}>
          <span class="question-box" classList={{ radio: !question().multiple, picked: chosen() }}><Show when={chosen() && question().multiple}><Glyph name="check" size={12} /></Show></span>
          <span class="question-option-main"><span class="question-label">{option.label}</span><span class="question-description">{option.description}</span></span>
        </button>;
      }}</For>
      <input class="question-custom" placeholder={tr().chat.question.custom} value={custom[index()]} onInput={event => type(event.currentTarget.value)}
        onKeyDown={event => { if (event.key === "Enter" && !event.isComposing) next(); }} />
    </div>
    <div class="question-footer">
      <Press label={tr().chat.question.dismiss} class="question-button" onClick={() => dismissAsk(props.chatId)}>{tr().chat.question.dismiss}</Press>
      <div class="question-actions">
        <Show when={index() > 0}><Press label={tr().chat.question.back} class="question-button" onClick={() => setIndex(index() - 1)}>{tr().chat.question.back}</Press></Show>
        <Press label={last() ? tr().chat.question.submit : tr().chat.question.next} class="question-button" variant="primary" onClick={next} disabled={answerOf(index()).length === 0}>
          {last() ? tr().chat.question.submit : tr().chat.question.next}
        </Press>
      </div>
    </div>
  </div>;
}
