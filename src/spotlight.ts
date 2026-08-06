import "@fontsource/ibm-plex-sans/400.css";
import "@fontsource/ibm-plex-mono/400.css";
import "./tokens.css";
import "./spotlight.css";

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { el } from "./dom";
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
  | { request_id: number; kind: "error"; message: string };

const input = document.getElementById("spot-input") as HTMLInputElement;
const ledger = document.getElementById("spot-ledger") as HTMLDivElement;
const results = document.getElementById("spot-results") as HTMLDivElement;

let current: number | null = null;
let answer = "";
let streaming = false;
let used: string[] = [];

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
  results.scrollTop = results.scrollHeight;
}

function fail(message: string): void {
  streaming = false;
  results.hidden = false;
  results.append(el("div", "spot-error", message));
}

async function ask(): Promise<void> {
  const text = input.value.trim();
  if (text === "") return;
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

async function promote(): Promise<void> {
  try {
    await invoke<number>("spotlight_promote");
  } catch (err) {
    fail(String(err));
  }
}

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    void invoke("spotlight_hide");
    return;
  }
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    void promote();
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
    fail(data.message);
  }
});

void listen("spotlight-show", reset);
reset();
