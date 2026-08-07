import { el } from "./dom";

// Inline completion: the rest of what is being typed, drawn where the caret is,
// ⇥ to take it. The layer floats over the field with everything already typed
// repeated in transparent text, so the remainder starts exactly where the real
// text ends — the field itself only ever holds what was actually typed, which
// keeps the filtering and the `/brain` rule honest.
export type Field = HTMLInputElement | HTMLTextAreaElement;

export type Ghost = {
  wrap: HTMLElement;
  /// `true` while a completion is on screen, which is also when ⇥ has work to
  /// do — a list marks its highlighted row with the key only then.
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
      // A field tall enough to scroll has scrolled its text; the mirror has to
      // follow or the ghost lands a line or two off.
      layer.scrollTop = field.scrollTop;
      return tail !== "";
    },
  };
}

/// Writes the completion into the field. `false` when there was none, so the
/// caller can leave ⇥ to the browser and let focus move on.
export function accept(field: Field, full: string | undefined): boolean {
  const tail = suffix(field, full);
  if (tail === "") return false;
  field.value += tail;
  const end = field.value.length;
  field.setSelectionRange(end, end);
  return true;
}

// What is left of the completion, measured against the word under the caret —
// which is the whole field for a `/` command, and the last word for a mention
// typed at the end of a message. Nothing is offered unless the caret sits at
// the very end, so an edit in the middle is never guessed at.
function suffix(field: Field, full: string | undefined): string {
  const value = field.value;
  if (full === undefined) return "";
  if (field.selectionStart !== null && field.selectionStart !== value.length) return "";
  const word = /\S*$/.exec(value)?.[0] ?? "";
  if (word === "" || full.length <= word.length) return "";
  return full.toLowerCase().startsWith(word.toLowerCase()) ? full.slice(word.length) : "";
}
