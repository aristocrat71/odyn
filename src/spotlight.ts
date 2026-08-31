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
  { cmd: "/providers", view: "providers", hint: "models, endpoints and keys" },
  { cmd: "/config", view: "config", hint: "the file behind it all" },
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

type Turn = {
  id: number;
  // Known once the ask is accepted; until then an event belongs to whatever
  // ask this one replaced, and is dropped.
  request: number | null;
  answer: string;
  used: string[];
  saved: string[];
  updated: string[];
  deleted: string[];
  linked: string[];
  unlinked: string[];
  reminders: string[];
  node: HTMLDivElement;
  body: HTMLDivElement;
};

let streaming = false;
let turns: Turn[] = [];
// The turn the stream is filling; nothing else is ever redrawn.
let live: Turn | null = null;
let seq = 0;
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

// DESIGN.md §7: one line between field and answer, filled when notes are
// recalled.
function drawLedger(event: SpotEvent & { kind: "context" }): void {
  ledger.replaceChildren();
  if (event.tokens === 0) return;
  if (event.tokens > 0) {
    // Which notes came back is named by the `◈ used` trace under the answer.
    ledger.append(el("span", "ledger-reading", "◈ reading the brain"));
    ledger.append(el("span", "spot-ledger-total", `${event.tokens} tk`));
  }
  ledger.hidden = commandMode;
}

// A turn owns its body, so frozen markdown survives each delta and the turns
// above it are never re-parsed.
function newTurn(question: string): Turn {
  seq += 1;
  const node = el("div", "spot-turn");
  const asked = el("div", "spot-turn-ask");
  asked.append(el("span", "spot-turn-mark", "›"), el("span", "spot-turn-text", question));
  const body = el("div", "spot-answer");
  node.append(asked, body);
  return {
    id: seq,
    request: null,
    answer: "",
    used: [],
    saved: [],
    updated: [],
    deleted: [],
    linked: [],
    unlinked: [],
    reminders: [],
    node,
    body,
  };
}

function draw(turn: Turn): void {
  const flowing = streaming && turn === live;
  while (turn.body.nextSibling !== null) turn.body.nextSibling.remove();
  renderInto(turn.body, turn.answer);
  for (const mark of turn.body.querySelectorAll(".cursor")) mark.remove();
  if (flowing) {
    if (turn.answer === "") {
      turn.node.append(waiting());
      return;
    }
    const last = turn.body.lastElementChild ?? turn.body.appendChild(el("p", "para"));
    last.append(el("span", "cursor"));
    return;
  }
  const traces: [string, string, string[], string][] = [
    ["◈", "used", turn.used, "used"],
    ["✎", "saved", turn.saved, "saved"],
    ["✎", "updated", turn.updated, "updated"],
    ["✕", "deleted", turn.deleted, "deleted"],
    ["⌇", "linked", turn.linked, "linked"],
    ["⌇", "unlinked", turn.unlinked, "unlinked"],
    ["◔", "reminder", turn.reminders, "reminded"],
  ];
  for (const [mark, label, ids, key] of traces) {
    if (ids.length > 0) turn.node.append(trace(mark, label, ids, `${turn.id}:${key}`));
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
  const failed = live;
  live = null;
  // Whatever streamed before the failure is kept, minus the cursor.
  if (failed !== null) draw(failed);
  const box = failed?.node ?? results;
  results.hidden = false;
  box.append(el("div", "spot-error", message));
  if (detail !== undefined) {
    console.error(`[odyn] ${detail}`);
    box.append(el("div", "spot-error-hint", "⌘K picks another model"));
  }
}

function clearScreen(): void {
  live = null;
  turns = [];
  streaming = false;
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
  // The command list took the panel; the thread it covered comes back.
  results.hidden = turns.length === 0;
  results.replaceChildren(...turns.map((turn) => turn.node));
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
  const turn = newTurn(text);
  turns.push(turn);
  live = turn;
  streaming = true;
  input.value = "";
  hint.draw(undefined);
  clearDue();
  ledger.hidden = true;
  ledger.replaceChildren();
  results.hidden = false;
  results.append(turn.node);
  draw(turn);
  // The new question goes to the top of the panel, so its answer streams into
  // view instead of below the fold.
  results.scrollTop =
    turn.node.getBoundingClientRect().top -
    results.getBoundingClientRect().top +
    results.scrollTop;
  try {
    turn.request = await invoke<number>("spotlight_ask", { text });
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
  if (e.key === "Backspace" && mod) {
    e.preventDefault();
    clearScreen();
    void invoke("spotlight_forget");
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
  const turn = live;
  if (turn === null || data.request_id !== turn.request) return;
  if (data.kind === "context") {
    turn.used = data.used;
    drawLedger(data);
  } else if (data.kind === "delta") {
    turn.answer += data.text;
    draw(turn);
  } else if (data.kind === "saved") {
    turn.saved.push(data.slug);
  } else if (data.kind === "updated") {
    turn.updated.push(data.slug);
  } else if (data.kind === "deleted") {
    turn.deleted.push(data.slug);
  } else if (data.kind === "linked") {
    turn.linked.push(`${data.from} → ${data.to}`);
  } else if (data.kind === "unlinked") {
    turn.unlinked.push(`${data.from} ⇢ ${data.to}`);
  } else if (data.kind === "reminded") {
    turn.reminders.push(`${data.text} · ${dueLabel(data.due_at)}`);
  } else if (data.kind === "done") {
    streaming = false;
    live = null;
    draw(turn);
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
// whatever is on screen alone. Esc is what empties the panel.
void listen("spotlight-show", () => {
  void loadTarget();
  if (!input.disabled) input.focus();
});

void listen("spotlight-clear", reset);
reset();
