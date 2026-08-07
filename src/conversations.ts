import type { Conversation } from "./api";
import { el } from "./dom";
import { deleteConversation, selectConversation, state } from "./state";

type Hit = { row: Conversation; score: number; marks: number[] };

// Characters a word can start after, which is what makes an initialism like
// `dsr` find "deepseek · slow reply" instead of only literal prefixes.
const BOUNDARY = " -_/.:·";

// The query lives on the input, not in app state: the whole list is already in
// memory, so filtering it is local to this view and nothing else needs to know.
const search = el("input", "conv-search");
search.placeholder = "search conversations…";
search.setAttribute("aria-label", "search conversations");

const count = el("div", "conv-count");
const list = el("div", "conv-list");

// Which row the arrows are on; the text changing puts it back on the best hit.
let active = 0;

search.addEventListener("input", () => {
  active = 0;
  fill();
});

search.addEventListener("keydown", (event) => {
  const hits = matches();
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    if (hits.length === 0) return;
    const delta = event.key === "ArrowDown" ? 1 : -1;
    active = (active + delta + hits.length) % hits.length;
    fill();
    list.children[active]?.scrollIntoView({ block: "nearest" });
    return;
  }
  if (event.key === "Escape") {
    search.value = "";
    active = 0;
    fill();
    return;
  }
  if (event.key !== "Enter") return;
  event.preventDefault();
  const chosen = hits[active];
  if (chosen !== undefined) void selectConversation(chosen.row.id);
});

export function renderConversations(): HTMLElement {
  // Leaving the view detaches the input, so this is "just arrived" and the
  // focus is not taken again on the redraws that follow.
  const arrived = !search.isConnected;
  const view = el("div", "convs");
  const head = el("div", "conv-head");
  head.append(search, count);
  view.append(head, list);
  fill();
  if (arrived) queueMicrotask(() => search.focus());
  return view;
}

function fill(): void {
  const hits = matches();
  if (active >= hits.length) active = 0;
  count.textContent = summary(hits.length);
  list.replaceChildren(...hits.map(line));
  if (hits.length > 0) return;
  const empty = state.conversations.length === 0 ? "no conversations yet" : "nothing matches";
  list.append(el("div", "conv-none", empty));
}

function line(hit: Hit, index: number): HTMLElement {
  const row = el("div", "conv-row");
  if (index === active) row.classList.add("active");
  if (hit.row.id === state.selected) row.classList.add("current");

  const open = el("button", "conv-open");
  const model = hit.row.model === "" ? "no model" : hit.row.model;
  open.append(
    title(hit),
    el("span", "conv-model", `${hit.row.provider} · ${model}`),
    el("span", "conv-date", date(hit.row.updated_at)),
  );
  open.addEventListener("click", () => void selectConversation(hit.row.id));
  open.addEventListener("pointerenter", () => point(index));

  const remove = el("button", "conv-delete", "✕");
  remove.setAttribute("aria-label", `delete ${hit.row.title}`);
  remove.addEventListener("click", () => void deleteConversation(hit.row.id));

  row.append(open, remove);
  return row;
}

// The letters that earned the hit are marked, so a match nobody would have
// guessed at least shows its reasoning.
function title(hit: Hit): HTMLElement {
  const box = el("span", "conv-title");
  const text = hit.row.title;
  let at = 0;
  for (const mark of hit.marks) {
    if (mark > at) box.append(text.slice(at, mark));
    box.append(el("span", "conv-mark", text.slice(mark, mark + 1)));
    at = mark + 1;
  }
  box.append(text.slice(at));
  return box;
}

// Spaces are dropped from the query, so `dee cha` is the same search as
// `deecha` and neither has to guess where the title puts its own spaces.
function matches(): Hit[] {
  const query = search.value.toLowerCase().replace(/\s+/g, "");
  if (query === "") return state.conversations.map((row) => ({ row, score: 0, marks: [] }));
  const hits: Hit[] = [];
  for (const row of state.conversations) {
    const hit = fuzzy(row.title, query);
    if (hit !== null) hits.push({ row, ...hit });
  }
  // The sort is stable, so equal scores stay in most-recent-first order.
  return hits.sort((a, b) => b.score - a.score);
}

// The letters have to appear in order; ones that land together or start a word
// count for more, which is the whole difference between a fuzzy match and a
// substring one.
function fuzzy(text: string, query: string): { score: number; marks: number[] } | null {
  const hay = text.toLowerCase();
  const marks: number[] = [];
  let score = 0;
  let at = 0;
  let first = 0;
  for (const letter of query) {
    const found = hay.indexOf(letter, at);
    if (found === -1) return null;
    if (marks.length === 0) first = found;
    if (found === at && found > 0) score += 8;
    else if (found === 0 || BOUNDARY.includes(hay.charAt(found - 1))) score += 5;
    marks.push(found);
    at = found + letter.length;
  }
  // Same letters, so what separates the rows is where the match starts and how
  // much title it had to search through to find it.
  return { score: score - first / 10 - text.length / 100, marks };
}

function point(index: number): void {
  if (active === index) return;
  active = index;
  for (const [at, row] of [...list.children].entries()) {
    row.classList.toggle("active", at === index);
  }
}

function summary(shown: number): string {
  const total = state.conversations.length;
  if (shown !== total) return `${shown} of ${total}`;
  return total === 1 ? "1 conversation" : `${total} conversations`;
}

const date = (seconds: number): string =>
  new Date(seconds * 1000).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
  });
