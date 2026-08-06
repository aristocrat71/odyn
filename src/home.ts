import { prefillComposer } from "./chat";
import { el } from "./dom";
import { setView, state, type View } from "./state";

type Command = {
  cmd: string;
  view: View;
  hint: string;
};

const COMMANDS: Command[] = [
  { cmd: "/chat", view: "chat", hint: "the conversation" },
  { cmd: "/providers", view: "providers", hint: "models, endpoints and keys" },
  { cmd: "/brain", view: "brain", hint: "what odyn remembers" },
  { cmd: "/guide", view: "guide", hint: "how everything works" },
  { cmd: "/config", view: "config", hint: "the file behind it all" },
];

// The input outlives redraws, like the composer: the draft and caret stay.
const input = el("input", "home-input");
input.placeholder = "type a command, or just start talking…";
input.setAttribute("aria-label", "command");

// Which suggestion the arrows are on; reset when the text changes.
let active = 0;
let list: HTMLElement | null = null;

input.addEventListener("input", () => {
  active = 0;
  refill();
});

input.addEventListener("keydown", (event) => {
  const shown = matches();
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    if (shown.length === 0) return;
    const delta = event.key === "ArrowDown" ? 1 : -1;
    active = (active + delta + shown.length) % shown.length;
    refill();
    return;
  }
  if (event.key === "Escape") {
    input.value = "";
    active = 0;
    refill();
    return;
  }
  if (event.key !== "Enter") return;
  event.preventDefault();
  const text = input.value.trim();
  const chosen = shown[active];
  if (chosen !== undefined) {
    run(chosen);
    return;
  }
  // Not a command: it was the first message all along.
  if (text !== "" && !text.startsWith("/")) {
    prefillComposer(text);
    input.value = "";
    setView("chat");
  }
});

export function renderHome(): HTMLElement {
  const view = el("div", "home");
  const box = el("div", "home-box");
  box.append(
    el("div", "home-rune", "ᛟ"),
    el("div", "home-title", "ODYN"),
    input,
  );
  list = el("div", "home-commands");
  refill();
  box.append(list);
  view.append(box);
  queueMicrotask(() => input.focus());
  return view;
}

// An empty or "/"-leading draft offers commands; more letters narrow them.
function matches(): Command[] {
  const text = input.value.trim().toLowerCase();
  if (!text.startsWith("/")) return text === "" ? COMMANDS : [];
  return COMMANDS.filter((command) => command.cmd.startsWith(text));
}

function refill(): void {
  if (list === null) return;
  const shown = matches();
  if (active >= shown.length) active = 0;
  list.replaceChildren(
    ...shown.map((command, index) => {
      const row = el("button", "home-cmd");
      if (index === active) row.classList.add("active");
      row.append(
        el("span", "home-cmd-name", command.cmd),
        el("span", "home-cmd-hint", command.hint),
      );
      row.addEventListener("click", () => run(command));
      row.addEventListener("pointerenter", () => {
        active = index;
        refill();
      });
      return row;
    }),
  );
  const conversations = state.conversations.length;
  if (input.value.trim() === "" && conversations > 0) {
    const line = conversations === 1 ? "1 conversation" : `${conversations} conversations`;
    list.append(el("div", "home-note", line));
  }
}

function run(command: Command): void {
  input.value = "";
  active = 0;
  setView(command.view);
}
