import type { MemoryRow } from "./api";
import { el } from "./dom";
import { dropdown } from "./dropdown";
import { renderGraph } from "./graph";
import {
  cancelMemoryEdit,
  chooseEmbedModel,
  commitMemoryEdit,
  loadMoreMemories,
  removeMemory,
  scheduleBrainSearch,
  setBrainMode,
  setBrainSort,
  startMemoryEdit,
  state,
} from "./state";

// How close to the bottom (px) still counts as reaching it.
const NEAR = 160;
const INJECTED_TAG_S = 5 * 60;
const SORTS = ["recent", "hits", "created"] as const;

// Persistent inputs: a search being typed and an edit in progress survive the
// redraws background refreshes cause.
const search = el("input", "brain-search");
search.placeholder = "search memories…";
search.addEventListener("input", () => scheduleBrainSearch(search.value));

const editor = el("input", "mem-input");
let editingShown: string | null = null;
editor.addEventListener("keydown", (event) => {
  if (event.key === "Enter") void commitMemoryEdit(editor.value);
  if (event.key === "Escape") cancelMemoryEdit();
});

export function renderBrain(): HTMLElement {
  const root = el("div", "brain");
  root.append(header());
  if (state.brain.mode === "graph") {
    root.append(renderGraph());
    return root;
  }
  root.append(listColumn());
  root.addEventListener("scroll", () => {
    if (root.scrollHeight - root.scrollTop - root.clientHeight < NEAR) {
      void loadMoreMemories();
    }
  });
  return root;
}

const models = dropdown({
  label: "model",
  empty: "…",
  onPick: (value) => void chooseEmbedModel(value),
});

function header(): HTMLElement {
  const row = el("div", "brain-header");
  const overview = state.brain.overview;
  const stats =
    overview === null
      ? ""
      : `${overview.count} memories · top-k ${overview.top_k} · ` +
        `cap ${overview.cap_tokens} tk`;
  const left = el("div", "brain-stats", stats);
  if (overview !== null) left.title = overview.path;
  left.append(" ", modelPicker());
  row.append(left);
  const toggle = el("div", "brain-toggle");
  for (const mode of ["list", "graph"] as const) {
    const word = el("button", "brain-mode", mode);
    if (state.brain.mode === mode) word.classList.add("active");
    word.addEventListener("click", () => setBrainMode(mode));
    toggle.append(word);
    if (mode === "list") toggle.append(el("span", "brain-mode-sep", " / "));
  }
  row.append(toggle);
  return row;
}

// Changing the model re-embeds every note, so the control says so while that
// runs. A remote model is marked: only that kind sends note text off-machine.
function modelPicker(): HTMLElement {
  const wrap = el("span", "brain-model");
  const overview = state.brain.overview;
  if (state.brain.swapping) {
    wrap.append(el("span", "brain-reindex", "re-embedding every note…"));
    return wrap;
  }
  const options = state.brain.models;
  const current = overview?.model ?? "";
  models.set(
    (options ?? []).map((option) => ({
      value: option.id,
      label: option.id,
      hint:
        (option.dim === null ? "" : `${option.dim}d · `) +
        (option.remote ? `⚠ ${option.description}` : option.description),
    })),
    current,
  );
  models.setDisabled(options === null);
  wrap.append(models.root);
  if (overview !== null && overview.dim > 0) {
    wrap.append(el("span", "brain-dim", `${overview.dim}d`));
  }
  if (overview?.model_remote === true) {
    const warn = el("span", "brain-remote", "⚠ notes are sent to this provider");
    wrap.append(warn);
  }
  return wrap;
}

function listColumn(): HTMLElement {
  const column = el("div", "brain-col brain-list");
  const overview = state.brain.overview;
  column.append(
    el("div", "col-label epi", "MEMORIES — recalled on /brain"),
  );
  if (overview !== null) {
    column.append(el("div", "brain-path", overview.path));
  }

  const toolbar = el("div", "brain-toolbar");
  const sort = el("button", "sort-toggle", `${state.brain.sort} ▾`);
  sort.addEventListener("click", () => {
    const next = SORTS[(SORTS.indexOf(state.brain.sort) + 1) % SORTS.length];
    void setBrainSort(next);
  });
  toolbar.append(search, sort);
  column.append(toolbar);

  const rows = state.brain.results ?? state.brain.memories;
  for (const row of rows) {
    column.append(state.brain.editing === row.slug ? editRow(row) : memRow(row));
  }
  if (rows.length === 0 && state.brain.results !== null) {
    column.append(el("div", "brain-none", "nothing similar"));
  }
  if (state.brain.editing === "new") {
    column.append(editRow(null));
  } else {
    const add = el("button", "ghost-link", "+ add a note");
    add.addEventListener("click", () => startMemoryEdit("new"));
    column.append(add);
  }
  return column;
}

function memRow(row: MemoryRow): HTMLElement {
  const line = el("div", "mem-row");
  line.append(
    el("span", "mem-id epi", row.slug),
    el("span", "mem-content", flat(row.content)),
  );
  const now = Math.floor(Date.now() / 1000);
  if (row.last_injected_at !== null && now - row.last_injected_at <= INJECTED_TAG_S) {
    const minutes = Math.max(0, Math.floor((now - row.last_injected_at) / 60));
    line.append(el("span", "mem-tag", `injected ${minutes}m ago`));
  }
  const meta = row.hits > 0 ? `${row.hits} hits` : date(row.created_at);
  line.append(el("span", "mem-meta", meta));
  const actions = el("span", "mem-actions");
  const edit = el("button", "mem-edit", "✎");
  edit.addEventListener("click", () => startMemoryEdit(row.slug));
  const remove = el("button", "mem-delete", "✕");
  remove.addEventListener("click", () => void removeMemory(row.slug));
  actions.append(edit, remove);
  line.append(actions);
  return line;
}

// The row becomes an input in place; `null` is the add-new one.
function editRow(row: MemoryRow | null): HTMLElement {
  const line = el("div", "mem-row editing");
  if (row !== null) line.append(el("span", "mem-id epi", row.slug));
  if (editingShown !== state.brain.editing) {
    editingShown = state.brain.editing;
    editor.value = row?.content ?? "";
    queueMicrotask(() => editor.focus());
  }
  line.append(editor);
  return line;
}

// A multi-line note flattens to one row; the graph tip shows it whole.
const flat = (content: string): string => content.split("\n").join(" ⏎ ");

const date = (seconds: number): string =>
  new Date(seconds * 1000).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
  });
