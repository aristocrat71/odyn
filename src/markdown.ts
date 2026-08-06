import { el } from "./dom";

// The whole markdown Odyn renders: paragraphs, fenced code blocks, inline code
// spans. Nothing is ever set as HTML, so a model cannot write into the DOM.

const FENCE = "```";

export function renderMarkdown(text: string): HTMLElement[] {
  const blocks: HTMLElement[] = [];
  // Odd sections sit between fences; a fence left open while streaming is a
  // code block that has not ended yet.
  text.split(FENCE).forEach((section, index) => {
    if (index % 2 === 1) {
      blocks.push(fenced(section));
      return;
    }
    for (const part of section.split(/\n[ \t]*\n/)) {
      const paragraph = part.trim();
      if (paragraph !== "") blocks.push(inline(paragraph));
    }
  });
  return blocks;
}

function fenced(section: string): HTMLElement {
  const block = el("pre", "code");
  // The opening line names a language, if it names anything.
  block.append(el("code", undefined, section.replace(/^[^\n]*\n/, "").trimEnd()));
  return block;
}

function inline(text: string): HTMLElement {
  const paragraph = el("p", "para");
  text.split("`").forEach((part, index) => {
    if (part === "") return;
    paragraph.append(index % 2 === 1 ? el("code", "code-inline", part) : part);
  });
  return paragraph;
}
