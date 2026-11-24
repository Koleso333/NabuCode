import { createMemo, createSignal, For, on, createEffect, onCleanup, Show, type JSX } from "solid-js";
import type { Account, ProviderEntry } from "@nabu/protocol";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { tr } from "./i18n";
import { ConfirmDialog, ContextMenu, Glyph, IconButton, MenuItem, Popover, Press } from "./controls";
import { Button, Empty, FilterField, Page, Row, Section, TextBox, Toggle, ToggleRow, type Text } from "./settings-parts";
import { accountsOf, catalog, entryOf, onLoginFailed, usageOf, watchProvider } from "./providers";
import { isHidden, providerIcon, serverModels, setModelsHidden } from "./catalog";
import { UsageLine } from "./usage";
import { request } from "./server";
import { showFailure } from "./errors";
import type { Model } from "./data";

const isConnected = (entry: ProviderEntry) => entry.accounts > 0;

const providerName = (entry: ProviderEntry, tag?: JSX.Element) =>
  <span class="provider-name"><Glyph name={providerIcon(entry.id)} size={20} /><span class="provider-title">{entry.name}</span>{tag}</span>;

function setEnabled(entry: ProviderEntry, enabled: boolean) {
  void request("setProviderEnabled", { provider: entry.id, enabled }).catch(showFailure);
}

function removeProvider(entry: ProviderEntry) {
  const removing = entry.auth === "custom" ? request("removeCustomProvider", { id: entry.id }) : request("providerRemoveAll", { provider: entry.id });
  void removing.catch(showFailure);
}

// A provider page asked for from outside the settings, e.g. by its quota in the chat
const [requested, setRequested] = createSignal<string | null>(null);
export const openProviderPage = (id: string) => setRequested(id);

export function ProvidersPage() {
  const text = () => tr().settings.providers;
  const [opened, setOpened] = createSignal<string | null>(null);
  createEffect(() => {
    const id = requested();
    if (id) { setOpened(id); setRequested(null); }
  });
  const [menu, setMenu] = createSignal<{ x: number; y: number; entry: ProviderEntry } | null>(null);
  const [removing, setRemoving] = createSignal<ProviderEntry | null>(null);
  const connected = () => catalog().filter(isConnected);
  const available = () => catalog().filter(entry => !isConnected(entry) && entry.auth !== "custom");
  return <Show when={opened()} keyed fallback={<Page title={tr().settings.tabs.providers}>
    <Show when={connected().length}>
      <Section title={text().connected}>
        <For each={connected()}>{entry => <Row class={`provider ${entry.enabled ? "" : "disabled"}`}
          onContextMenu={event => setMenu({ x: event.clientX, y: event.clientY, entry })}
          title={providerName(entry, <>
            <Toggle label={text().enabled(entry.name)} on={entry.enabled} onChange={on => setEnabled(entry, on)} />
            <span class="tag">{text().auth[entry.auth]}</span>
          </>)}>
          <Show when={entry.auth !== "custom"}><Button label={text().settings} variant="ghost" onClick={() => setOpened(entry.id)} /></Show>
        </Row>}</For>
      </Section>
    </Show>
    <Section title={text().all}>
      <For each={available()}>{entry => <Row class="provider" description={text().notes[entry.id]} title={providerName(entry)}>
        <Button label={text().connect} icon="plus-small" onClick={() => setOpened(entry.id)} />
      </Row>}</For>
      {/* The custom provider form is still to be designed */}
      <Row class="provider" description={text().custom.note}
        title={providerName({ id: "custom", name: text().custom.title, auth: "custom", accounts: 0, enabled: true })}>
        <Button label={text().connect} icon="plus-small" />
      </Row>
    </Section>
    <Show when={menu()}>{opened => <ContextMenu x={opened().x} y={opened().y} label={text().menu} onClose={() => setMenu(null)}>
      <MenuItem icon="trash" label={text().remove} onClick={() => { setRemoving(opened().entry); setMenu(null); }} />
    </ContextMenu>}</Show>
    <Show when={removing()}>{entry => <ConfirmDialog title={text().removeTitle(entry().name)} description={text().removeDescription}
      action={text().removeConfirm} onConfirm={() => removeProvider(entry())} onClose={() => setRemoving(null)} />}</Show>
  </Page>}>{id => <ProviderPage id={id} onBack={() => setOpened(null)} />}</Show>;
}

function ProviderPage(props: { id: string; onBack: () => void }) {
  const text = () => tr().settings.providers.page;
  const entry = () => entryOf(props.id);
  const keys = () => entry()?.auth === "key";
  const accounts = () => accountsOf(props.id);
  const bars = () => usageOf(props.id);
  const busy = () => accounts().some(account => account.status.state === "checking");
  const [signingIn, setSigningIn] = createSignal(false);
  // Claude's sign-in page shows a code that is pasted back here
  const [code, setCode] = createSignal<string | null>(null);
  const [key, setKey] = createSignal("");
  watchProvider(props.id);
  onCleanup(onLoginFailed((provider, message) => {
    if (provider !== props.id) return;
    setSigningIn(false);
    showFailure(message);
  }));
  createEffect(on(() => accounts().length, () => setSigningIn(false), { defer: true }));

  async function signIn() {
    setSigningIn(true);
    const zone = Intl.DateTimeFormat().resolvedOptions();
    const started = await request("providerLogin", props.id === "zcode-account"
      ? { provider: props.id, language: zone.locale, timezone: zone.timeZone } : { provider: props.id })
      .catch(cause => { setSigningIn(false); showFailure(cause); });
    if (!started) return;
    if (props.id === "claude-account") {
      setSigningIn(false);
      setCode("");
    }
    await openUrl(started.url).catch(showFailure);
  }

  function finishSignIn() {
    if (!code()?.trim()) return;
    void request("providerLoginFinish", { provider: props.id, code: code()!.trim() }).then(() => setCode(null), showFailure);
  }

  async function importAuth() {
    const path = await open({ filters: [{ name: "auth.json", extensions: ["json"] }] }).catch(showFailure);
    if (typeof path === "string") await request("chatGptImportAuth", { path }).catch(showFailure);
  }

  function addKey() {
    if (!key().trim() || busy()) return;
    void request("providerAddKeys", { provider: props.id, keys: [key().trim()] }).then(() => setKey(""), showFailure);
  }

  const title = () => <span class="provider-heading">
    <IconButton name="arrow-left" label={text().back} size={16} width={24} onClick={props.onBack} />
    <Glyph name={providerIcon(props.id)} size={20} /><span>{entry()?.name}</span>
  </span>;
  return <Page title={title()} action={<Show when={entry()}>{current =>
    <Toggle label={tr().settings.providers.enabled(current().name)} on={current().enabled} onChange={on => setEnabled(current(), on)} />}</Show>}>
    <Show when={keys()} fallback={<Section title={text().signIn}>
      <Row title={text().browser.title} description={text().browser.description}>
        <Button label={signingIn() ? text().waiting : text().login} icon="plus-small" onClick={() => void signIn()} />
      </Row>
      <Show when={code() !== null}>
        <Row title={text().code.title} class="key-row">
          <TextBox label={text().code.title} placeholder={text().code.placeholder} value={code()!} class="grow" onInput={setCode} onEnter={finishSignIn} />
          <Button label={text().code.connect} disabled={!code()!.trim()} onClick={finishSignIn} />
        </Row>
      </Show>
      <Show when={props.id === "chatgpt-account"}>
        <Row title={text().importAuth.title} description={text().importAuth.description}>
          <Button label={text().chooseFile} onClick={() => void importAuth()} />
        </Row>
      </Show>
    </Section>}>
      <Section title={text().addKey} class="key-card">
        <TextBox label={text().keyPlaceholder} placeholder={text().keyPlaceholder} value={key()} class="grow" onInput={setKey} onEnter={addKey} />
        <Button label={text().add} icon="plus-small" disabled={!key().trim() || busy()} onClick={addKey} />
      </Section>
    </Show>
    <Show when={bars().length}>
      <Section title={text().usage} class="usage-card">
        <For each={bars()}>{bar => <UsageLine bar={bar} />}</For>
        <Show when={bars().some(bar => bar.partial)}><div class="usage-note">{text().partial}</div></Show>
        <Show when={props.id === "grok-account" && accounts().some(account => account.tier?.toLowerCase() === "free")}>
          <div class="usage-note">{text().grokEstimate}</div>
        </Show>
      </Section>
    </Show>
    <Section head={<h3 class="section-head">
      <span>{keys() ? text().keys(accounts().length) : text().accounts(accounts().length)}</span>
      <span class="section-tools">
        <Show when={props.id === "antigravity"}>
          <Button label={text().refresh} variant="ghost" disabled={busy() || !accounts().length} onClick={() => void request("antigravityRefresh").catch(showFailure)} />
        </Show>
        <Button label={busy() ? text().checking : text().checkAll} variant="ghost" disabled={busy() || !accounts().length}
          onClick={() => void request("providerCheckAll", { provider: props.id }).catch(showFailure)} />
        <RemoveAllButton disabled={!accounts().length} onConfirm={() => void request("providerRemoveAll", { provider: props.id }).catch(showFailure)} />
      </span>
    </h3>}>
      <For each={accounts()} fallback={<div class="settings-row empty-row">{keys() ? text().noKeys : text().noAccounts}</div>}>
        {account => <AccountRow account={account} provider={entry()?.name ?? props.id}
          onRemove={() => void request("providerRemove", { provider: props.id, email: account.email, id: account.id }).catch(showFailure)} />}
      </For>
    </Section>
    <Show when={props.id === "chatgpt-account"}>
      <Section title={text().options}>
        <OptionRow text={text().preferFree} load={() => request("chatGptPreferFree")} save={enabled => request("setChatGptPreferFree", { enabled })} />
      </Section>
    </Show>
    <Show when={props.id === "coarena"}>
      <Section title={text().options}>
        <OptionRow text={text().directChecks} load={() => request("coArenaDirectChecks")} save={enabled => request("setCoArenaDirectChecks", { enabled })} />
        <OptionRow text={text().experimentalContext} load={() => request("coArenaExperimentalContext")}
          save={enabled => request("setCoArenaExperimentalContext", { enabled })} />
      </Section>
    </Show>
  </Page>;
}

/** Email, plan and status of one account or key; a status with a message opens it on click. */
function AccountRow(props: { account: Account; provider: string; onRemove: () => void }) {
  const text = () => tr().settings.providers.page;
  const [message, setMessage] = createSignal<HTMLElement | null>(null);
  const [removing, setRemoving] = createSignal(false);
  const status = () => props.account.status;
  const tier = () => props.account.tier && props.account.tier.charAt(0).toUpperCase() + props.account.tier.slice(1);
  const credits = (value: number) => new Intl.NumberFormat(undefined, { maximumFractionDigits: 5 }).format(value);
  return <Row class="account-row" title={<span class="account-name">
    <span class="truncate" title={props.account.email}>{props.account.email || "…"}</span>
    <Show when={props.account.maxCredits !== undefined}><span class="tag">Max: {credits(props.account.maxCredits!)}</span></Show>
    <Show when={tier()}><span class="tag">{tier()}</span></Show>
  </span>}>
    <Show when={"message" in status()} fallback={<span class={`account-status ${status().state}`}>{text().status[status().state]}</span>}>
      <button type="button" class={`account-status with-message ${status().state}`}
        onClick={event => setMessage(message() ? null : event.currentTarget)}>{text().status[status().state]}</button>
    </Show>
    <IconButton name="trash" label={text().removeAccount} size={15} width={26} onClick={() => setRemoving(true)} />
    <Show when={message()}>{anchor => <Popover anchor={anchor()} title={props.account.email} onClose={() => setMessage(null)}>
      {"message" in status() ? (status() as { message: string }).message : ""}
    </Popover>}</Show>
    <Show when={removing()}><ConfirmDialog title={text().removeAccountTitle} description={text().removeAccountDescription(props.account.email, props.provider)}
      action={text().removeAccount} onConfirm={props.onRemove} onClose={() => setRemoving(false)} /></Show>
  </Row>;
}

/** Removing everything takes a second click within a few seconds. */
function RemoveAllButton(props: { disabled: boolean; onConfirm: () => void }) {
  const text = () => tr().settings.providers.page;
  const [confirming, setConfirming] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  function click() {
    clearTimeout(timer);
    if (confirming()) {
      setConfirming(false);
      return props.onConfirm();
    }
    setConfirming(true);
    timer = setTimeout(() => setConfirming(false), 3000);
  }
  return <Press label={text().removeAll} class="settings-button ghost" disabled={props.disabled} onClick={click}>
    <span>{confirming() ? text().removeAllConfirm : text().removeAll}</span>
  </Press>;
}

/** A provider option kept on the server: read once, saved on every switch. */
function OptionRow(props: { text: Text; load: () => Promise<boolean>; save: (enabled: boolean) => Promise<unknown> }) {
  const [on, setOn] = createSignal(false);
  void props.load().then(setOn, showFailure);
  function change(next: boolean) {
    setOn(next);
    props.save(next).catch(cause => { setOn(!next); showFailure(cause); });
  }
  return <ToggleRow text={props.text} on={on()} onChange={change} />;
}

export function ModelsPage() {
  const text = () => tr().settings.models;
  const [filter, setFilter] = createSignal("");
  // Older models of a provider get their own group after the current ones
  const groups = createMemo(() => {
    const query = filter().trim().toLowerCase();
    const found = serverModels().filter(model => `${model.provider.name} ${model.name} ${model.id}`.toLowerCase().includes(query));
    const providers = [...new Set(found.map(model => model.provider))];
    return providers.flatMap(provider => [false, true].map(older => ({
      title: older ? `${provider.name} · ${text().older}` : provider.name,
      icon: provider.icon,
      items: found.filter(model => model.provider === provider && !!model.older === older),
    }))).filter(group => group.items.length);
  });
  const shown = (items: Model[]) => items.some(model => !isHidden(model));
  const setShown = (items: Model[], on: boolean) => void setModelsHidden(items, !on).catch(showFailure);
  return <Page title={tr().settings.tabs.models} search={<FilterField value={filter()} placeholder={text().search} onInput={setFilter} />}>
    <For each={groups()}>{group => <Section head={<div class="models-head"><Glyph name={group.icon} size={20} /><span>{group.title}</span>
      <Toggle label={group.title} on={shown(group.items)} onChange={on => setShown(group.items, on)} /></div>}>
      <For each={group.items}>{model => <Row title={model.name} class="regular">
        <Toggle label={model.name} on={!isHidden(model)} onChange={on => setShown([model], on)} />
      </Row>}</For>
    </Section>}</For>
    <Show when={!groups().length}><Empty message={text().empty} /></Show>
  </Page>;
}
