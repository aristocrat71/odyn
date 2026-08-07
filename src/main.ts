import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
import "./tokens.css";
import "./app.css";

import { listen } from "@tauri-apps/api/event";

import { spotlightToggle } from "./api";
import { refreshLedger } from "./chat";
import { el } from "./dom";
import { renderSidebar } from "./sidebar";
import {
  isView,
  load,
  newConversation,
  onChange,
  refreshStatus,
  selectConversation,
  setView,
  watchStream,
} from "./state";
import { renderView } from "./view";

const STATUS_INTERVAL_MS = 30_000;

const app = el("div", "app");
const sidebar = el("aside", "sidebar");
const main = el("main", "main");
// DESIGN.md §9: below 900px the sidebar is an overlay, and this is its handle.
const hamburger = el("button", "hamburger", "≡");
hamburger.setAttribute("aria-label", "menu");
app.append(hamburger, sidebar, main);
document.body.append(app);

hamburger.addEventListener("click", () => app.classList.toggle("sidebar-open"));

document.addEventListener("pointerdown", (event) => {
  const target = event.target;
  if (!(target instanceof Node)) return;
  if (sidebar.contains(target) || hamburger.contains(target)) return;
  app.classList.remove("sidebar-open");
});

// DESIGN.md §9. Every shortcut is a modifier combo, so a focused input can
// never swallow one and none of them need a focus guard.
const MAC = navigator.platform.startsWith("Mac");
const SHORTCUTS: Record<string, () => void> = {
  k: () => void spotlightToggle(),
  n: () => void newConversation(),
  "1": () => setView("chat"),
  "2": () => setView("brain"),
};

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") app.classList.remove("sidebar-open");
  const mod = MAC ? event.metaKey : event.ctrlKey;
  if (!mod || event.altKey || event.shiftKey) return;
  const act = SHORTCUTS[event.key.toLowerCase()];
  if (act === undefined) return;
  event.preventDefault();
  act();
});

onChange(() => {
  // Elements that survive a redraw — the composer — keep the caret too.
  const focused = document.activeElement;
  renderSidebar(sidebar);
  renderView(main);
  if (focused instanceof HTMLElement && focused.isConnected) focused.focus();
});
renderSidebar(sidebar);
renderView(main);

watchStream();
void load();
void refreshStatus();
setInterval(() => void refreshStatus(), STATUS_INTERVAL_MS);

// A memory added from the CLI shows up when the window comes back.
window.addEventListener("focus", refreshLedger);

// A promoted spotlight exchange lands here as a ready-made conversation.
void listen<number>("open-conversation", async (event) => {
  await load();
  await selectConversation(event.payload);
});

void listen<string>("open-view", (event) => {
  if (isView(event.payload)) setView(event.payload);
});
