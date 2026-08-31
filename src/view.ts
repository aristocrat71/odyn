import { renderBrain } from "./brain";
import { renderConfig } from "./config";
import { el } from "./dom";
import { renderProviders } from "./providers";
import { renderReminders } from "./reminders";
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
  const head = el("h1", "title", state.view);
  const count = state.brain.overview?.count;
  if (state.view === "brain" && count !== undefined) {
    head.append(" ", el("span", "title-note", `${count} memories`));
  }
  left.append(head);
  bar.append(left);
  return bar;
}

function body(): HTMLElement {
  const view = el("section", "view");
  if (state.view === "brain") view.append(renderBrain());
  if (state.view === "reminders") view.append(renderReminders());
  if (state.view === "providers") view.append(renderProviders());
  if (state.view === "config") view.append(renderConfig());
  return view;
}
