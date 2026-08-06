import type { Conversation, ProviderGroup } from "./api";
import { el } from "./dom";
import { chooseModel, closePicker, state, togglePicker } from "./state";

// The trigger and the menu outlive every redraw, so a click that opens the
// menu and the redraw that follows do not race each other.
const wrap = el("div", "picker-wrap");
const trigger = el("button", "picker");
const menu = el("div", "picker-menu");
wrap.append(trigger);

// Focus sits on a real button, so Enter needs no handler of its own.
let choices: HTMLButtonElement[] = [];
let active = -1;
let opened = false;

trigger.addEventListener("click", togglePicker);

wrap.addEventListener("keydown", (event) => {
  if (!state.picker.open) return;
  const delta = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
  if (delta === 0) return;
  event.preventDefault();
  step(delta);
});

document.addEventListener("pointerdown", (event) => {
  if (event.target instanceof Node && wrap.contains(event.target)) return;
  closePicker();
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closePicker();
});

export function renderPicker(current: Conversation): HTMLElement {
  trigger.replaceChildren(
    el("span", "picker-provider", `${current.provider} / `),
    `${current.model === "" ? "no model" : current.model} ▾`,
  );
  if (state.picker.open !== opened) {
    opened = state.picker.open;
    active = -1;
  }
  if (!state.picker.open) {
    menu.remove();
    return wrap;
  }
  fill(current);
  wrap.append(menu);
  // A redraw rebuilds the item the keyboard was on, so the focus is put back.
  if (active !== -1) queueMicrotask(() => choices[active]?.focus());
  return wrap;
}

function fill(current: Conversation): void {
  choices = [];
  if (state.picker.loading) {
    menu.replaceChildren(el("div", "picker-loading", "checking providers…"));
    return;
  }
  menu.replaceChildren(
    ...state.picker.groups.map((group) => section(group, current)),
  );
}

function section(group: ProviderGroup, current: Conversation): HTMLElement {
  const box = el("div", "picker-group");
  box.append(head(group));
  for (const model of group.models) {
    const item = el("button", "picker-item");
    const chosen = group.name === current.provider && model.name === current.model;
    item.append(
      el("span", "picker-mark", chosen ? "●" : ""),
      el("span", "picker-name", model.name),
    );
    if (model.size_bytes !== null) {
      item.append(el("span", "picker-meta", size(model.size_bytes)));
    }
    // An unreachable provider still lists what it would serve, greyed out.
    item.disabled = !group.reachable;
    if (group.reachable) {
      item.addEventListener("click", () => void chooseModel(group.name, model.name));
      choices.push(item);
    }
    box.append(item);
  }
  return box;
}

function head(group: ProviderGroup): HTMLElement {
  const line = el("div", "picker-head", group.name);
  if (group.kind === "ollama") line.append(el("span", "picker-local", "local"));
  if (!group.reachable) line.append(el("span", "picker-offline", "· offline"));
  return line;
}

function step(delta: number): void {
  const count = choices.length;
  if (count === 0) return;
  active = active === -1 && delta < 0 ? count - 1 : (active + delta + count) % count;
  choices[active]?.focus();
}

// Ollama reports on-disk bytes and rounds them decimally in its own `list`, so
// the number here matches the one the terminal shows.
function size(bytes: number): string {
  if (bytes < 1e9) return `${Math.round(bytes / 1e6)}MB`;
  return `${(bytes / 1e9).toFixed(1)}GB`;
}
