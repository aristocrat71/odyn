import type { Conversation } from "./api";
import { el } from "./dom";
import {
  cancelRename,
  commitRename,
  deleteConversation,
  newConversation,
  selectConversation,
  setDraft,
  setView,
  startRename,
  state,
  type View,
} from "./state";

const VIEWS: View[] = ["chat", "brain", "config", "guide"];

export function renderSidebar(root: HTMLElement): void {
  root.replaceChildren(
    wordmark(),
    nav(),
    el("div", "label", "conversations"),
    conversations(),
    newLink(),
    footer(),
  );
}

function wordmark(): HTMLElement {
  const mark = el("div", "wordmark");
  mark.append(el("span", "rune", "ᛟ"), el("span", "wordmark-text", "ODYN"));
  return mark;
}

function nav(): HTMLElement {
  const bar = el("nav", "nav");
  for (const view of VIEWS) {
    const item = el("button", "nav-item");
    if (view === state.view) {
      item.classList.add("active");
      item.append(el("span", "mark", "—"), ` ${view}`);
    } else {
      item.textContent = view;
    }
    item.addEventListener("click", () => setView(view));
    bar.append(item);
  }
  return bar;
}

function conversations(): HTMLElement {
  const list = el("div", "conversations");
  for (const row of state.conversations) list.append(conversation(row));
  return list;
}

function conversation(row: Conversation): HTMLElement {
  const line = el("div", "row");
  if (row.id === state.selected) line.classList.add("active");
  if (row.id === state.editing) {
    line.append(rename(row));
    return line;
  }

  const title = el("button", "row-title");
  if (row.id === state.selected) title.append(el("span", "mark", "—"), ` ${row.title}`);
  else title.textContent = row.title;
  title.addEventListener("click", () => void selectConversation(row.id));
  title.addEventListener("dblclick", () => startRename(row.id));

  const remove = el("button", "row-delete", "✕");
  remove.addEventListener("click", () => void deleteConversation(row.id));

  line.append(title, remove);
  return line;
}

function rename(row: Conversation): HTMLElement {
  const input = el("input", "row-input");
  input.value = state.draft;
  input.addEventListener("input", () => setDraft(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") void commitRename();
    if (event.key === "Escape") cancelRename();
  });
  input.addEventListener("blur", () => {
    if (state.editing === row.id) void commitRename();
  });
  // The row is not in the document until this render finishes.
  queueMicrotask(() => {
    input.focus();
    input.select();
  });
  return input;
}

function newLink(): HTMLElement {
  const link = el("button", "new", "+ new");
  link.addEventListener("click", () => void newConversation());
  return link;
}

function footer(): HTMLElement {
  const box = el("div");
  const line = el("div", "status");
  box.append(line);
  const status = state.status;
  if (status === null) return box;
  line.append(probe(status.provider_name, status.provider_reachable));
  // A second dot only when a local Ollama runs beside the default provider.
  if (status.ollama_reachable !== null) {
    line.append(probe("ollama", status.ollama_reachable));
  }
  line.append(el("span", "status-rss", megabytes(status.rss_bytes)));
  if (state.hotkeyError !== null) {
    box.append(el("div", "status status-warn", state.hotkeyError));
  }
  return box;
}

function probe(name: string, reachable: boolean): HTMLElement {
  const box = el("span");
  box.append(el("span", reachable ? "dot" : "dot down", "●"), name);
  return box;
}

function megabytes(bytes: number): string {
  return `${Math.round(bytes / 1024 / 1024)}MB`;
}
