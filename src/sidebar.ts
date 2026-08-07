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

const NAV: View[] = ["home", "chat", "brain", "providers", "config", "guide"];

const RECENT = 7;

export function renderSidebar(root: HTMLElement): void {
  root.replaceChildren(
    wordmark(),
    nav(),
    label(),
    conversations(),
    newLink(),
    footer(),
  );
}

function label(): HTMLElement {
  const link = el("button", "label label-link");
  const word = el("span");
  if (state.view === "conversations") {
    link.classList.add("active");
    word.append(el("span", "mark", "—"), " conversations");
  } else {
    word.textContent = "conversations";
  }
  link.append(word);
  const total = state.conversations.length;
  if (total > RECENT) link.append(el("span", "label-count", String(total)));
  link.addEventListener("click", () => setView("conversations"));
  return link;
}

function wordmark(): HTMLElement {
  const mark = el("div", "wordmark");
  mark.append(el("span", "rune", "ᛟ"), el("span", "wordmark-text", "ODYN"));
  return mark;
}

function nav(): HTMLElement {
  const bar = el("nav", "nav");
  for (const view of NAV) {
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
  for (const row of recent()) list.append(conversation(row));
  return list;
}

function recent(): Conversation[] {
  const rows = state.conversations.slice(0, RECENT);
  if (state.selected === null || rows.some((row) => row.id === state.selected)) return rows;
  const open = state.conversations.find((row) => row.id === state.selected);
  if (open === undefined) return rows;
  return [...rows.slice(0, RECENT - 1), open];
}

function conversation(row: Conversation): HTMLElement {
  const line = el("div", "row");
  // Selected is not the same as open: only the chat view is inside it.
  const open = state.view === "chat" && row.id === state.selected;
  if (open) line.classList.add("active");
  if (row.id === state.editing) {
    line.append(rename(row));
    return line;
  }

  const title = el("button", "row-title");
  if (open) title.append(el("span", "mark", "—"), ` ${row.title}`);
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
  if (state.hotkeyError !== null) {
    box.append(el("div", "status status-warn", state.hotkeyError));
  }
  return box;
}
