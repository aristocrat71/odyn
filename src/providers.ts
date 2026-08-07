import type { CatalogItem, ProviderDraft, ProviderEntry } from "./api";
import { el } from "./dom";
import { dropdown } from "./dropdown";
import {
  cancelProviderEdit,
  chooseDefaultProvider,
  connectProvider,
  deleteProvider,
  openKeysPage,
  pickCatalogProvider,
  saveProvider,
  startProviderEdit,
  state,
} from "./state";

// The open form outlives redraws — the 30s status refresh must not eat a
// half-typed key — so it is rebuilt only when what it edits changes.
let formBox: HTMLElement | null = null;
let formFor: string | null | undefined;

export function renderProviders(): HTMLElement {
  const view = el("div", "providers");
  const entries = state.providers.entries;
  if (entries === null) {
    view.append(el("div", "prov-loading", "reading config…"));
    return view;
  }

  const editing = state.providers.editing;
  if (editing === null) {
    formBox = null;
    formFor = undefined;
  } else if (formBox === null || formFor !== editing.name) {
    const entry = entries.find((candidate) => candidate.name === editing.name) ?? null;
    formBox = form(entry);
    formFor = editing.name;
  }

  for (const entry of entries) {
    if (editing !== null && editing.name === entry.name && formBox !== null) {
      view.append(formBox);
    } else {
      view.append(row(entry));
    }
  }
  // The custom form and the connect panel edit the same file; only one of them
  // is on screen at a time.
  if (editing !== null && editing.name === null && formBox !== null) {
    view.append(formBox);
  } else {
    view.append(connectPanel(entries), customLink());
  }
  view.append(
    el(
      "div",
      "prov-note",
      "written to odyn.toml and adopted immediately · the file stays hand-editable",
    ),
  );
  return view;
}

function row(entry: ProviderEntry): HTMLElement {
  const line = el("div", "prov-row");
  const head = el("div", "prov-head");
  head.append(el("span", "prov-name", entry.name));
  if (entry.default) head.append(el("span", "prov-default", "default"));
  head.append(el("span", "prov-kind", entry.kind));

  const detail = el("div", "prov-detail");
  detail.append(el("span", "prov-url", entry.base_url));
  detail.append(key(entry));
  if (entry.default_model !== null) {
    detail.append(el("span", "prov-model", entry.default_model));
  }
  if (entry.keep_alive !== null) {
    detail.append(el("span", "prov-model", `keep ${entry.keep_alive}`));
  }

  const actions = el("div", "prov-actions");
  if (!entry.default) {
    const promote = el("button", "prov-act", "make default");
    promote.addEventListener("click", () => void chooseDefaultProvider(entry.name));
    actions.append(promote);
  }
  const edit = el("button", "prov-act", "✎");
  edit.setAttribute("aria-label", `edit ${entry.name}`);
  edit.addEventListener("click", () => startProviderEdit(entry.name));
  actions.append(edit);
  if (!entry.default) {
    const remove = el("button", "prov-act prov-remove", "✕");
    remove.setAttribute("aria-label", `remove ${entry.name}`);
    remove.addEventListener("click", () => void deleteProvider(entry.name));
    actions.append(remove);
  }

  line.append(head, detail, actions);
  return line;
}

// What the row can say about a key without ever holding one.
function key(entry: ProviderEntry): HTMLElement {
  if (entry.kind === "ollama") return el("span", "prov-key dim", "no key needed");
  if (entry.key_stored) return el("span", "prov-key ok", "● key in config");
  if (entry.key_env !== null) {
    const set = entry.key_env_set ? "ok" : "missing";
    const mark = entry.key_env_set ? "●" : "○";
    return el("span", `prov-key ${set}`, `${mark} env ${entry.key_env}`);
  }
  return el("span", "prov-key dim", "no key");
}

// ── connect ───────────────────────────────────────────────────────────────
//
// One field. The endpoint and the model list are already known for everything
// in the catalog, so a key is the whole of what a person has that Odyn does
// not. Built once and refilled in place: a redraw between the paste and the
// click must not take the key with it.

const panel = el("div", "conn");
const keyField = el("input", "conn-key");
const aim = el("div", "conn-aim");
const tiles = el("div", "conn-tiles");
const go = el("button", "conn-go");
const keysLink = el("button", "conn-keys", "get a key ↗");
const defaultBox = el("input");
const defaultLabel = el("label", "conn-default");
const notice = el("div", "conn-notice");

keyField.type = "password";
keyField.placeholder = "paste an api key";
keyField.setAttribute("aria-label", "api key");
keyField.autocomplete = "off";
keyField.spellcheck = false;
defaultBox.type = "checkbox";
defaultLabel.append(defaultBox, el("span", "conn-default-text", "use by default"));

// A key that names its own provider aims the panel as it is typed; one that
// names nobody leaves whatever tile was picked alone.
keyField.addEventListener("input", () => {
  const found = detect(keyField.value);
  if (found !== null) pickCatalogProvider(found.id);
  fill();
});
keyField.addEventListener("keydown", (event) => {
  if (event.key !== "Enter") return;
  event.preventDefault();
  submit();
});
go.addEventListener("click", submit);
keysLink.addEventListener("click", () => {
  const chosen = picked();
  if (chosen !== null) void openKeysPage(chosen.id);
});

{
  const foot = el("div", "conn-foot");
  foot.append(go, keysLink, defaultLabel);
  panel.append(
    el("div", "conn-head", "connect a provider"),
    keyField,
    aim,
    tiles,
    foot,
    notice,
  );
}

// Whether the box below is the user's answer or ours, so a redraw never
// re-ticks what they just cleared.
let defaultTouched = false;
defaultBox.addEventListener("change", () => (defaultTouched = true));

function connectPanel(entries: ProviderEntry[]): HTMLElement {
  // Nothing but a local Ollama yet: the first key is also the one that should
  // answer by default, so the box starts ticked and the choice stays visible.
  const first = entries.every((entry) => entry.kind === "ollama");
  if (first && !defaultTouched) defaultBox.checked = true;
  fill();
  return panel;
}

function picked(): CatalogItem | null {
  const catalog = state.providers.catalog;
  if (catalog === null) return null;
  return catalog.find((item) => item.id === state.providers.pick) ?? null;
}

/// The provider a pasted key belongs to, by the longest prefix that matches.
/// A shape half the industry issues belongs to nobody: guessing wrong costs
/// more than the click it saves.
function detect(text: string): CatalogItem | null {
  const value = text.trim();
  if (value === "") return null;
  let best: CatalogItem | null = null;
  let longest = 0;
  for (const item of state.providers.catalog ?? []) {
    for (const prefix of item.key_prefixes) {
      if (value.startsWith(prefix) && prefix.length > longest) {
        longest = prefix.length;
        best = item;
      }
    }
  }
  return best;
}

function fill(): void {
  const catalog = state.providers.catalog;
  const chosen = picked();
  const connecting = state.providers.connecting;

  tiles.replaceChildren(...(catalog ?? []).map((item) => tile(item, chosen)));

  const typed = keyField.value.trim() !== "";
  if (chosen === null) {
    aim.textContent = typed
      ? "that key is not one Odyn recognises — pick where it is from"
      : "paste a key, or pick a provider";
    aim.className = "conn-aim";
  } else {
    aim.textContent = `${chosen.label} · ${chosen.base_url}`;
    aim.className = "conn-aim on";
  }

  go.textContent = connecting
    ? "connecting…"
    : chosen === null
      ? "connect"
      : `connect ${chosen.id}`;
  go.disabled = connecting || chosen === null || (chosen.needs_key && !typed);
  keysLink.hidden = chosen === null || !chosen.needs_key;
  keyField.disabled = connecting;
  keyField.placeholder =
    chosen !== null && !chosen.needs_key ? "no key needed" : "paste an api key";

  const said = state.providers.connected;
  notice.textContent = said ?? "";
  notice.hidden = said === null;
}

function tile(item: CatalogItem, chosen: CatalogItem | null): HTMLElement {
  const button = el("button", "conn-tile", item.label);
  if (item.id === chosen?.id) button.classList.add("on");
  // Already in the file: connecting again is how a rotated key gets in, so the
  // tile stays live and only says what it already is.
  if (item.configured) button.append(el("span", "conn-have", "●"));
  if (!item.needs_key) button.append(el("span", "conn-local", "local"));
  button.disabled = state.providers.connecting;
  button.addEventListener("click", () => {
    // A local endpoint has nothing left to ask for.
    if (!item.needs_key) {
      run(item, "");
      return;
    }
    pickCatalogProvider(item.id);
    keyField.focus();
  });
  return button;
}

function submit(): void {
  const chosen = picked();
  if (chosen === null) return;
  const value = keyField.value.trim();
  if (chosen.needs_key && value === "") return;
  run(chosen, value);
}

function run(item: CatalogItem, apiKey: string): void {
  if (state.providers.connecting) return;
  void connectProvider(item.id, apiKey, defaultBox.checked).then(() => {
    // The field empties only once the key it held has reached the config file:
    // a key the endpoint refused stays where it is, to be fixed rather than
    // found and pasted a second time, and a local endpoint that wanted no key
    // has no business clearing one meant for somewhere else.
    if (state.providers.connected === null) return;
    if (apiKey !== "") keyField.value = "";
    defaultBox.checked = false;
    defaultTouched = false;
    fill();
  });
}

function customLink(): HTMLElement {
  const link = el("button", "prov-add", "+ custom endpoint");
  link.addEventListener("click", () => startProviderEdit(null));
  return link;
}

function form(entry: ProviderEntry | null): HTMLElement {
  const box = el("div", "prov-form");
  const adding = entry === null;

  const name = field("name", "zen");
  name.value = entry?.name ?? "";
  name.disabled = !adding;

  const kind = dropdown({ onPick: () => refill() });
  kind.set(
    [
      { value: "openai_compat", hint: "api endpoint" },
      { value: "ollama", hint: "local models" },
    ],
    entry?.kind ?? "openai_compat",
  );

  const url = field("base url", "https://opencode.ai/zen/v1");
  url.value = entry?.base_url ?? "";

  const apiKey = field("api key", entry?.key_stored ? "unchanged" : "sk-…");
  apiKey.type = "password";
  const keyEnv = field("or env var", "OPENCODE_API_KEY");
  keyEnv.value = entry?.key_env ?? "";
  const model = field("default model", "kimi-k3");
  model.value = entry?.default_model ?? "";
  const keepAlive = field("keep alive", "5m");
  keepAlive.value = entry?.keep_alive ?? "";

  const makeDefault = el("input");
  makeDefault.type = "checkbox";
  makeDefault.checked = entry?.default ?? false;
  makeDefault.disabled = entry?.default ?? false;

  const rows = el("div", "prov-fields");
  const line = (label: string, ...controls: HTMLElement[]): HTMLElement => {
    const row = el("label", "prov-field");
    row.append(el("span", "prov-label", label), ...controls);
    return row;
  };
  const openaiRows = [
    line("api key", apiKey),
    line("key env", keyEnv),
    line("model", model),
  ];
  const ollamaRows = [line("keep alive", keepAlive)];

  const refill = (): void => {
    const openai = kind.value() === "openai_compat";
    rows.replaceChildren(
      line("name", name),
      line("kind", kind.root),
      line("base url", url),
      ...(openai ? openaiRows : ollamaRows),
      line("default", makeDefault),
    );
  };
  refill();

  const save = el("button", "prov-act prov-save", "save");
  save.addEventListener("click", () => {
    const draft: ProviderDraft = {
      name: name.value.trim(),
      kind: kind.value(),
      base_url: url.value.trim(),
      api_key: apiKey.value,
      api_key_env: keyEnv.value,
      default_model: model.value,
      keep_alive: keepAlive.value,
      make_default: makeDefault.checked && !(entry?.default ?? false),
    };
    void saveProvider(draft);
  });
  const cancel = el("button", "prov-act", "cancel");
  cancel.addEventListener("click", cancelProviderEdit);
  const actions = el("div", "prov-form-actions");
  actions.append(save, cancel);

  box.append(rows, actions);
  queueMicrotask(() => (adding ? name : url).focus());
  return box;
}

function field(label: string, hint: string): HTMLInputElement {
  const input = el("input", "prov-input");
  input.placeholder = hint;
  input.setAttribute("aria-label", label);
  return input;
}
