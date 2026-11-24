import { createSignal } from "solid-js";
import { ru } from "./ru";
import { en } from "./en";

export type Language = "ru" | "en";
export type Dictionary = typeof ru;

export const languageNames: Record<Language, string> = { ru: "Русский", en: "English" };

const dictionaries: Record<Language, Dictionary> = { ru, en };

// Not picked yet: Russian on a Russian system, else English; installs from before this were Russian
function startLanguage(): Language {
  const saved = localStorage.getItem("everyend-language");
  if (saved === "ru" || saved === "en") return saved;
  const earlier = localStorage.getItem("everyend-settings") || localStorage.getItem("everyend-theme");
  const found = earlier || navigator.languages.some(tag => tag.toLowerCase().startsWith("ru")) ? "ru" : "en";
  localStorage.setItem("everyend-language", found);
  return found;
}

const [language, setLanguageSignal] = createSignal<Language>(startLanguage());
export { language };
document.documentElement.lang = language();

export const tr = () => dictionaries[language()];

export function setLanguage(next: Language) {
  localStorage.setItem("everyend-language", next);
  document.documentElement.lang = next;
  setLanguageSignal(next);
}
