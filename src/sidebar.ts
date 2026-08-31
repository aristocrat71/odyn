import { el } from "./dom";
import { setView, state, type View } from "./state";

const NAV: View[] = ["brain", "reminders", "providers", "config"];

export function renderSidebar(root: HTMLElement): void {
  root.replaceChildren(wordmark(), nav(), footer());
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

function footer(): HTMLElement {
  const box = el("div");
  if (state.hotkeyError !== null) {
    box.append(el("div", "status status-warn", state.hotkeyError));
  }
  return box;
}
