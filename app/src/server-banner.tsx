import { protocolVersion } from "@nabu/protocol";
import { createSignal, Match, Show, Switch } from "solid-js";
import { Press, Ring } from "./controls";
import { tr } from "./i18n";
import { connection, restartServer } from "./server";

type Problem = { title: string; details: string; action: string; run: () => void };

export function ServerBanner() {
  const [details, setDetails] = createSignal(false);
  const problem = (): Problem | undefined => {
    const current = connection();
    if (current.state === "failed") return { title: tr().server.failed, details: current.message, action: tr().server.restart, run: restartServer };
    if (current.state === "mismatch") {
      const details = tr().server.mismatchDetails(protocolVersion, current.server);
      return { title: tr().server.mismatch, details, action: tr().server.retry, run: restartServer };
    }
  };
  const act = (run: () => void) => {
    setDetails(false);
    run();
  };
  return <Switch>
    <Match when={connection().state === "reconnecting"}>
      <div class="server-toast panel-frame"><Ring fraction={.3} size={14} loading /><span>{tr().server.reconnecting}</span></div>
    </Match>
    <Match when={problem()}>{current => <div class="server-toast failed panel-frame">
      <span class="server-failed-dot" /><span>{current().title}</span>
      <Press label={tr().server.details} onClick={() => setDetails(!details())} selected={details()}>{tr().server.details}</Press>
      <Press label={current().action} variant="primary" onClick={() => act(current().run)}>{current().action}</Press>
      <Show when={details()}><div class="server-details panel-frame">{current().details}</div></Show>
    </div>}</Match>
  </Switch>;
}
