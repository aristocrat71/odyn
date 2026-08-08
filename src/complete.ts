import { el } from "./dom";

// Inline completion, ⇥ to take it. A layer repeats the typed text transparently
// so the remainder starts where the real text ends; the field holds only what
// was actually typed.
export type Field = HTMLInputElement | HTMLTextAreaElement;

export type Ghost = {
  wrap: HTMLElement;
  /// `true` while a completion is on screen, which is when ⇥ has work to do.
  draw: (full: string | undefined) => boolean;
};

export function ghost(field: Field, className: string): Ghost {
  const wrap = el("div", `ghost-wrap ${className}`);
  // A textarea wraps and grows, so its mirror has to wrap the same way.
  const multi = field instanceof HTMLTextAreaElement;
  const layer = el("div", multi ? "ghost multi" : "ghost");
  const typed = el("span", "ghost-typed");
  const rest = el("span", "ghost-rest");
  layer.append(typed, rest);
  // A no-op while the field is still detached, as the composer's is on load.
  field.replaceWith(wrap);
  wrap.append(layer, field);
  return {
    wrap,
    draw(full) {
      const tail = suffix(field, full);
      typed.textContent = tail === "" ? "" : field.value;
      rest.textContent = tail;
      // A scrolled field has scrolled its text, so the mirror must follow.
      layer.scrollTop = field.scrollTop;
      return tail !== "";
    },
  };
}

/// Writes the completion in. `false` when there was none, so ⇥ falls through.
export function accept(field: Field, full: string | undefined): boolean {
  const tail = suffix(field, full);
  if (tail === "") return false;
  field.value += tail;
  const end = field.value.length;
  field.setSelectionRange(end, end);
  return true;
}

// What is left of the completion, measured against the word under the caret.
// Nothing is offered unless the caret sits at the very end.
function suffix(field: Field, full: string | undefined): string {
  const value = field.value;
  if (full === undefined) return "";
  if (field.selectionStart !== null && field.selectionStart !== value.length) return "";
  const word = /\S*$/.exec(value)?.[0] ?? "";
  if (word === "" || full.length <= word.length) return "";
  return full.toLowerCase().startsWith(word.toLowerCase()) ? full.slice(word.length) : "";
}
