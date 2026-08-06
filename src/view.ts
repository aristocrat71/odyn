import type { Conversation } from "./api";
import { renderBrain } from "./brain";
import { renderBrevity } from "./brevctl";
import { renderChat } from "./chat";
import { renderConfig } from "./config";
import { el } from "./dom";
import { renderHome } from "./home";
import { renderPicker } from "./picker";
import { renderProviders } from "./providers";
import { state } from "./state";

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

  left.append(
    el("h1", "title", state.view === "chat" && current ? current.title : state.view),
  );
  const crumbs = crumbLine(current);
  if (crumbs !== "") left.append(el("div", "crumbs", crumbs));

  bar.append(left);
  // The picker sets the model of a conversation, so it exists only where one
  // is; the brevity control sits beside it and follows the same rule.
  if (state.view === "chat" && current) {
    const controls = el("div", "topbar-controls");
    controls.append(renderBrevity(current), renderPicker(current));
    bar.append(controls);
  }
  return bar;
}

function body(): HTMLElement {
  const view = el("section", "view");
  if (state.view === "home") view.append(renderHome());
  if (state.view === "chat") view.append(renderChat());
  if (state.view === "brain") view.append(renderBrain());
  if (state.view === "providers") view.append(renderProviders());
  if (state.view === "config") view.append(renderConfig());
  if (state.view === "guide") view.append(guide());
  return view;
}

// The guide is a chunk of its own — nothing it contains is in the main bundle
// until the view is first opened, which is why it is reached by `import()` and
// never by a static import. The promise is cached, not just the module, so a
// redraw while the chunk is in flight does not start a second load.
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
  // A view switched away from leaves `box` detached, and writing to it is a
  // no-op — so nothing has to be cancelled.
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

// Nothing to say about an empty conversation, so it says nothing. Token counts
// come from the provider, and not every provider reports them.
function crumbLine(current: Conversation | undefined): string {
  if (state.view !== "chat" || current === undefined || state.turns === 0) return "";
  const turns = state.turns === 1 ? "1 turn" : `${state.turns} turns`;
  if (state.tokens === null) return turns;
  return `${turns} · ${(state.tokens / 1000).toFixed(1)}k tokens`;
}
