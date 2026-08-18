import type { Conversation, ProviderGroup } from "./api";
import { el } from "./dom";
import {
  chooseModel,
  closePicker,
  state,
  togglePicker,
  type PickerMenu,
} from "./state";

// Two menus, not one: a combined catalog is too long to find a model in.
const provider = control("provider");
const model = control("model");

// Focus sits on a real button, so Enter needs no handler of its own.
let choices: HTMLButtonElement[] = [];
let active = -1;
let opened: PickerMenu = null;

document.addEventListener("pointerdown", (event) => {
  if (!(event.target instanceof Node)) return;
  if (provider.wrap.contains(event.target) || model.wrap.contains(event.target)) return;
  closePicker();
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closePicker();
});

export function renderPickers(current: Conversation): HTMLElement[] {
  provider.trigger.replaceChildren(
    el("span", "picker-provider", "provider "),
    `${current.provider} ▾`,
  );
  model.trigger.replaceChildren(
    el("span", "picker-provider", "model "),
    `${current.model === "" ? "no model" : current.model} ▾`,
  );
  if (state.picker.open !== opened) {
    opened = state.picker.open;
    active = -1;
  }
  choices = [];
  provider.menu.remove();
  model.menu.remove();
  if (state.picker.open === "provider") {
    fillProviders(current);
    provider.wrap.append(provider.menu);
  }
  if (state.picker.open === "model") {
    fillModels(current);
    model.wrap.append(model.menu);
  }
  // A redraw rebuilds the item the keyboard was on, so the focus is put back.
  if (active !== -1) queueMicrotask(() => choices[active]?.focus());
  return [provider.wrap, model.wrap];
}

// Trigger and menu outlive redraws, so a click and its redraw do not race.
function control(which: "provider" | "model") {
  const wrap = el("div", "picker-wrap");
  const trigger = el("button", "picker");
  const menu = el("div", `picker-menu ${which}-menu`);
  wrap.append(trigger);
  trigger.addEventListener("click", () => togglePicker(which));
  wrap.addEventListener("keydown", (event) => {
    if (state.picker.open === null) return;
    const delta = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
    if (delta === 0) return;
    event.preventDefault();
    step(delta);
  });
  return { wrap, trigger, menu };
}

// A provider that is down is labelled `offline` rather than hidden: a menu
// that hides what is down explains nothing.
function fillProviders(current: Conversation): void {
  if (state.picker.loading) {
    provider.menu.replaceChildren(el("div", "picker-loading", "checking providers…"));
    return;
  }
  provider.menu.replaceChildren(
    ...state.picker.groups.map((group) => {
      const item = el("button", "picker-item");
      item.append(
        el("span", "picker-mark", group.name === current.provider ? "●" : ""),
        el("span", "picker-name", group.name),
      );
      if (group.kind === "ollama") item.append(el("span", "picker-local", "local"));
      if (!group.reachable) item.append(el("span", "picker-meta", "offline"));
      item.addEventListener("click", () => void switchProvider(group, current));
      choices.push(item);
      return item;
    }),
  );
}

function fillModels(current: Conversation): void {
  if (state.picker.loading) {
    model.menu.replaceChildren(el("div", "picker-loading", "checking providers…"));
    return;
  }
  const group = state.picker.groups.find((row) => row.name === current.provider);
  if (group === undefined) {
    const note = `${current.provider} is not configured`;
    model.menu.replaceChildren(el("div", "picker-loading", note));
    return;
  }
  if (group.models.length === 0) {
    const note = group.reachable ? "no models" : `${group.name} is offline`;
    model.menu.replaceChildren(el("div", "picker-loading", note));
    return;
  }
  model.menu.replaceChildren(
    ...group.models.map((row) => {
      const item = el("button", "picker-item");
      item.append(
        el("span", "picker-mark", row.name === current.model ? "●" : ""),
        el("span", "picker-name", row.name),
      );
      // Shown, never hidden: the memory mentions need a tool-calling model.
      if (row.tools === false) {
        item.append(el("span", "picker-meta", "no tools"));
      }
      if (row.size_bytes !== null) {
        item.append(el("span", "picker-meta", size(row.size_bytes)));
      }
      // An unreachable provider still lists what it would serve, greyed out.
      item.disabled = !group.reachable;
      if (group.reachable) {
        item.addEventListener("click", () => void chooseModel(group.name, row.name));
        choices.push(item);
      }
      return item;
    }),
  );
}

// The model is kept when the new provider serves it too, so a change of
// endpoint is not silently a change of model.
function switchProvider(group: ProviderGroup, current: Conversation): Promise<void> {
  const names = group.models.map((row) => row.name);
  const kept = names.includes(current.model) ? current.model : (names[0] ?? "");
  return chooseModel(group.name, kept);
}

function step(delta: number): void {
  const count = choices.length;
  if (count === 0) return;
  active = active === -1 && delta < 0 ? count - 1 : (active + delta + count) % count;
  choices[active]?.focus();
}

// Ollama rounds decimally in its own `list`, so this matches the terminal.
function size(bytes: number): string {
  if (bytes < 1e9) return `${Math.round(bytes / 1e6)}MB`;
  return `${(bytes / 1e9).toFixed(1)}GB`;
}
