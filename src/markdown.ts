import "./markdown.css";

import { invoke } from "@tauri-apps/api/core";

import { el } from "./dom";

// The whole markdown Odyn renders. Nothing is ever set as HTML, so a model
// cannot write into the DOM.

const FENCE = "```";

const ITEM = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const HEADING = /^(#{1,6})\s+(.*)$/;
const TABLE_SEP = /^\s*\|?[\s:|-]*-[\s:|-]*\|?\s*$/;
const HR = /^ {0,3}(-{3,}|\*{3,}|_{3,})\s*$/;
const SPAN =
  /\[([^\]\n]+)\]\(([^)\s]+)\)|(\*\*|__)([^\n]+?)\3|~~([^\n]+?)~~|\*([^*\n]+?)\*|(?<![\w_])_([^_\n]+?)_(?![\w_])/;

type Seg = { kind: "code" | "prose"; src: string };

export function renderMarkdown(text: string): HTMLElement[] {
  return segments(text).flatMap(render);
}

// Frozen-block streaming: every segment but the growing tail keeps its DOM
// node across deltas, so a long answer never re-parses whole.
const caches = new WeakMap<HTMLElement, { segs: Seg[]; nodes: HTMLElement[][] }>();

export function renderInto(node: HTMLElement, text: string): void {
  const segs = segments(text);
  const cached = caches.get(node) ?? { segs: [], nodes: [] };
  let keep = 0;
  while (
    keep < segs.length - 1 &&
    keep < cached.segs.length &&
    cached.segs[keep]?.kind === segs[keep]?.kind &&
    cached.segs[keep]?.src === segs[keep]?.src
  ) {
    keep += 1;
  }
  const nodes = cached.nodes.slice(0, keep);
  for (const seg of segs.slice(keep)) nodes.push(render(seg));
  caches.set(node, { segs, nodes });
  node.replaceChildren(...nodes.flat());
}

// Odd sections sit between fences; one left open is a block still streaming.
function segments(text: string): Seg[] {
  const out: Seg[] = [];
  text.split(FENCE).forEach((section, index) => {
    if (index % 2 === 1) {
      out.push({ kind: "code", src: section });
      return;
    }
    for (const part of section.split(/\n[ \t]*\n/)) {
      const chunk = part.trim();
      if (chunk !== "") out.push({ kind: "prose", src: chunk });
    }
  });
  return out;
}

const render = (seg: Seg): HTMLElement[] =>
  seg.kind === "code" ? [fenced(seg.src)] : prose(seg.src);

function fenced(section: string): HTMLElement {
  const block = el("pre", "code");
  // The opening line names a language, if it names anything.
  block.append(el("code", undefined, section.replace(/^[^\n]*\n/, "").trimEnd()));
  return block;
}

function prose(chunk: string): HTMLElement[] {
  const blocks: HTMLElement[] = [];
  const lines = chunk.split("\n");
  let at = 0;
  while (at < lines.length) {
    const line = lines[at] ?? "";
    const heading = HEADING.exec(line);
    if (heading !== null) {
      const level = heading[1]?.length ?? 1;
      const head = el(`h${level}` as "h1", `md-h md-h${level}`);
      inline(head, heading[2] ?? "");
      blocks.push(head);
      at += 1;
      continue;
    }
    if (HR.test(line)) {
      blocks.push(el("hr", "md-hr"));
      at += 1;
      continue;
    }
    if (line.startsWith(">")) {
      const quoted: string[] = [];
      while (lines[at]?.startsWith(">")) {
        quoted.push((lines[at] ?? "").replace(/^>\s?/, ""));
        at += 1;
      }
      const quote = el("blockquote", "md-quote");
      inline(quote, quoted.join("\n"));
      blocks.push(quote);
      continue;
    }
    if (ITEM.test(line)) {
      const items: string[] = [];
      while (at < lines.length && (lines[at] ?? "") !== "" && !HEADING.test(lines[at] ?? "")) {
        items.push(lines[at] ?? "");
        at += 1;
      }
      blocks.push(list(items));
      continue;
    }
    if (line.includes("|") && TABLE_SEP.test(lines[at + 1] ?? "")) {
      const rows: string[] = [];
      while (at < lines.length && (lines[at] ?? "").includes("|")) {
        rows.push(lines[at] ?? "");
        at += 1;
      }
      blocks.push(table(rows));
      continue;
    }
    const plain: string[] = [];
    while (at < lines.length && !breaks(lines[at] ?? "", lines[at + 1] ?? "")) {
      plain.push(lines[at] ?? "");
      at += 1;
    }
    const paragraph = el("p", "para");
    inline(paragraph, plain.join("\n"));
    blocks.push(paragraph);
  }
  return blocks;
}

// Where a paragraph ends without a blank line: a heading, quote, list item or
// table start on the next line begins its own block.
function breaks(line: string, next: string): boolean {
  return (
    HEADING.test(line) ||
    line.startsWith(">") ||
    ITEM.test(line) ||
    HR.test(line) ||
    (line.includes("|") && TABLE_SEP.test(next))
  );
}

function list(lines: string[]): HTMLElement {
  const root = el("div", "md-list-root");
  // One open list per indent depth; an item deeper than the last nests inside it.
  const stack: { list: HTMLElement; indent: number }[] = [];
  for (const line of lines) {
    const match = ITEM.exec(line);
    if (match === null) {
      // A continuation line joins the item before it.
      const open = stack[stack.length - 1]?.list.lastElementChild;
      if (open !== null && open !== undefined) inline(open as HTMLElement, `\n${line.trim()}`);
      continue;
    }
    const indent = match[1]?.length ?? 0;
    const ordered = /^\d/.test(match[2] ?? "");
    while (stack.length > 0 && indent < (stack[stack.length - 1]?.indent ?? 0)) stack.pop();
    let top = stack[stack.length - 1];
    if (top === undefined || indent > top.indent) {
      const fresh = el(ordered ? "ol" : "ul", "md-list");
      (top === undefined ? root : (top.list.lastElementChild ?? top.list)).append(fresh);
      top = { list: fresh, indent };
      stack.push(top);
    }
    const item = el("li", "md-item");
    inline(item, match[3] ?? "");
    top.list.append(item);
  }
  return root;
}

function table(rows: string[]): HTMLElement {
  const wrap = el("div", "md-table-wrap");
  const node = el("table", "md-table");
  const aligns = cells(rows[1] ?? "").map((sep) => {
    if (sep.startsWith(":") && sep.endsWith(":")) return "center";
    if (sep.endsWith(":")) return "right";
    return "";
  });
  const row = (line: string, tag: "th" | "td"): HTMLElement => {
    const tr = el("tr");
    cells(line).forEach((cell, index) => {
      const box = el(tag);
      const align = aligns[index];
      if (align !== undefined && align !== "") box.style.textAlign = align;
      inline(box, cell);
      tr.append(box);
    });
    return tr;
  };
  const head = el("thead");
  head.append(row(rows[0] ?? "", "th"));
  const body = el("tbody");
  for (const line of rows.slice(2)) body.append(row(line, "td"));
  node.append(head, body);
  wrap.append(node);
  return wrap;
}

const cells = (line: string): string[] =>
  line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());

function inline(target: HTMLElement, text: string): void {
  text.split("`").forEach((part, index) => {
    if (part === "") return;
    if (index % 2 === 1) target.append(el("code", "code-inline", part));
    else target.append(...spans(part));
  });
}

function spans(text: string): (Node | string)[] {
  const out: (Node | string)[] = [];
  let rest = text;
  while (rest !== "") {
    const match = SPAN.exec(rest);
    if (match === null) {
      out.push(rest);
      break;
    }
    if (match.index > 0) out.push(rest.slice(0, match.index));
    if (match[1] !== undefined) out.push(link(match[1], match[2] ?? ""));
    else if (match[4] !== undefined) out.push(wrap("strong", match[4]));
    else if (match[5] !== undefined) out.push(wrap("del", match[5]));
    else out.push(wrap("em", match[6] ?? match[7] ?? ""));
    rest = rest.slice(match.index + match[0].length);
  }
  return out;
}

function wrap(tag: "strong" | "em" | "del", inner: string): HTMLElement {
  const node = el(tag);
  node.append(...spans(inner));
  return node;
}

// Only http(s) ever becomes a link; anything else keeps its words and loses
// the linkness. Clicks leave through the backend, never by navigation.
function link(label: string, url: string): Node {
  if (!/^https?:\/\//i.test(url)) {
    const plain = el("span");
    plain.append(...spans(label));
    return plain;
  }
  const anchor = el("a", "md-link");
  anchor.append(...spans(label));
  anchor.href = url;
  anchor.title = url;
  anchor.addEventListener("click", (event) => {
    event.preventDefault();
    void invoke("open_url", { url });
  });
  return anchor;
}
