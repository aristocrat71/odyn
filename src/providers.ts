import type { ProviderDraft, ProviderEntry } from "./api";
import { el } from "./dom";
import {
  cancelProviderEdit,
  chooseDefaultProvider,
  deleteProvider,
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
  if (editing !== null && editing.name === null && formBox !== null) {
    view.append(formBox);
  } else {
    view.append(addLink());
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

function addLink(): HTMLElement {
  const link = el("button", "prov-add", "+ add provider");
  link.addEventListener("click", () => startProviderEdit(null));
  return link;
}

function form(entry: ProviderEntry | null): HTMLElement {
  const box = el("div", "prov-form");
  const adding = entry === null;

  const name = field("name", "zen");
  name.value = entry?.name ?? "";
  name.disabled = !adding;

  const kind = el("select", "prov-input");
  for (const option of ["openai_compat", "ollama"]) {
    const item = el("option", undefined, option);
    item.value = option;
    kind.append(item);
  }
  kind.value = entry?.kind ?? "openai_compat";

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
    const openai = kind.value === "openai_compat";
    rows.replaceChildren(
      line("name", name),
      line("kind", kind),
      line("base url", url),
      ...(openai ? openaiRows : ollamaRows),
      line("default", makeDefault),
    );
  };
  kind.addEventListener("change", refill);
  refill();

  const save = el("button", "prov-act prov-save", "save");
  save.addEventListener("click", () => {
    const draft: ProviderDraft = {
      name: name.value.trim(),
      kind: kind.value,
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
