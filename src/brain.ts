import type { MemoryRow } from "./api";
import { el } from "./dom";
import { renderGraph } from "./graph";
import {
  cancelMemoryEdit,
  commitMemoryEdit,
  loadMoreEpisodic,
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
// redraws that background refreshes cause.
const search = el("input", "brain-search");
search.placeholder = "search memories…";
search.addEventListener("input", () => scheduleBrainSearch(search.value));

const editor = el("input", "mem-input");
let editingShown: number | "new" | null = null;
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
  const columns = el("div", "brain-columns");
  columns.append(coreColumn(), episodicColumn());
  root.append(columns);
  root.addEventListener("scroll", () => {
    if (root.scrollHeight - root.scrollTop - root.clientHeight < NEAR) {
      void loadMoreEpisodic();
    }
  });
  return root;
}

function header(): HTMLElement {
  const row = el("div", "brain-header");
  const overview = state.brain.overview;
  const stats =
    overview === null
      ? ""
      : `${overview.episodic_count} episodic · ${overview.model} · ` +
        `top-k ${overview.top_k} · cap ${overview.cap_tokens} tk`;
  row.append(el("div", "brain-stats", stats));
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

function coreColumn(): HTMLElement {
  const column = el("div", "brain-col brain-core");
  column.append(el("div", "col-label core", "CORE PROFILE — always injected"));
  const overview = state.brain.overview;
  if (overview !== null) column.append(budget(overview.core_tokens, overview.core_budget_tokens));
  for (const row of overview?.core ?? []) {
    column.append(state.brain.editing === row.id ? editRow(row) : coreRow(row));
  }
  if (state.brain.editing === "new") {
    column.append(editRow(null));
  } else {
    const add = el("button", "ghost-link", "+ add core memory");
    add.addEventListener("click", () => startMemoryEdit("new"));
    column.append(add);
  }
  return column;
}

// DESIGN.md §6.1: `342 ——▓▓▓——— 500 tk`, a 2px amber bar between the numbers.
function budget(used: number, cap: number): HTMLElement {
  const line = el("div", "budget");
  const bar = el("div", "budget-bar");
  const fill = el("div", "budget-fill");
  fill.style.width = `${Math.min(100, cap === 0 ? 100 : (used / cap) * 100)}%`;
  bar.append(fill);
  line.append(el("span", "budget-used", String(used)), bar, el("span", "budget-cap", `${cap} tk`));
  return line;
}

function coreRow(row: MemoryRow): HTMLElement {
  const line = el("div", "mem-row");
  line.append(
    el("span", "mem-id core", row.display_id),
    el("span", "mem-content", row.content),
    el("span", "mem-tokens", `${row.tokens} tk`),
  );
  const actions = el("span", "mem-actions");
  const edit = el("button", "mem-edit", "✎");
  edit.addEventListener("click", () => startMemoryEdit(row.id));
  const remove = el("button", "mem-delete", "✕");
  remove.addEventListener("click", () => void removeMemory(row.id));
  actions.append(edit, remove);
  line.append(actions);
  return line;
}

// The row becomes an input in place; `null` is the add-new row.
function editRow(row: MemoryRow | null): HTMLElement {
  const line = el("div", "mem-row editing");
  if (row !== null) line.append(el("span", "mem-id core", row.display_id));
  if (editingShown !== state.brain.editing) {
    editingShown = state.brain.editing;
    editor.value = row?.content ?? "";
    queueMicrotask(() => editor.focus());
  }
  line.append(editor);
  return line;
}

function episodicColumn(): HTMLElement {
  const column = el("div", "brain-col brain-epi");
  column.append(el("div", "col-label epi", "EPISODIC — top-k retrieval"));

  const toolbar = el("div", "brain-toolbar");
  const sort = el("button", "sort-toggle", `${state.brain.sort} ▾`);
  sort.addEventListener("click", () => {
    const next = SORTS[(SORTS.indexOf(state.brain.sort) + 1) % SORTS.length];
    void setBrainSort(next);
  });
  toolbar.append(search, sort);
  column.append(toolbar);

  const rows = state.brain.results ?? state.brain.episodic;
  for (const row of rows) column.append(epiRow(row));
  if (rows.length === 0 && state.brain.results !== null) {
    column.append(el("div", "brain-none", "nothing similar"));
  }
  return column;
}

function epiRow(row: MemoryRow): HTMLElement {
  const line = el("div", "mem-row");
  line.append(
    el("span", "mem-id epi", row.display_id),
    el("span", "mem-content", row.content),
  );
  const now = Math.floor(Date.now() / 1000);
  if (row.last_injected_at !== null && now - row.last_injected_at <= INJECTED_TAG_S) {
    const minutes = Math.max(0, Math.floor((now - row.last_injected_at) / 60));
    line.append(el("span", "mem-tag", `injected ${minutes}m ago`));
  }
  const meta =
    row.hits > 0 ? `${row.hits} hits` : date(row.created_at);
  line.append(el("span", "mem-meta", meta));
  return line;
}

const date = (seconds: number): string =>
  new Date(seconds * 1000).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
  });
