export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className !== undefined) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

const TRACE_LIMIT = 6;
// Keyed per trace, so an expansion survives the redraws streaming causes.
const expanded = new Set<string>();

export const forgetTraces = (): void => expanded.clear();

// DESIGN.md §5: `◈ used e-0142 · e-0087` — mark and ids teal, the rest dim.
// A longer recall folds at TRACE_LIMIT behind `show all`.
export function trace(
  mark: string,
  label: string,
  ids: string[],
  key: string,
): HTMLElement {
  const line = el("div", "trace");
  line.append(el("span", "trace-mark", mark), ` ${label} `);
  const shown = expanded.has(key) ? ids : ids.slice(0, TRACE_LIMIT);
  shown.forEach((id, index) => {
    if (index > 0) line.append(el("span", "trace-sep", " · "));
    line.append(el("span", "trace-id", id));
  });
  if (ids.length > shown.length) {
    const more = el("button", "trace-more", `show all ${ids.length - shown.length}`);
    more.addEventListener("click", () => {
      expanded.add(key);
      line.replaceWith(trace(mark, label, ids, key));
    });
    line.append(el("span", "trace-sep", " · "), more);
  }
  return line;
}

// Recall then provider latency leave seconds of nothing to draw before the
// first token. Chat and spotlight both show this, so neither sits blank.
export function waiting(): HTMLElement {
  const line = el("div", "waiting");
  line.append(el("span", "waiting-word", "thinking"));
  const dots = el("span", "waiting-dots");
  for (let i = 0; i < 3; i += 1) dots.append(el("i", "waiting-dot"));
  line.append(dots);
  return line;
}
