import type { Conversation } from "./api";
import { renderBrain } from "./brain";
import { refreshLedger, renderChat } from "./chat";
import { renderConfig } from "./config";
import { renderConversations } from "./conversations";
import { el } from "./dom";
import { renderHome } from "./home";
import { renderProviders } from "./providers";
import { renderReminders } from "./reminders";
import {
  closeWorkspacePopover,
  commitWorkspace,
  state,
  toggleWorkspacePopover,
} from "./state";

export function renderView(root: HTMLElement): void {
  const parts: HTMLElement[] = [topbar()];
  if (state.error !== "") parts.push(el("div", "error", state.error));
  parts.push(body());
  // Built before it is swapped in, so the outgoing view can still be measured.
  root.replaceChildren(...parts);
}

function topbar(): HTMLElement {
  const bar = el("header", "topbar");
  const left = el("div", "topbar-left");
  const current = selected();

  const head = el(
    "h1",
    "title",
    state.view === "chat" && current ? current.title : state.view,
  );
  const count = state.brain.overview?.count;
  if (state.view === "brain" && count !== undefined) {
    head.append(" ", el("span", "title-note", `${count} memories`));
  }
  left.append(head);
  const crumbs = crumbLine(current);
  if (crumbs !== "") left.append(el("div", "crumbs", crumbs));

  bar.append(left);
  if (state.view === "chat" && current !== undefined) {
    bar.append(workspaceControl(current));
  }
  return bar;
}

// The popover input outlives redraws, so typing survives the 30s rerender.
const wsInput = el("input", "workspace-input");
wsInput.placeholder = "~/path/to/folder";
wsInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    void commitWorkspace(wsInput.value).then(refreshLedger);
  }
  if (event.key === "Escape") closeWorkspacePopover();
});
let popWasOpen = false;

// `⚒ notes ✕` when set; empty conversations without one get a quiet
// affordance instead. The popover takes a typed path — no dialogs.
function workspaceControl(current: Conversation): HTMLElement {
  const box = el("div", "workspace");
  if (current.workspace !== null) {
    const chip = el("button", "workspace-chip");
    chip.title = current.workspace;
    chip.append(el("span", "workspace-mark", "⚒ "), shortPath(current.workspace));
    chip.addEventListener("click", toggleWorkspacePopover);
    const clear = el("button", "workspace-clear", "✕");
    clear.title = "clear the workspace";
    clear.addEventListener("click", () => void commitWorkspace("").then(refreshLedger));
    box.append(chip, clear);
  } else if (state.messages.length === 0) {
    const set = el("button", "workspace-set", "⚒ set a workspace");
    set.addEventListener("click", toggleWorkspacePopover);
    box.append(set);
  }
  if (state.workspacePopover) {
    if (!popWasOpen) {
      wsInput.value = current.workspace ?? "";
      queueMicrotask(() => wsInput.focus());
    }
    const pop = el("div", "workspace-pop");
    pop.append(
      wsInput,
      el("div", "workspace-hint", "a folder the agent may work in · ⏎ set · empty clears"),
    );
    box.append(pop);
  }
  popWasOpen = state.workspacePopover;
  return box;
}

function shortPath(path: string): string {
  const parts = path.split("/").filter((part) => part !== "");
  return parts[parts.length - 1] ?? path;
}

function body(): HTMLElement {
  const view = el("section", "view");
  if (state.view === "home") view.append(renderHome());
  if (state.view === "chat") view.append(renderChat());
  if (state.view === "conversations") view.append(renderConversations());
  if (state.view === "brain") view.append(renderBrain());
  if (state.view === "reminders") view.append(renderReminders());
  if (state.view === "providers") view.append(renderProviders());
  if (state.view === "config") view.append(renderConfig());
  if (state.view === "guide") view.append(guide());
  return view;
}

// The guide is its own chunk, reached by `import()`. The promise is cached,
// not just the module, so a redraw mid-flight does not start a second load.
let loaded: typeof import("./guide") | null = null;
let loading: Promise<typeof import("./guide")> | null = null;

function guide(): HTMLElement {
  const box = el("div", "guide-view");
  if (loaded !== null) {
    box.append(loaded.renderGuide());
    return box;
  }
  box.append(el("div", "guide-loading", "loading guide…"));
  if (loading === null) loading = import("./guide");
  // A view switched away leaves `box` detached; writing to it is a no-op.
  void loading.then(
    (module) => {
      loaded = module;
      box.replaceChildren(module.renderGuide());
    },
    (err: unknown) => {
      box.replaceChildren(el("div", "guide-error", `guide failed to load: ${String(err)}`));
    },
  );
  return box;
}

function selected(): Conversation | undefined {
  return state.conversations.find((row) => row.id === state.selected);
}

// Token counts come from the provider, and not every provider reports them.
function crumbLine(current: Conversation | undefined): string {
  if (state.view !== "chat" || current === undefined || state.turns === 0) return "";
  const turns = state.turns === 1 ? "1 turn" : `${state.turns} turns`;
  if (state.tokens === null) return turns;
  return `${turns} · ${(state.tokens / 1000).toFixed(1)}k tokens`;
}
