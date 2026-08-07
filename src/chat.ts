import type { Conversation, Message } from "./api";
import { renderBrevity } from "./brevctl";
import { accept, ghost } from "./complete";
import { el, waiting } from "./dom";
import { renderMarkdown } from "./markdown";
import { renderPickers } from "./picker";
import {
  cancelStream,
  expandLedger,
  onStream,
  refreshPreview,
  resend,
  schedulePreview,
  send,
  state,
  streaming,
  type Stream,
} from "./state";

const INTERRUPTED = " (interrupted)";
const MOD = navigator.platform.startsWith("Mac") ? "⌘" : "Ctrl";
// How far from the end still counts as reading the end of the stream.
const SLACK = 24;
// Chips shown before the tail collapses to `◈ +N more`.
const CHIP_LIMIT = 5;
// Mentions can sit anywhere in a line, so the word under the caret is matched.
const MENTIONS = ["/brain", "/memory"];

// The composer outlives every redraw: draft, height and caret all survive.
const input = el("textarea", "composer-input");
input.rows = 1;
input.placeholder = "Message Odyn…";
const hint = ghost(input, "composer-ask");
const drawMention = (): void => {
  for (const mention of MENTIONS) if (hint.draw(mention)) return;
};
input.addEventListener("input", () => {
  grow();
  drawMention();
  schedulePreview(input.value);
});
input.addEventListener("keydown", (event) => {
  // → takes the completion only from the end of the draft. Neither key is
  // swallowed with nothing to complete: ⇥ still moves focus on.
  const end = input.selectionStart === input.value.length;
  if (event.key === "Tab" || (event.key === "ArrowRight" && end)) {
    if (MENTIONS.find((mention) => accept(input, mention)) === undefined) return;
    event.preventDefault();
    drawMention();
    // The mention is what turns recall on, so the CONTEXT line answers for it.
    schedulePreview(input.value);
    return;
  }
  if (event.key !== "Enter" || event.shiftKey) return;
  event.preventDefault();
  submit();
});
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") cancelStream();
});
onStream(patch);

let transcript: HTMLElement | null = null;
let answer: HTMLElement | null = null;
let shown: number | null = null;

export function renderChat(): HTMLElement {
  if (state.selected === null) return empty();

  const opened = shown !== state.selected;
  const follow = opened || transcript === null || atBottom(transcript);
  const offset = transcript === null ? 0 : transcript.scrollTop;
  shown = state.selected;

  const rolled = el("div", "transcript");
  answer = null;
  if (state.messages.length === 0) rolled.append(empty());
  for (const message of state.messages) rolled.append(said(message));
  const stream = state.stream;
  if (stream !== null && stream.conversation === state.selected) {
    rolled.append(streamed(stream));
  }
  transcript = rolled;

  const column = el("div", "chat");
  column.append(
    rolled,
    composer(),
    el("div", "hint", "the CONTEXT line is exactly what the model sees"),
  );
  // A conversation just opened previews immediately; typing debounces.
  if (opened) void refreshPreview(input.value);
  // Scrolling needs the transcript to be in the document and laid out.
  queueMicrotask(() => {
    rolled.scrollTop = follow ? rolled.scrollHeight : offset;
  });
  return column;
}

// Refreshed on window focus: a memory added from the CLI needs no restart.
export function refreshLedger(): void {
  if (state.selected !== null) void refreshPreview(input.value);
}

// The home input hands its non-command text over as a draft.
export function prefillComposer(text: string): void {
  input.value = text;
  drawMention();
  schedulePreview(text);
}

// One message per delta; the transcript follows only if already at the end.
function patch(): void {
  const stream = state.stream;
  if (answer === null || transcript === null || stream === null) return;
  const follow = atBottom(transcript);
  fill(answer, stream.text, true);
  if (follow) transcript.scrollTop = transcript.scrollHeight;
}

function said(message: Message): HTMLElement {
  const block = el("div", `message ${message.role}`);
  const text = el("div", "text");
  fill(text, message.content, false);
  block.append(speaker(message.role), text);
  if (message.used.length > 0) block.append(trace(message.used));
  return block;
}

function streamed(stream: Stream): HTMLElement {
  const block = el("div", "message assistant");
  const text = el("div", "text");
  fill(text, stream.text, stream.error === "");
  block.append(speaker("assistant"), text);
  if (stream.error === "") answer = text;
  else block.append(failed(stream.error));
  if (stream.used.length > 0) block.append(trace(stream.used));
  if (stream.saved.length > 0) block.append(saved(stream.saved));
  return block;
}

// DESIGN.md §5: `◈ used e-0142 e-0087` — mark and ids teal, the rest dim.
function trace(used: string[]): HTMLElement {
  const line = el("div", "trace");
  line.append(el("span", "trace-mark", "◈"), " used");
  for (const id of used) {
    line.append(" ", el("span", "trace-id", id));
  }
  return line;
}

function saved(slugs: string[]): HTMLElement {
  const line = el("div", "trace");
  line.append(el("span", "trace-mark", "✎"), " saved");
  for (const slug of slugs) {
    line.append(" ", el("span", "trace-id", slug));
  }
  return line;
}

function speaker(role: Message["role"]): HTMLElement {
  return el("div", "speaker", role === "user" ? "MITUL" : "ᛟ ODYN");
}

function fill(node: HTMLElement, content: string, cursor: boolean): void {
  // A lone cursor under the speaker reads as idle; the wait line stands in.
  if (cursor && content === "") {
    node.replaceChildren(waiting());
    return;
  }
  const interrupted = content.endsWith(INTERRUPTED);
  const blocks = renderMarkdown(
    interrupted ? content.slice(0, -INTERRUPTED.length) : content,
  );
  node.replaceChildren(...blocks);
  // Both marks belong at the end of the last line, unless that line is code.
  const last = blocks[blocks.length - 1];
  const tail = last instanceof HTMLParagraphElement ? last : node;
  if (interrupted) tail.append(" ", el("span", "interrupted", "(interrupted)"));
  if (cursor) tail.append(el("span", "cursor"));
}

function failed(message: string): HTMLElement {
  const line = el("div", "stream-error", `${message} · `);
  const link = el("button", "retry", "retry");
  link.addEventListener("click", resend);
  line.append(link);
  return line;
}

// DESIGN.md §5.1: one mono status line fused to the top of the composer.
function ledger(): HTMLElement {
  const line = el("div", "ledger");
  line.append(el("span", "ledger-label", "CONTEXT"));
  if (state.ledger.error !== null) {
    line.append(el("span", "ledger-note", state.ledger.error));
    return line;
  }
  const preview = state.ledger.preview;
  if (preview === null) return line;
  if (!preview.active) {
    line.append(el("span", "ledger-note", "/brain recalls memory · /memory saves one"));
    return line;
  }
  if (preview.memories.length === 0) {
    line.append(el("span", "ledger-note", "/brain · nothing to recall yet"));
    return line;
  }

  const visible = state.ledger.expanded
    ? preview.memories
    : preview.memories.slice(0, CHIP_LIMIT);
  for (const item of visible) {
    const chip = el("span", "chip chip-epi");
    chip.append("◈ ", item.id, " ", el("span", "chip-tokens", String(item.tokens)));
    chip.dataset.tip = item.content;
    line.append(chip);
  }
  const rest = preview.memories.slice(CHIP_LIMIT);
  if (!state.ledger.expanded && rest.length > 0) {
    const total = rest.reduce((sum, item) => sum + item.tokens, 0);
    const more = el("button", "chip chip-epi chip-more");
    more.append(`◈ +${rest.length} more `, el("span", "chip-tokens", String(total)));
    more.addEventListener("click", expandLedger);
    line.append(more);
  }
  line.append(
    el("span", "ledger-total", `${count(preview.tokens)} / ${count(preview.cap_tokens)} tk`),
  );
  return line;
}

const count = (tokens: number): string => tokens.toLocaleString("en-US");

function composer(): HTMLElement {
  const box = el("div", "composer");
  const field = el("div", "composer-field");
  const glyph = el("button", "send", "↵");
  glyph.addEventListener("click", submit);
  field.append(hint.wrap, glyph);
  box.append(ledger(), field, foot());
  return box;
}

function foot(): HTMLElement {
  const line = el("div", "composer-foot");
  const current = selected();
  if (current !== undefined) {
    const picks = el("span", "composer-picks");
    picks.append(renderBrevity(current), ...renderPickers(current));
    line.append(picks);
  }
  line.append(
    el("span", "composer-hints", `⏎ send · ⇧⏎ newline · ${MOD}K spotlight`),
  );
  return line;
}

function selected(): Conversation | undefined {
  return state.conversations.find((row) => row.id === state.selected);
}

function submit(): void {
  const text = input.value.trim();
  if (text === "" || streaming()) return;
  input.value = "";
  grow();
  drawMention();
  void send(text);
}

function grow(): void {
  input.style.height = "auto";
  input.style.height = `${input.scrollHeight}px`;
}

function atBottom(node: HTMLElement): boolean {
  return node.scrollHeight - node.scrollTop - node.clientHeight < SLACK;
}

function empty(): HTMLElement {
  return el("div", "empty", `${MOD}K anywhere · or start here`);
}
