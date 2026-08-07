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
