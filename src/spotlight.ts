import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "./tokens.css";
import "./spotlight.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { accept, ghost } from "./complete";
import { el, forgetTraces, trace, waiting } from "./dom";
import { closeOpenDropdown, dropdown } from "./dropdown";
import { renderInto } from "./markdown";
import { dueLabel } from "./due";
import { mentionAsk } from "./mentions";

type SpotEvent =
  | {
      request_id: number;
      kind: "context";
      used: string[];
      tokens: number;
      soul: number;
    }
  | { request_id: number; kind: "delta"; text: string }
  | { request_id: number; kind: "saved"; slug: string }
  | { request_id: number; kind: "updated"; slug: string }
  | { request_id: number; kind: "deleted"; slug: string }
  | { request_id: number; kind: "linked"; from: string; to: string }
  | { request_id: number; kind: "unlinked"; from: string; to: string }
  | { request_id: number; kind: "reminded"; text: string; due_at: number }
  | { request_id: number; kind: "done" }
  // `detail` present means `message` stands in for the provider's own words.
  | { request_id: number; kind: "error"; message: string; detail?: string };

type SpotProvider = { name: string; kind: string; models: string[] };
type SpotTarget = {
  provider: string;
  model: string;
  needs_key: boolean;
  providers: SpotProvider[];
};

const input = document.getElementById("spot-input") as HTMLInputElement;
const ledger = document.getElementById("spot-ledger") as HTMLDivElement;
const results = document.getElementById("spot-results") as HTMLDivElement;
const dueBox = document.getElementById("spot-due") as HTMLDivElement;
const picks = document.getElementById("spot-picks") as HTMLSpanElement;
const surface = document.querySelector(".spot-surface") as HTMLDivElement;

const hint = ghost(input, "spot-ask");

// Menus drop into the window's empty lower half; higher up, the top edge clips.
const providerDrop = dropdown({
  label: "provider",
  onPick: (value) => void pick(value, ""),
});
const modelDrop = dropdown({
  label: "model",
  empty: "no model",
  onPick: (value) => void pick(providerDrop.value(), value),
});
picks.append(providerDrop.root, modelDrop.root);

type Due = { text: string; due_at: number };

const chime = new Audio("/odyn-notif.wav");
chime.loop = true;

/// `view: null` is a mention, not a destination: the text stays in the field.
type Command = { cmd: string; view: string | null; hint: string };

const COMMANDS: Command[] = [
  { cmd: "/home", view: "home", hint: "the front door" },
  { cmd: "/chat", view: "chat", hint: "the conversation" },
  { cmd: "/convos", view: "conversations", hint: "every conversation, searchable" },
  { cmd: "/providers", view: "providers", hint: "models, endpoints and keys" },
  { cmd: "/config", view: "config", hint: "the file behind it all" },
  { cmd: "/guide", view: "guide", hint: "how everything works" },
  { cmd: "/view-brain", view: "brain", hint: "what odyn remembers" },
  { cmd: "/view-reminders", view: "reminders", hint: "what odyn will remind you of" },
  { cmd: "/brain", view: null, hint: "ask with what odyn remembers" },
  { cmd: "/memory", view: null, hint: "tell odyn something to remember" },
  { cmd: "/update-memory", view: null, hint: "tell odyn something changed" },
  { cmd: "/delete-memory", view: null, hint: "tell odyn to forget something" },
  { cmd: "/link-memory", view: null, hint: "connect two memories" },
  { cmd: "/unlink-memory", view: null, hint: "disconnect two memories" },
  { cmd: "/reminder", view: null, hint: "set a reminder" },
];

let current: number | null = null;
let answer = "";
let streaming = false;
let used: string[] = [];
let saved: string[] = [];
let updated: string[] = [];
let deleted: string[] = [];
let linked: string[] = [];
let unlinked: string[] = [];
let reminders: string[] = [];
let dueNow: Due[] = [];
let target: SpotTarget | null = null;
// While true, the ask field is the key intake: masked, saved on ⏎.
let keyMode = false;
let commandMode = false;
let cursor = 0;

function reset(): void {
  clearScreen();
  void loadTarget();
}

async function loadTarget(): Promise<void> {
  try {
    target = await invoke<SpotTarget>("spotlight_target");
  } catch (err) {
    fail(String(err));
    return;
  }
  drawTarget();
}

function drawTarget(): void {
  if (target === null) return;
  providerDrop.set(
    target.providers.map((p) => ({ value: p.name })),
    target.provider,
  );

  const models = target.providers.find((p) => p.name === target?.provider)?.models ?? [];
  const items = models.map((model) => ({ value: model }));
  if (target.model !== "" && !models.includes(target.model)) {
    items.push({ value: target.model });
  }
  modelDrop.set(items, target.model !== "" ? target.model : (models[0] ?? ""));
  modelDrop.setDisabled(items.length === 0);

  // The field itself takes the key, masked against shoulders and screen shares.
  keyMode = target.needs_key;
  input.type = keyMode ? "password" : "text";
  input.placeholder = keyMode ? `paste the ${target.provider} api key…` : "ask odyn…";
  if (keyMode) keyCard();
}

function keyCard(): void {
  if (target === null) return;
  const card = el("div", "spot-card");
  card.append(
    el("div", undefined, `${target.provider} needs a key before it can answer.`),
    el(
      "div",
      "spot-card-dim",
      "paste it above and press ⏎ — it is stored in odyn.toml and never shown again.",
    ),
  );
  results.hidden = false;
  results.replaceChildren(card);
}

// DESIGN.md §7: one line between field and answer, filled when anything is
// injected — recalled notes, the soul note, or both.
function drawLedger(event: SpotEvent & { kind: "context" }): void {
  ledger.replaceChildren();
  if (event.tokens === 0 && event.soul === 0) return;
  if (event.soul > 0) ledger.append(el("span", "ledger-soul", `● soul ${event.soul}`));
  if (event.tokens > 0) {
    // Which notes came back is named by the `◈ used` trace under the answer.
    ledger.append(el("span", "ledger-reading", "◈ reading the brain"));
    ledger.append(el("span", "spot-ledger-total", `${event.tokens} tk`));
  }
  ledger.hidden = commandMode;
}

// Holds the streamed answer so frozen markdown blocks survive each delta.
const answerBox = el("div", "spot-answer");

function draw(): void {
  if (commandMode) return;
  results.hidden = false;
  if (streaming && answer === "") {
    results.replaceChildren(waiting());
    return;
  }
  renderInto(answerBox, answer);
  for (const mark of answerBox.querySelectorAll(".cursor")) mark.remove();
  if (streaming) {
    const last = answerBox.lastElementChild ?? answerBox.appendChild(el("p", "para"));
    last.append(el("span", "cursor"));
  }
  results.replaceChildren(answerBox);
  if (!streaming && used.length > 0) {
    results.append(trace("◈", "used", used, "used"));
  }
  if (!streaming && saved.length > 0) {
    results.append(trace("✎", "saved", saved, "saved"));
  }
  if (!streaming && updated.length > 0) {
    results.append(trace("✎", "updated", updated, "updated"));
  }
  if (!streaming && deleted.length > 0) {
    results.append(trace("✕", "deleted", deleted, "deleted"));
  }
  if (!streaming && linked.length > 0) {
    results.append(trace("⌇", "linked", linked, "linked"));
  }
  if (!streaming && unlinked.length > 0) {
    results.append(trace("⌇", "unlinked", unlinked, "unlinked"));
  }
  if (!streaming && reminders.length > 0) {
    results.append(trace("◔", "reminder", reminders, "reminded"));
  }
  // No auto-scroll: a growing answer must not yank the panel while reading.
}

// A due reminder takes the whole panel: the field, footer and any answer are
// hidden, so the only thing to do is read it and dismiss it.
function drawDue(): void {
  dueBox.replaceChildren();
  surface.classList.toggle("due-only", dueNow.length > 0);
  if (dueNow.length === 0) {
    dueBox.hidden = true;
    input.disabled = false;
    return;
  }
  input.disabled = true;
  input.blur();
  void chime.play().catch(() => {});
  for (const due of dueNow) {
    const row = el("div", "spot-due-row");
    row.append(
      el("span", "spot-due-mark", "◔"),
      el("span", "spot-due-text", due.text),
      el("span", "spot-due-at", dueLabel(due.due_at)),
    );
    dueBox.append(row);
  }
  const dismiss = el("button", "spot-due-clear", "dismiss");
  dismiss.addEventListener("click", clearDue);
  dueBox.append(dismiss);
  dueBox.hidden = false;
}

function clearDue(): void {
  chime.pause();
  chime.currentTime = 0;
  dueNow = [];
  dueBox.replaceChildren();
  dueBox.hidden = true;
  surface.classList.remove("due-only");
  input.disabled = false;
  input.focus();
}

function fail(message: string, detail?: string): void {
  streaming = false;
  results.querySelector(".waiting")?.remove();
  // Whatever streamed before the failure is kept, minus the cursor.
  if (answer !== "") draw();
  results.hidden = false;
  results.append(el("div", "spot-error", message));
  if (detail !== undefined) {
    console.error(`[odyn] ${detail}`);
    results.append(el("div", "spot-error-hint", "⌘K picks another model"));
  }
}

function clearScreen(): void {
  current = null;
  answer = "";
  streaming = false;
  used = [];
  saved = [];
  updated = [];
  deleted = [];
  linked = [];
  unlinked = [];
  reminders = [];
  clearDue();
  forgetTraces();
  commandMode = false;
  cursor = 0;
  input.value = "";
  hint.draw(undefined);
  ledger.hidden = true;
  ledger.replaceChildren();
  results.hidden = true;
  results.replaceChildren();
  input.focus();
}

function commands(): Command[] {
  const text = input.value.trim().toLowerCase();
  return COMMANDS.filter((command) => command.cmd.startsWith(text));
}

function drawCommands(): void {
  const shown = commands();
  if (cursor >= shown.length) cursor = 0;
  ledger.hidden = true;
  results.hidden = false;
  // The highlighted row is what the field completes to, ghost included.
  const completing = hint.draw(shown[cursor]?.cmd);
  if (shown.length === 0) {
    results.replaceChildren(el("div", "spot-cmd-none", "no such command"));
    return;
  }
  const box = el("div", "spot-cmds");
  box.append(
    ...shown.map((command, index) => {
      const row = el("button", "spot-cmd");
      if (index === cursor) row.classList.add("active");
      row.append(
        el("span", "spot-cmd-name", command.cmd),
        el("span", "spot-cmd-hint", command.hint),
      );
      if (index === cursor && completing) row.append(el("span", "spot-cmd-key", "⇥"));
      row.addEventListener("click", () => void run(command));
      row.addEventListener("pointerenter", () => {
        cursor = index;
        drawCommands();
      });
      return row;
    }),
  );
  results.replaceChildren(box);
}

function drawAsk(): void {
  ledger.hidden = ledger.childElementCount === 0;
  hint.draw(undefined);
  // An ask typed over mid-flight comes back to the answer so far.
  if (answer !== "" || streaming) {
    draw();
    return;
  }
  results.hidden = true;
  results.replaceChildren();
}

async function run(command: Command): Promise<void> {
  // A mention is finished in place, not run: the field is now an ask.
  if (command.view === null) {
    take(command);
    return;
  }
  try {
    await invoke("spotlight_open_view", { view: command.view });
  } catch (err) {
    fail(String(err));
    return;
  }
  clearScreen();
}

// A field that has become a `/brain` ask leaves the command list behind.
function take(command: Command | undefined): boolean {
  if (!accept(input, command?.cmd)) return false;
  input.focus();
  if (mentionAsk(input.value)) {
    commandMode = false;
    drawAsk();
  } else {
    drawCommands();
  }
  return true;
}

async function ask(): Promise<void> {
  const text = input.value.trim();
  if (text === "") return;
  if (keyMode) {
    await saveKey(text);
    return;
  }
  answer = "";
  streaming = true;
  used = [];
  saved = [];
  updated = [];
  deleted = [];
  linked = [];
  unlinked = [];
  reminders = [];
  clearDue();
  forgetTraces();
  ledger.hidden = true;
  ledger.replaceChildren();
  draw();
  try {
    current = await invoke<number>("spotlight_ask", { text });
  } catch (err) {
    fail(String(err));
  }
}

async function saveKey(key: string): Promise<void> {
  const name = target?.provider ?? "";
  try {
    await invoke("spotlight_save_key", { key });
  } catch (err) {
    fail(String(err));
    return;
  }
  input.value = "";
  await loadTarget();
  results.hidden = false;
  results.replaceChildren(el("div", "spot-ok", `● ${name} connected · ask away`));
  input.focus();
}

async function promote(): Promise<void> {
  try {
    await invoke<number>("spotlight_promote");
  } catch (err) {
    fail(String(err));
  }
}

// Written to `[spotlight]` in odyn.toml: it survives restarts, and the CLI too.
async function pick(provider: string, model: string): Promise<void> {
  if (model === "") {
    const models = target?.providers.find((p) => p.name === provider)?.models ?? [];
    model = models[0] ?? "";
  }
  try {
    await invoke("spotlight_set_target", { provider, model });
  } catch (err) {
    fail(String(err));
    return;
  }
  await loadTarget();
  input.focus();
}

function cycleProvider(): void {
  if (target === null || target.providers.length < 2) return;
  const names = target.providers.map((p) => p.name);
  const next = names[(names.indexOf(target.provider) + 1) % names.length];
  if (next !== undefined) void pick(next, "");
}

function cycleModel(): void {
  if (target === null) return;
  const models = target.providers.find((p) => p.name === target?.provider)?.models ?? [];
  if (models.length < 2) return;
  const at = models.indexOf(target.model);
  const next = models[(Math.max(at, 0) + 1) % models.length];
  if (next !== undefined) void pick(target.provider, next);
}

input.addEventListener("input", () => {
  if (!keyMode && input.value.startsWith("/") && !mentionAsk(input.value)) {
    cursor = 0;
    commandMode = true;
    drawCommands();
    return;
  }
  if (!commandMode) return;
  commandMode = false;
  drawAsk();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    // An open menu takes the Esc; the next one reaches the panel.
    if (closeOpenDropdown()) return;
    void invoke("spotlight_hide");
    return;
  }
  const mod = e.metaKey || e.ctrlKey;
  if (e.key === "Enter" && mod) {
    e.preventDefault();
    void promote();
    return;
  }
  if (e.key === "Backspace" && mod) {
    e.preventDefault();
    clearScreen();
    return;
  }
  if (mod && e.key.toLowerCase() === "k") {
    e.preventDefault();
    cycleModel();
    return;
  }
  if (mod && e.key.toLowerCase() === "p") {
    e.preventDefault();
    cycleProvider();
    return;
  }
  if (commandMode) {
    const shown = commands();
    // → takes the completion only from the end of the line.
    const end = input.selectionStart === input.value.length;
    if (e.key === "Tab" || (e.key === "ArrowRight" && end)) {
      if (take(shown[cursor])) e.preventDefault();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (shown.length === 0) return;
      cursor = (cursor + (e.key === "ArrowDown" ? 1 : -1) + shown.length) % shown.length;
      drawCommands();
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      const chosen = shown[cursor];
      if (chosen !== undefined) void run(chosen);
      return;
    }
  }
  if (e.key === "Enter" && document.activeElement === input) {
    e.preventDefault();
    void ask();
  }
});

void listen<SpotEvent>("spotlight-event", (event) => {
  const data = event.payload;
  if (data.request_id !== current) return;
  if (data.kind === "context") {
    used = data.used;
    drawLedger(data);
  } else if (data.kind === "delta") {
    answer += data.text;
    draw();
  } else if (data.kind === "saved") {
    saved.push(data.slug);
  } else if (data.kind === "updated") {
    updated.push(data.slug);
  } else if (data.kind === "deleted") {
    deleted.push(data.slug);
  } else if (data.kind === "linked") {
    linked.push(`${data.from} → ${data.to}`);
  } else if (data.kind === "unlinked") {
    unlinked.push(`${data.from} ⇢ ${data.to}`);
  } else if (data.kind === "reminded") {
    reminders.push(`${data.text} · ${dueLabel(data.due_at)}`);
  } else if (data.kind === "done") {
    streaming = false;
    draw();
  } else {
    fail(data.message, data.detail);
  }
});

// Survives `reset`: a reminder stays up until it is dismissed or the next ask,
// and it is already marked shown, so it never arrives twice.
void listen<Due[]>("reminder-due", (event) => {
  for (const due of event.payload) dueNow.push(due);
  drawDue();
});

// Hiding keeps the exchange, so a re-summon refreshes the target and leaves
// whatever is on screen alone. Esc and promotion are what empty the panel.
void listen("spotlight-show", () => {
  void loadTarget();
  if (!input.disabled) input.focus();
});

void listen("spotlight-clear", reset);
reset();
