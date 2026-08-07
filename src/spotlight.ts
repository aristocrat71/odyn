import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "./tokens.css";
import "./spotlight.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { el } from "./dom";
import { closeOpenDropdown, dropdown } from "./dropdown";
import { renderMarkdown } from "./markdown";

type SpotEvent =
  | {
      request_id: number;
      kind: "context";
      used: string[];
      core_tokens: number;
      episodic_tokens: number;
    }
  | { request_id: number; kind: "delta"; text: string }
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
const picks = document.getElementById("spot-picks") as HTMLSpanElement;

// Menus drop below the footer, into the window's empty lower half — above
// the footer sits the whole surface, and the window's top edge would clip.
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

let current: number | null = null;
let answer = "";
let streaming = false;
let used: string[] = [];
let target: SpotTarget | null = null;
// While true, the ask field is the key intake: masked, saved on ⏎.
let keyMode = false;

function reset(): void {
  current = null;
  answer = "";
  streaming = false;
  used = [];
  input.value = "";
  ledger.hidden = true;
  ledger.replaceChildren();
  results.hidden = true;
  results.replaceChildren();
  input.focus();
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

  // asklight's one-time setup: the field itself takes the key, masked so it
  // can't be shoulder-surfed or screen-shared.
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

// DESIGN.md §7: a minimal one-line ledger between the field and the answer.
function drawLedger(event: SpotEvent & { kind: "context" }): void {
  ledger.replaceChildren();
  const total = event.core_tokens + event.episodic_tokens;
  if (total === 0) return;
  if (event.core_tokens > 0) {
    ledger.append(el("span", "chip-core", `● core ${event.core_tokens}`));
  }
  for (const id of event.used) {
    ledger.append(el("span", "chip-epi", `◈ ${id}`));
  }
  ledger.append(el("span", "spot-ledger-total", `${total} tk`));
  ledger.hidden = false;
}

function draw(): void {
  results.hidden = false;
  const body = renderMarkdown(answer);
  if (streaming) {
    const last = body[body.length - 1] ?? el("p");
    last.append(el("span", "cursor"));
    if (body.length === 0) body.push(last);
  }
  results.replaceChildren(...body);
  if (!streaming && used.length > 0) {
    const trace = el("div", "trace");
    trace.append(el("span", "trace-mark", "◈"), " used");
    for (const id of used) trace.append(" ", el("span", "trace-id", id));
    results.append(trace);
  }
  // No auto-scroll: a growing answer must not yank the panel while reading.
}

function fail(message: string, detail?: string): void {
  streaming = false;
  // Whatever streamed before the failure is kept, minus the streaming cursor.
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
  input.value = "";
  ledger.hidden = true;
  ledger.replaceChildren();
  results.hidden = true;
  results.replaceChildren();
  input.focus();
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
  ledger.hidden = true;
  ledger.replaceChildren();
  results.replaceChildren();
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

// The pick is written to `[spotlight]` in odyn.toml, so it survives restarts
// and stays in force for the CLI too.
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
  } else if (data.kind === "done") {
    streaming = false;
    draw();
  } else {
    fail(data.message, data.detail);
  }
});

void listen("spotlight-show", reset);
reset();
