import { createSignal } from "solid-js";
import { onConnected, onServerEvent, request } from "./server";
import { nameModels } from "./chat/remote";
import type { Model } from "./data";
import { providerIcons } from "./provider-icons";

// The models of the server's providers, in the shape the composer lists
// An unchanged list after a reload is not set again, so rows are not rebuilt
const [serverModels, setServerModels] = createSignal<Model[]>([], { equals: (a, b) => JSON.stringify(a) === JSON.stringify(b) });
const [hidden, setHidden] = createSignal(new Set<string>());
export { serverModels };

const keyOf = (providerID: string, modelID: string) => `${providerID}\n${modelID}`;

export const providerIcon = (id: string) => `providers/${providerIcons.has(id) ? id : "synthetic"}`;

/** Switched off in the models list: not offered in the composer. */
export const isHidden = (model: Model) => hidden().has(keyOf(model.provider.id, model.id));

export function setModelsHidden(list: Model[], hide: boolean) {
  const next = new Set(hidden());
  for (const model of list) next[hide ? "add" : "delete"](keyOf(model.provider.id, model.id));
  setHidden(next);
  return request("setModelsHidden", { models: list.map(model => ({ providerID: model.provider.id, modelID: model.id })), hidden: hide });
}

async function load() {
  const [providers, hiddenModels] = await Promise.all([request("providers"), request("hiddenModels")]).catch(() => []);
  if (!providers || !hiddenModels) return;
  setHidden(new Set(hiddenModels.map(model => keyOf(model.providerID, model.modelID))));
  setServerModels(providers.flatMap(provider => {
    const owner = { id: provider.id, name: provider.name, icon: providerIcon(provider.id) };
    return provider.models.map(model => ({
      id: model.id, name: model.name, provider: owner, efforts: model.efforts, fast: model.fast, context: model.context, images: model.images,
      older: model.older,
    }));
  }));
}

onConnected(() => void load());
onServerEvent(event => { if (event.type === "providersChanged") void load(); });
nameModels(ref => serverModels().find(model => model.id === ref.modelID && model.provider.id === ref.providerID)?.name);
