import { createSignal } from "solid-js";
import type { Project as WireProject } from "@nabu/protocol";
import { onConnected, onServerEvent, request } from "./server";
import { serverPath } from "./chat/remote";
import type { Project } from "./data";

// The project list lives on the server; branches come with it and change when git switches outside NabuCode
const [serverProjects, setServerProjects] = createSignal<Project[]>([]);
export { serverProjects };

export const samePath = (a: string, b: string) => serverPath(a).toLowerCase() === serverPath(b).toLowerCase();

const toProject = (project: WireProject): Project => ({ path: project.path, name: project.name, branch: project.branch ?? "" });
const show = (list: WireProject[]) => setServerProjects(list.map(toProject));

export const addProject = (path: string, name: string) => request("projectAdd", { path: serverPath(path), name }).then(show);
export const renameProject = (path: string, name: string) => request("projectRename", { path, name }).then(show);
export const removeProject = (path: string) => request("projectRemove", { path }).then(show);

// Shown in the new order at once, so a dropped row doesn't jump back while the server saves
export function reorderProjects(paths: string[]) {
  const rank = (project: Project) => paths.indexOf(project.path);
  setServerProjects([...serverProjects()].sort((a, b) => rank(a) - rank(b)));
  return request("projectReorder", { paths }).then(show);
}
export const listBranches = (directory: string) => request("gitBranches", { directory });
export const switchBranch = (directory: string, branch: string) => request("gitSwitch", { directory, branch });

// Projects added before the list moved to the server go there once
async function moveLocalProjects() {
  const key = "everyend-projects";
  let saved: { path?: unknown; name?: unknown }[] = [];
  try { saved = JSON.parse(localStorage.getItem(key) || "[]"); } catch { /* unreadable: nothing to move */ }
  for (const item of Array.isArray(saved) ? saved : []) {
    if (typeof item?.path === "string" && typeof item?.name === "string") await addProject(item.path, item.name);
  }
  localStorage.removeItem(key);
}

let markLoaded = () => {};
// Settles once the first list has come, so the start screen doesn't leave before the project is known
export const projectsLoaded = new Promise<void>(resolve => { markLoaded = resolve; });

async function load() {
  await moveLocalProjects().catch(() => {});
  const list = await request("projectList").catch(() => undefined);
  if (list) show(list);
  markLoaded();
}

onConnected(() => void load());
onServerEvent(event => { if (event.type === "projectsChanged") show(event.projects); });
