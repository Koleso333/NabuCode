import { createSignal, Match, Show, Switch } from "solid-js";
import { Press, Ring } from "./controls";
import { tr } from "./i18n";
import { connection, restartServer } from "./server";

export function ServerBanner() {
  const [details, setDetails] = createSignal(false);
  const failure = () => {
    const current = connection();
    return current.state === "failed" ? current.message : undefined;
  };
  const restart = () => {
    setDetails(false);
    restartServer();
  };
  return <Switch>
    <Match when={connection().state === "reconnecting"}>
      <div class="server-toast panel-frame"><Ring fraction={.3} size={14} loading /><span>{tr().server.reconnecting}</span></div>
    </Match>
    <Match when={failure()}>{message => <div class="server-toast failed panel-frame">
      <span class="server-failed-dot" /><span>{tr().server.failed}</span>
      <Press label={tr().server.details} onClick={() => setDetails(!details())} selected={details()}>{tr().server.details}</Press>
      <Press label={tr().server.restart} variant="primary" onClick={restart}>{tr().server.restart}</Press>
      <Show when={details()}><div class="server-details panel-frame">{message()}</div></Show>
    </div>}</Match>
  </Switch>;
}
