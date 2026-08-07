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

// Between the send and the first token there is recall, then the provider's
// own latency — seconds of nothing to draw. Chat and spotlight both show this
// line until text starts arriving, so neither ever sits blank.
export function waiting(): HTMLElement {
  const line = el("div", "waiting");
  line.append(el("span", "waiting-word", "thinking"));
  const dots = el("span", "waiting-dots");
  for (let i = 0; i < 3; i += 1) dots.append(el("i", "waiting-dot"));
  line.append(dots);
  return line;
}
