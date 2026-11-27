import { createResource, createSignal, For, Show } from "solid-js";
import type { SkillHarness, SkillInfo, SkillScope, SkillSource } from "@nabu/protocol";
import { tr } from "./i18n";
import { Glyph } from "./controls";
import { demoMode } from "./chat/actions";
import { demoSkills } from "./data";
import { preferences, setPreference } from "./preferences";
import { onConnected, request } from "./server";
import { panelMode } from "./panel";
import { FilterField, Page, Section, Toggle, ToggleRow } from "./settings-parts";

type Place = { harness: SkillHarness; scope: SkillScope; folders: string };

// In the order the server searches them
const places: Place[] = [
  { harness: "nabucode", scope: "project", folders: ".nabucode\\skills" },
  { harness: "nabucode", scope: "global", folders: "~\\.nabucode\\skills" },
  { harness: "claude", scope: "project", folders: ".claude\\skills, .claude\\commands" },
  { harness: "claude", scope: "global", folders: "~\\.claude\\skills, ~\\.claude\\commands" },
  { harness: "claude", scope: "synced", folders: "" },
  { harness: "codex", scope: "project", folders: ".agents\\skills" },
  { harness: "codex", scope: "global", folders: "~\\.codex\\skills, ~\\.agents\\skills" },
];

const harnessNames: Record<SkillHarness, string> = { nabucode: "NabuCode", claude: "Claude Code", codex: "Codex" };

const enabled = (place: Place) => !!(preferences.skills[place.harness] as Partial<Record<SkillScope, boolean>>)[place.scope];

function sendSources() {
  const sources: SkillSource[] = places.map(place => ({ harness: place.harness, scope: place.scope, enabled: enabled(place) }));
  return Promise.all([request("setSkillSources", { sources }), request("setDisabledSkills", { paths: [...preferences.disabledSkills] })]);
}

// The panel leaves the sources to everyend
if (!demoMode && !panelMode) onConnected(() => void sendSources().catch(() => undefined));

function SkillMark(props: { harness: SkillHarness }) {
  return <Show when={props.harness !== "nabucode"} fallback={<img class="skill-logo" src={`${import.meta.env.BASE_URL}logo.png`} width="20" height="20" alt="" />}>
    <Glyph name={props.harness === "claude" ? "providers/anthropic" : "providers/openai"} size={20} />
  </Show>;
}

function SkillRow(props: { skill: SkillInfo; onToggle: (on: boolean) => void }) {
  const on = () => !preferences.disabledSkills.includes(props.skill.path);
  return <div class={`settings-row provider skill-row ${on() ? "" : "disabled"}`}>
    <div class="settings-row-text">
      <span class="settings-row-title"><span class="provider-name">
        <SkillMark harness={props.skill.harness} />
        <span class="provider-title">{props.skill.name}</span>
        <span class="skill-scope">{tr().settings.skills.scopes[props.skill.scope]}</span>
      </span></span>
      <Show when={props.skill.described}><span class="settings-row-description">{props.skill.description}</span></Show>
      <Show when={props.skill.preview}><span class="skill-preview"><span>{props.skill.preview}</span></span></Show>
    </div>
    <div class="settings-row-control"><Toggle label={props.skill.name} on={on()} onChange={props.onToggle} /></div>
  </div>;
}

export function SkillsPage(props: { directory: string }) {
  const text = () => tr().settings.skills;
  const [filter, setFilter] = createSignal("");
  const [skills, { refetch }] = createResource(() => props.directory || demoMode, () =>
    demoMode ? demoSkills : request("skills", { directory: props.directory }).catch(() => [] as SkillInfo[]));
  const shown = () => {
    const query = filter().trim().toLowerCase();
    return (skills() ?? []).filter(skill => !query || `${skill.name} ${skill.description}`.toLowerCase().includes(query));
  };
  const send = () => { if (!demoMode) void sendSources().then(() => refetch(), () => undefined); };
  const toggle = (place: Place, on: boolean) => {
    setPreference("skills", place.harness, place.scope as never, on as never);
    send();
  };
  const toggleSkill = (skill: SkillInfo, on: boolean) => {
    const others = preferences.disabledSkills.filter(path => path !== skill.path);
    setPreference("disabledSkills", on ? others : [...others, skill.path]);
    send();
  };
  const placeRow = (place: Place) => place.scope === "synced"
    ? <ToggleRow text={text().synced} on={enabled(place)} onChange={on => toggle(place, on)} />
    : <ToggleRow text={{ title: text().scopes[place.scope], description: place.folders }} on={enabled(place)} onChange={on => toggle(place, on)} />;
  return <Page title={tr().settings.tabs.skills}>
    <For each={Object.keys(harnessNames) as SkillHarness[]}>{harness => <Section title={harnessNames[harness]}>
      <For each={places.filter(place => place.harness === harness)}>{placeRow}</For>
    </Section>}</For>
    <Section head={<><h3>{text().list}</h3><div class="skill-search"><FilterField value={filter()} placeholder={text().search} onInput={setFilter} /></div></>}>
      <Show when={shown().length} fallback={<div class="skill-empty">{filter() ? text().nothing : text().none}</div>}>
        <For each={shown()}>{skill => <SkillRow skill={skill} onToggle={on => toggleSkill(skill, on)} />}</For>
      </Show>
    </Section>
  </Page>;
}
