import { createSignal } from "solid-js";
import { createStore } from "solid-js/store";
import type { ModelRef } from "@nabu/protocol";
import { chats } from "./chat/model";
import type { Model } from "./data";

/**
 * The model, effort and fast mode the composer sends with. A new chat takes the last pick; a chat goes on with what
 * its last message was sent with, and a pick inside it changes only that chat. Effort is remembered for each model.
 */
type Choice = { provider: string; model: string; effort?: string; fast: boolean };

const modelKey = "everyend-model";
const effortsKey = "everyend-efforts";

function read<T>(key: string, fallback: T): T {
  try { return JSON.parse(localStorage.getItem(key) || "null") ?? fallback; } catch { return fallback; }
}

const [lastPick, setLastPick] = createSignal<Choice | null>(read(modelKey, null));
const [efforts, setEfforts] = createStore<Record<string, string>>(read(effortsKey, {}));
// Picks made in a chat stay until its next message carries them
const [chatPicks, setChatPicks] = createStore<Record<string, Choice>>({});

export const keyOf = (model: Model) => `${model.provider.id}/${model.id}`;

function sentWith(chatId: string): Choice | undefined {
  const ref = chats[chatId]?.messages.findLast(message => message.sent)?.sent;
  return ref && { provider: ref.providerID, model: ref.modelID, effort: ref.variant, fast: !!ref.fast };
}

const choiceOf = (chatId: string | null) => (chatId && (chatPicks[chatId] ?? sentWith(chatId))) || lastPick();

/** The chat's model among those on offer; an unknown or switched-off one gives way to the first. */
export function chosenModel(chatId: string | null, list: Model[]) {
  const choice = choiceOf(chatId);
  return list.find(model => model.provider.id === choice?.provider && model.id === choice.model) ?? list[0];
}

function defaultEffort(levels: string[]) {
  return levels.includes("medium") ? "medium" : levels[Math.floor(levels.length / 2)];
}

/** The effort to send with; none for a model without levels. */
export function chosenEffort(chatId: string | null, model: Model) {
  if (!model.efforts.length) return undefined;
  const choice = choiceOf(chatId);
  const own = choice?.provider === model.provider.id && choice.model === model.id ? choice.effort : undefined;
  return [own, efforts[keyOf(model)]].find(level => level && model.efforts.includes(level)) ?? defaultEffort(model.efforts);
}

export const chosenFast = (chatId: string | null, model: Model) => model.fast && !!choiceOf(chatId)?.fast;

export function modelRef(chatId: string | null, model: Model): ModelRef {
  const fast = chosenFast(chatId, model);
  return { providerID: model.provider.id, modelID: model.id, variant: chosenEffort(chatId, model), ...(fast ? { fast } : {}) };
}

function save(chatId: string | null, choice: Choice) {
  if (chatId) setChatPicks(chatId, choice);
  setLastPick(choice);
  localStorage.setItem(modelKey, JSON.stringify(choice));
}

export function pickModel(chatId: string | null, model: Model) {
  const fast = !!choiceOf(chatId)?.fast;
  save(chatId, { provider: model.provider.id, model: model.id, effort: efforts[keyOf(model)], fast });
}

export function pickEffort(chatId: string | null, model: Model, effort: string) {
  setEfforts(keyOf(model), effort);
  localStorage.setItem(effortsKey, JSON.stringify(efforts));
  save(chatId, { provider: model.provider.id, model: model.id, effort, fast: chosenFast(chatId, model) });
}

export function pickFast(chatId: string | null, model: Model, fast: boolean) {
  save(chatId, { provider: model.provider.id, model: model.id, effort: chosenEffort(chatId, model), fast });
}

/** How full the chat's context is after its last answer, against the window of the model that gave it. */
export function contextOf(chatId: string, list: Model[]) {
  const usage = chats[chatId]?.messages.findLast(message => message.usage)?.usage;
  const limit = usage && list.find(model => keyOf(model) === usage.model)?.context;
  return usage && limit ? { used: usage.tokens, limit } : undefined;
}
