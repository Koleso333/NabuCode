import { Failure, type Reply, type Script } from "./engine";
import type { FileChange } from "./model";

// Demo replies until the server is connected: each new message plays the next one
const fence = "```";

// Lines start with " ", "+" or "-", as in a unified diff
function change(relativePath: string, start: number, lines: string, type: FileChange["type"] = "update"): FileChange {
  const body = lines.split("\n");
  const additions = body.filter(line => line[0] === "+").length;
  const deletions = body.filter(line => line[0] === "-").length;
  const before = body.length - additions;
  const after = body.length - deletions;
  const header = `--- ${type === "add" ? "/dev/null" : relativePath}\n+++ ${relativePath}\n@@ -${type === "add" ? 0 : start},${before} +${start},${after} @@`;
  return { filePath: `C:/src/NabuCode/everyend/${relativePath}`, relativePath, type, patch: `${header}\n${lines}`, additions, deletions };
}

// Read tool output: "number<tab>text", with a note when the file goes on
function numbered(from: number, lines: string[], total?: number) {
  const to = from + lines.length - 1;
  const note = total && to < total ? `\n(lines ${from}-${to} of ${total}; use offset=${to + 1} to continue)` : "";
  return lines.map((line, index) => `${from + index}\t${line}`).join("\n") + note;
}

const themeEdit = change("src/theme.ts", 140, [
  " function readSaved(): ThemeChoice | null {",
  "-  try {",
  "-    const saved = JSON.parse(localStorage.getItem(\"everyend-theme\") || \"null\");",
  "-    if (!saved) return null;",
  "-    return {",
  "-      theme: Object.hasOwn(themes, saved.theme) ? saved.theme : \"oc-2\",",
  "-      mode: [\"system\", \"dark\", \"light\"].includes(saved.mode) ? saved.mode : \"system\",",
  "-    };",
  "-  } catch {",
  "-    return null;",
  "-  }",
  "+  return parseChoice(localStorage.getItem(\"everyend-theme\"));",
  " }",
].join("\n"));

const themeFile = change("src/theme-file.ts", 1, [
  "+import themes from \"./themes.json\";",
  "+import type { ThemeChoice } from \"./theme\";",
  "+",
  "+const modes = [\"system\", \"dark\", \"light\"];",
  "+",
  "+// One parser for the saved choice and the Flutter prototype's theme.json",
  "+export function parseChoice(text: string | null): ThemeChoice | null {",
  "+  try {",
  "+    const saved = JSON.parse(text || \"null\");",
  "+    if (!saved) return null;",
  "+    return {",
  "+      theme: Object.hasOwn(themes, saved.theme) ? saved.theme : \"oc-2\",",
  "+      mode: modes.includes(saved.mode) ? saved.mode : \"system\",",
  "+    };",
  "+  } catch {",
  "+    return null;",
  "+  }",
  "+}",
].join("\n"), "add");

const mainEdit = change("src/main.tsx", 419, [
  "     const finish = async () => {",
  "       if (appWindow) {",
  "-        importTheme(await invoke<ThemeChoice | null>(\"theme_choice\").catch(() => null));",
  "+        importTheme(parseChoice(await invoke<string>(\"theme_text\").catch(() => null)));",
  "       }",
].join("\n"));

const rustEdit = change("src-tauri/src/main.rs", 31, [
  " #[tauri::command]",
  "-fn theme_choice() -> Option<serde_json::Value> {",
  "-    let text = std::fs::read_to_string(theme_path()?).ok()?;",
  "-    serde_json::from_str(&text).ok()",
  "+fn theme_text() -> Option<String> {",
  "+    std::fs::read_to_string(theme_path()?).ok()",
  " }",
].join("\n"));

const main: Script = async reply => {
  await reply.wait(900);
  await reply.think("**Разбираю запрос**\n\nНужно понять, где читается сохранённая тема и почему выбор из Flutter-прототипа иногда теряется. Сначала посмотрю `theme.ts` и место запуска в `main.tsx`.");
  const theme = reply.tool("read", { path: "src/theme.ts" });
  const app = reply.tool("read", { path: "src/main.tsx", offset: 416, limit: 9 });
  await reply.wait(700);
  reply.done(theme, numbered(138, [
    "",
    "function readSaved(): ThemeChoice | null {",
    "  try {",
    "    const saved = JSON.parse(localStorage.getItem(\"everyend-theme\") || \"null\");",
    "    if (!saved) return null;",
    "    return {",
    "      theme: Object.hasOwn(themes, saved.theme) ? saved.theme : \"oc-2\",",
    "      mode: [\"system\", \"dark\", \"light\"].includes(saved.mode) ? saved.mode : \"system\",",
    "    };",
    "  } catch {",
    "    return null;",
    "  }",
    "}",
  ]));
  await reply.wait(250);
  reply.done(app, numbered(416, [
    "  onMount(() => {",
    "    const finish = async () => {",
    "      if (appWindow) {",
    "        importTheme(await invoke<ThemeChoice | null>(\"theme_choice\").catch(() => null));",
    "      }",
    "      setReady(true);",
    "    };",
    "    void finish();",
    "  });",
  ], 616));
  await reply.run("grep", { pattern: "theme_choice|everyend-theme", path: "src" }, 600,
    "src/theme.ts:142:    const saved = JSON.parse(localStorage.getItem(\"everyend-theme\") || \"null\");\nsrc/theme.ts:176:  localStorage.setItem(\"everyend-theme\", JSON.stringify(choice()));\nsrc/main.tsx:422:        importTheme(await invoke<ThemeChoice | null>(\"theme_choice\").catch(() => null));");
  await reply.run("glob", { pattern: "src-tauri/src/*.rs", path: "." }, 400, "src-tauri/src/main.rs\nsrc-tauri/src/fonts.rs");
  await reply.run("list", { path: "src" }, 350, "chat/\ni18n/\nbootstrap.ts\ncontrols.tsx\ndata.ts\nfonts.ts\nmain.tsx\npreferences.ts\nsettings.tsx\nstyles.css\ntheme.ts\nthemes.json");
  await reply.think("Тема разбирается дважды: в `readSaved` и на стороне Rust через `serde_json`. Rust отдаёт уже разобранный объект, и если в файле лишнее поле, `importTheme` получает `null`. Проще отдавать сырой текст и разбирать его в одном месте.");
  await reply.say("Нашёл причину: сохранённая тема разбирается в двух местах по-разному. Вынесу разбор в одну функцию и буду отдавать из Rust сырой текст.");
  await reply.wait(500);
  await reply.run("write", { path: "src/theme-file.ts" }, 700, "", { files: [themeFile] });
  await reply.run("edit", { path: "src/theme.ts" }, 600, "", { files: [themeEdit] });
  await reply.run("edit", { path: "src/main.tsx" }, 800, "", { files: [mainEdit, rustEdit] });
  await reply.run("powershell", { command: "npx tsc --noEmit", description: "Проверка типов" }, 1600, "");
  await reply.run("powershell", { command: "cargo check --manifest-path src-tauri/Cargo.toml" }, 2200,
    "    Checking everyend-tauri v0.1.0 (C:\\src\\NabuCode\\everyend\\src-tauri)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.41s");
  await reply.say([
    "## Что изменил",
    "",
    "Тема из Flutter-прототипа терялась, потому что Rust разбирал `theme.json` строго и возвращал `null` при любом лишнем поле.",
    "",
    "1. Новый файл `src/theme-file.ts` с функцией `parseChoice` — единственное место, где разбирается выбор темы.",
    "2. `readSaved` в `src/theme.ts` теперь просто вызывает `parseChoice`.",
    "3. Команда Rust переименована в `theme_text` и отдаёт сырой текст файла.",
    "",
    `${fence}ts`,
    "export function parseChoice(text: string | null): ThemeChoice | null {",
    "  const saved = JSON.parse(text || \"null\");",
    "  return saved && { theme: saved.theme, mode: saved.mode };",
    "}",
    fence,
    "",
    "| Проверка | Результат |",
    "| --- | --- |",
    "| `tsc --noEmit` | без ошибок |",
    "| `cargo check` | без ошибок |",
    "",
    "> Старый файл темы читается как раньше, менять его не нужно.",
  ].join("\n"));
};

const serverAgent: Script = async reply => {
  await reply.wait(500);
  await reply.think("Смотрю, как сервер раздаёт тулы и где решается, какие из них подгружаются позже.");
  const tools = reply.tool("read", { path: "crates/nabu-server/src/tools/mod.rs" });
  const agent = reply.tool("read", { path: "crates/nabu-server/src/tools/agent.rs" });
  await reply.wait(800);
  reply.done(tools, numbered(80, [
    "pub const TOOLS: [&str; 11] = [",
    "    \"read\", \"list\", \"glob\", \"grep\", \"edit\", \"write\", \"bash\", \"powershell\", \"task\", \"agent\", \"ask\",",
    "];",
    "",
    "// Loaded on demand through the tools tool",
    "const DEFERRED: [(&str, &str); 2] = [(\"search\", search::BRIEF), (\"fetch\", fetch::BRIEF)];",
  ], 260));
  reply.done(agent, numbered(1, [
    "use crate::session::Sessions;",
    "",
    "/// Runs a subagent in its own session; it can't start subagents of its own",
    "pub async fn run(sessions: &Sessions, parent: &str, args: &Value) -> Result<String, String> {",
    "    let prompt = str_arg(args, \"prompt\")?;",
    "    let child = sessions.spawn_child(parent, args).await?;",
    "    child.run(prompt).await",
    "}",
  ]));
  await reply.run("grep", { pattern: "DEFERRED|fn loadable", path: "crates/nabu-server/src" }, 700,
    "tools/mod.rs:86:const DEFERRED: [(&str, &str); 2] = [(\"search\", search::BRIEF), (\"fetch\", fetch::BRIEF)];\ntools/mod.rs:212:    fn loadable(&self) -> Vec<&'static str> {");
  await reply.run("read", { path: "crates/nabu-server/src/tools/jobs.rs", offset: 480, limit: 6 }, 600, numbered(480, [
    "    fn wait(&self, id: &str, until: Option<&str>, timeout: Duration) -> JobState {",
    "        let deadline = Instant::now() + timeout;",
    "        loop {",
    "            if let Some(done) = self.finished(id, until) { return done; }",
    "            if Instant::now() >= deadline { return self.state(id); }",
    "            thread::sleep(POLL);",
  ], 612));
  await reply.say("Сервер держит 11 тулов в таблице `TOOLS`. `search` и `fetch` отложены: модель подгружает их через `tools`. `agent` и `ask` обрабатываются отдельно, потому что им нужен доступ к сессиям. Субагент не может запускать своих субагентов.");
};

const uiAgent: Script = async reply => {
  await reply.wait(700);
  await reply.run("glob", { pattern: "app/ui/kit/src/components/message-part*", path: "." }, 500,
    "app/ui/kit/src/components/message-part.tsx\napp/ui/kit/src/components/message-part.css");
  const part = reply.tool("read", { path: "app/ui/kit/src/components/message-part.tsx", offset: 540, limit: 80 });
  const css = reply.tool("read", { path: "app/ui/kit/src/components/session-turn.css", offset: 230 });
  await reply.wait(900);
  reply.done(part, numbered(540, [
    "export function groupParts(parts: Part[]) {",
    "  const groups: Group[] = [];",
    "  let run: Part[] = [];",
    "  for (const part of parts) {",
    "    if (isStep(part)) { run.push(part); continue; }",
    "    flush();",
    "    groups.push({ kind: \"part\", parts: [part] });",
    "  }",
  ], 1210));
  await reply.wait(300);
  reply.done(css, numbered(230, [
    "[data-chat-view=\"new\"] [data-slot=\"steps\"] {",
    "  display: flex;",
    "  flex-direction: column;",
    "  gap: 2px;",
    "}",
  ]));
  await reply.run("bash", { command: "rg -c \"data-chat-view\" app/ui", description: "Где используется новый вид" }, 900,
    "app/ui/app/src/pages/session/message-timeline.tsx:1\napp/ui/kit/src/components/session-turn.css:1");
  await reply.think("Группа шагов собирается из частей между двумя текстами, если в ней хотя бы два тула.");
  await reply.say("В UI новый вид чата включается атрибутом `data-chat-view=\"new\"`. Группировку делает `groupParts`: рассуждения и тулы между двумя текстами сворачиваются в одну строку, если тулов два и больше. Агент и `ask` в группу не попадают.");
};

const agents: Script = async reply => {
  await reply.wait(700);
  await reply.think("Задача большая, разделю её: сервер и UI можно изучать независимо.");
  await reply.say("Запущу двух агентов параллельно: один разберёт тулы сервера, второй — как UI показывает их в ленте.");
  await Promise.all([
    reply.agent({ description: "Разобрать тулы сервера", prompt: "Изучи, как nabu-server регистрирует тулы и какие из них подгружаются позже. Ничего не меняй.", subagent_type: "explore" }, serverAgent),
    reply.agent({ description: "Разобрать вид тулов в UI", prompt: "Найди в app/ui, как новый вид чата группирует тулы. Ничего не меняй.", subagent_type: "explore" }, uiAgent),
  ]);
  await reply.wait(400);
  await reply.say([
    "Оба агента закончили. Коротко:",
    "",
    "- **Сервер** отдаёт 11 тулов, `search` и `fetch` подключаются через `tools` по требованию.",
    "- **UI** сворачивает тулы между двумя текстами в группу шагов, если их два и больше.",
    "",
    "Агентов можно открыть: нажмите на карточку, чтобы посмотреть, что делал каждый.",
  ].join("\n"));
};

const web: Script = async reply => {
  await reply.wait(800);
  await reply.think("Нужны свежие данные о Tauri 2, своих знаний мало. Подключу поиск и загрузку страниц.");
  await reply.run("tools", { load: ["search", "fetch"] }, 400, "loaded: search, fetch");
  await reply.run("search", { query: "tauri 2 data-tauri-drag-region custom titlebar" }, 1400,
    [
      "\"tauri 2 data-tauri-drag-region custom titlebar\" (brave 3; yahoo 3)",
      "1. Window Customization | Tauri",
      "   https://v2.tauri.app/learn/window-customization/",
      "   Tauri provides lots of options for customizing the look and feel of your app's window, including custom titlebars.",
      "2. data-tauri-drag-region doesn't work on nested elements · Issue #9503",
      "   https://github.com/tauri-apps/tauri/issues/9503",
      "   2024-04-18 — Dragging only starts on the element that has the attribute itself, not on its children.",
      "3. window | Tauri JavaScript API",
      "   https://v2.tauri.app/reference/javascript/api/namespacewindow/",
      "   startDragging(): starts dragging the window. Call it on mousedown to drag from any element.",
    ].join("\n"));
  await reply.run("fetch", { url: "https://v2.tauri.app/learn/window-customization/" }, 1200, "# Window Customization\nTauri provides lots of options for customizing the look and feel of your app's window...");
  await reply.say("Документация подтверждает: `startDragging()` работает и без атрибута. Проверю, как ведёт себя dev-сервер с этим изменением.");
  await reply.run("powershell", { command: "npm run dev", description: "Запуск dev-сервера", background: true }, 600,
    "Started task1 in the background.\nLog: C:\\Users\\me\\AppData\\Local\\NabuCode\\tasks\\task1.log");
  await reply.run("wait", { seconds: 4 }, 4000, "4 s passed; now 15:18:11.");
  await reply.run("task", { id: "task1", wait: 20, until: "ready in" }, 2600,
    "task1: running\n\n> everyend-tauri@0.1.0 dev\n> vite\n\n  VITE v7.3.8  ready in 412 ms\n\n  ➜  Local:   http://localhost:1431/");
  await reply.run("task", { id: "task1", stop: true }, 500, "task1: stopped (exit code 1)");
  await reply.say("Сервер поднимается за 0,4 с, ошибок в логе нет. Задачу остановил, чтобы не держать порт.");
};

const question: Script = async reply => {
  await reply.wait(600);
  await reply.think("Пользователь не сказал, где хранить настройку и для каких окон её применять. Лучше спросить, чем угадывать.");
  await reply.say("Перед правкой уточню пару моментов.");
  const answers = await reply.ask([
    {
      header: "Хранение",
      question: "Где хранить ширину боковой панели?",
      options: [
        { label: "localStorage (recommended)", description: "Как остальные настройки интерфейса, без участия Rust." },
        { label: "Файл настроек", description: "Рядом с theme.json, переживёт переустановку WebView." },
      ],
    },
    {
      header: "Окна",
      question: "Для каких окон запоминать ширину?",
      multiple: true,
      options: [
        { label: "Главное окно", description: "Окно с чатом и проектами." },
        { label: "Окно настроек", description: "Если настройки откроются отдельным окном." },
        { label: "Окно агента", description: "Будущее окно для просмотра агента отдельно." },
      ],
    },
  ]);
  await reply.wait(500);
  const where = answers[0]?.[0] ?? "localStorage";
  await reply.run("edit", { path: "src/main.tsx" }, 900, "", {
    files: [change("src/main.tsx", 149, [
      "-  const [preferredSidebar, setPreferredSidebar] = createSignal(258);",
      "+  const [preferredSidebar, setPreferredSidebar] = createSignal(Number(localStorage.getItem(\"everyend-sidebar\")) || 258);",
      "+  createEffect(() => localStorage.setItem(\"everyend-sidebar\", String(preferredSidebar())));",
    ].join("\n"))],
  });
  await reply.say(`Готово. Ширина хранится так: **${where}**, окна: ${(answers[1] ?? []).join(", ") || "не выбраны"}. Значение восстанавливается при запуске.`);
};

const failures: Script = async reply => {
  await reply.wait(700);
  const config = reply.tool("read", { path: "src/config.ts" });
  await reply.wait(500);
  reply.fail(config, "file not found: src/config.ts");
  await reply.say("Такого файла нет. Запущу тесты, чтобы понять, что именно ломается.");
  await reply.run("bash", { command: "cargo test -p nabu-server tools::", description: "Тесты тулов" }, 2400, [
    "running 14 tests",
    "test tools::changes::tests::counts_lines ... ok",
    "test tools::glob::tests::skips_ignored ... ok",
    "test tools::grep::tests::limits_lines ... FAILED",
    "",
    "failures:",
    "---- tools::grep::tests::limits_lines stdout ----",
    "thread 'tools::grep::tests::limits_lines' panicked at src/tools/grep.rs:188:9:",
    "assertion `left == right` failed",
    "  left: 201",
    "  right: 200",
    "",
    "test result: FAILED. 13 passed; 1 failed; 0 ignored",
  ].join("\n"));
  await reply.say("Падает `limits_lines`: выводится на одну строку больше лимита. Поправлю условие.");
  const edit = reply.tool("edit", { path: "crates/nabu-server/src/tools/grep.rs" });
  await reply.wait(700);
  reply.fail(edit, "`old` text not found in crates/nabu-server/src/tools/grep.rs: the file changed since it was read");
  await reply.wait(600);
  throw new Failure("529: сервер перегружен. Попробуйте ещё раз чуть позже.");
};

const longParagraph = (topic: string) => `Когда лента получает очередной кусок текста, перерисовывать весь ответ незачем: готовые абзацы уже не изменятся. Поэтому ${topic} разбирается по блокам, и обновляется только последний, недописанный блок. Остальные блоки рендерятся один раз и берутся из кэша.`;

const long: Script = async reply => {
  await reply.wait(500);
  const sections = ["разметка", "подсветка кода", "диффы", "прокрутка", "статусы", "агенты"];
  const text = sections.map((topic, index) => [
    `## ${index + 1}. ${topic[0].toUpperCase()}${topic.slice(1)}`,
    "",
    longParagraph(topic),
    "",
    "- готовые блоки не перерисовываются;",
    "- последний блок обновляется на каждом куске;",
    `- тяжёлая работа (${topic}) откладывается до простоя.`,
    "",
    `${fence}${index % 2 ? "rust" : "ts"}`,
    ...(index % 2
      ? ["pub fn split(text: &str) -> Vec<&str> {", "    text.split(\"\\n\\n\").filter(|block| !block.is_empty()).collect()", "}"]
      : ["export function split(text: string) {", "  return text.split(\"\\n\\n\").filter(block => block.length > 0);", "}"]),
    fence,
    "",
    longParagraph(topic),
  ].join("\n")).join("\n\n");
  await reply.stream("text", text, 6);
};

const scripts = [main, agents, web, question, failures, long];
let next = 0;

export function nextScript() {
  return scripts[next++ % scripts.length];
}

export const history: { chatId: string; text: string; script: Script }[] = [
  { chatId: "chat-1", text: "Почему тема из Flutter-прототипа иногда не подхватывается?", script: main },
  { chatId: "chat-2", text: "Разберись, как сервер и UI работают с тулами", script: agents },
];

export type { Reply };
