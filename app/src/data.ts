import type { CommandInfo, SkillInfo, UsageBar } from "@nabu/protocol";

export interface Project {
  path: string;
  name: string;
  branch: string;
}

export interface Session {
  id: string;
  project: Project;
  title: string;
  busy?: boolean;
}

export interface Provider {
  id: string;
  name: string;
  icon: string;
}

export interface Model {
  id: string;
  name: string;
  provider: Provider;
  /** Reasoning effort levels; none means the model has no effort setting. */
  efforts: string[];
  fast: boolean;
  /** Context window in tokens. */
  context: number;
  images: boolean;
  /** An older model, listed apart from the current ones. */
  older?: boolean;
}

const anthropic: Provider = { id: "anthropic", name: "Anthropic", icon: "anthropic" };
const openai: Provider = { id: "openai", name: "OpenAI", icon: "openai" };

const inHours = (hours: number) => new Date(Date.now() + hours * 3_600_000).toISOString();
const share = (label: string, used: number, hours: number): UsageBar =>
  ({ label, unit: "share", used, total: 1, reset: inHours(hours), idle: false, partial: false });

export const demoUsage: Record<string, UsageBar[]> = {
  anthropic: [share("5h", .38, 1.84), share("week", .86, 76.25)],
  openai: [share("5h", .28, 2.34)],
};

const levels = ["low", "medium", "high", "xhigh", "max"];
export const models: Model[] = [
  { id: "opus", name: "Claude Opus 5.5", provider: anthropic, efforts: levels, fast: true, context: 1000000, images: true },
  { id: "sonnet", name: "Claude Sonnet 5.5", provider: anthropic, efforts: levels, fast: false, context: 1000000, images: true },
  { id: "astra", name: "GPT 6.2 Astra", provider: openai, efforts: ["low", "medium", "high"], fast: true, context: 400000, images: true },
  { id: "sol", name: "GPT 6.2 Sol", provider: openai, efforts: ["low", "medium", "high"], fast: false, context: 400000, images: true },
  { id: "terra", name: "GPT 6.1 Terra", provider: openai, efforts: [], fast: false, context: 200000, images: false },
  { id: "luna", name: "GPT 6 Luna", provider: openai, efforts: [], fast: false, context: 128000, images: false },
];

export const contextUsage = { used: 400000, limit: 1000000, compactAt: 750000 };
export const demoContextParts = { system: 11000, tools: 15000, messages: 352000, deferred: 49000 };

export function createDemo(root: string) {
  const normalized = root.replace(/[\\/]$/, "");
  const slash = normalized.includes("\\") ? "\\" : "/";
  const parent = normalized.slice(0, normalized.lastIndexOf(slash));
  const projects: Project[] = [
    { path: normalized, name: "NabuCode", branch: "Main" },
    { path: `${parent}${slash}OmagadPlayer`, name: "OmagadPlayer", branch: "develop" },
    { path: `${parent}${slash}NabuCore`, name: "NabuCore", branch: "Main" },
  ];
  const projectChoices = [
    ...projects,
    { path: `${normalized}${slash}app`, name: "NabuCode App", branch: "Main" },
    { path: `${normalized}${slash}crates`, name: "NabuCode Crates", branch: "Main" },
  ];
  const sessions: Session[] = [
    { id: "chat-1", project: projects[0], title: "Чат номер 1" },
    { id: "chat-2", project: projects[0], title: "Чат номер 2" },
    { id: "long", project: projects[0], title: "Чат с длинным очень длинным названием" },
    { id: "playlist", project: projects[1], title: "Раскумар плейлист", busy: true },
    { id: "core", project: projects[2], title: "Проверка NabuCore" },
  ];
  const recent: Session[] = [
    { id: "latest", project: projects[0], title: "Латест чат" },
    sessions[0],
    { id: "latest-3", project: projects[0], title: "Латест чат 3" },
  ];
  return { projects, projectChoices, sessions, recent };
}

export const demoCommands: CommandInfo[] = [{ name: "compact", description: "Сжимает историю чата в краткую сводку" }];
export const demoSkills: SkillInfo[] = [
  { name: "review", description: "Ищет баги в текущем диффе и предлагает правки", described: true, preview: "# Ревью Прочитай дифф целиком, найди места, где поведение меняется, и проверь каждое: граничные случаи, ошибки, гонки…", enabled: true, path: "demo/review", harness: "claude", scope: "project" },
  { name: "tdd", description: "Разработка через тесты: сначала падающий тест, потом код, который его проходит, потом чистка. Подходит для новых функций и исправления багов, когда поведение можно описать проверкой.", described: true, preview: "", enabled: true, path: "demo/tdd", harness: "claude", scope: "global" },
  { name: "deploy", description: "Собирает и выкладывает проект", described: false, preview: "Собери проект скриптом scripts/build.ps1, проверь, что тесты прошли, и выложи сборку на сервер…", enabled: true, path: "demo/deploy", harness: "nabucode", scope: "project" },
  { name: "skill-creator", description: "Create new skills, modify and improve existing skills, and measure skill performance. Use when users want to create a skill from scratch, edit, or optimize an existing skill, run evals to test a skill, benchmark skill performance with variance analysis, or optimize a skill's description for better triggering accuracy.", described: true, preview: "# Skill Creator A skill for creating new skills and iteratively improving them. At a high level, the process of creating a skill goes like this: decide what you want the skill to do and roughly how it should do it, write a draft of the skill, create a few test prompts and run claude-with-access-to-the-skill on them…", enabled: true, path: "demo/skill-creator", harness: "claude", scope: "global" },
  { name: "code-review", description: "Review the current diff for correctness bugs", described: true, preview: "", enabled: true, path: "demo/code-review", harness: "claude", scope: "global" },
  { name: "handoff", description: "Передаёт работу другой сессии с кратким описанием", described: true, preview: "", enabled: true, path: "demo/handoff", harness: "claude", scope: "global" },
  { name: "grill-me", description: "Устраивает допрос по плану, пока не останется дыр", described: true, preview: "", enabled: true, path: "demo/grill-me", harness: "claude", scope: "global" },
  { name: "triage", description: "Разбирает входящие задачи по важности", described: true, preview: "", enabled: true, path: "demo/triage", harness: "claude", scope: "global" },
  { name: "to-spec", description: "Превращает разговор в спецификацию", described: true, preview: "", enabled: true, path: "demo/to-spec", harness: "claude", scope: "global" },
  { name: "wayfinder", description: "Ищет путь по незнакомой кодовой базе", described: true, preview: "", enabled: true, path: "demo/wayfinder", harness: "claude", scope: "global" },
  { name: "writing-tests", description: "Пишет тесты по правилам проекта", described: true, preview: "", enabled: true, path: "demo/writing-tests", harness: "claude", scope: "global" },
  { name: "writing-docs", description: "Пишет документацию", described: true, preview: "", enabled: true, path: "demo/writing-docs", harness: "claude", scope: "global" },
];
