import { el } from "./dom";
import { openConfigInEditor, state } from "./state";

type Kind = "plain" | "key" | "section" | "string" | "comment";
type Token = { kind: Kind; text: string };

// A bare TOML key, and the `=` that ends it.
const ASSIGN = /^(\s*)([A-Za-z0-9_.-]+)(\s*=\s*)/;

export function renderConfig(): HTMLElement {
  const root = el("div", "config");
  const file = state.config.file;
  if (file !== null) {
    root.append(pathLine(file.path), toml(file.text));
  }
  root.append(
    el(
      "div",
      "config-note",
      "read-only · edit the file in your editor",
    ),
  );
  return root;
}

function pathLine(path: string): HTMLElement {
  const line = el("div", "config-path");
  const open = el("button", "config-open", "open in editor");
  open.addEventListener("click", () => void openConfigInEditor());
  line.append(el("span", "config-file", path), open);
  return line;
}

function toml(text: string): HTMLElement {
  const block = el("pre", "toml");
  for (const line of text.split("\n")) {
    for (const token of scan(line)) {
      block.append(
        token.kind === "plain"
          ? token.text
          : el("span", `toml-${token.kind}`, token.text),
      );
    }
    block.append("\n");
  }
  return block;
}

// Line-based on purpose: v1 config has no multi-line values.
function scan(line: string): Token[] {
  const start = line.trimStart();
  if (start.startsWith("#")) return [{ kind: "comment", text: line }];
  if (start.startsWith("[")) {
    const end = line.indexOf("]");
    if (end === -1) return [{ kind: "section", text: line }];
    return [
      { kind: "section", text: line.slice(0, end + 1) },
      ...value(line.slice(end + 1)),
    ];
  }
  const assign = ASSIGN.exec(line);
  if (assign === null) return [{ kind: "plain", text: line }];
  return [
    { kind: "plain", text: assign[1] },
    { kind: "key", text: assign[2] },
    { kind: "plain", text: assign[3] },
    ...value(line.slice(assign[0].length)),
  ];
}

// Quotes and `#` only mean what they mean outside a string.
function value(text: string): Token[] {
  const tokens: Token[] = [];
  let plain = "";
  const flush = (): void => {
    if (plain !== "") tokens.push({ kind: "plain", text: plain });
    plain = "";
  };
  for (let at = 0; at < text.length; at++) {
    const char = text[at];
    if (char === "#") {
      flush();
      tokens.push({ kind: "comment", text: text.slice(at) });
      return tokens;
    }
    if (char === '"' || char === "'") {
      const close = text.indexOf(char, at + 1);
      const end = close === -1 ? text.length : close + 1;
      flush();
      tokens.push({ kind: "string", text: text.slice(at, end) });
      at = end - 1;
      continue;
    }
    plain += char;
  }
  flush();
  return tokens;
}
