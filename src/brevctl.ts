import type { BrevityLevel, Conversation } from "./api";
import { el } from "./dom";
import { chooseBrevity, closeBrevityMenu, state, toggleBrevityMenu } from "./state";

const LEVELS: { level: BrevityLevel; hint: string }[] = [
  { level: "off", hint: "natural prose" },
  { level: "lite", hint: "trim filler" },
  { level: "full", hint: "fragments, substance only" },
  { level: "ultra", hint: "minimum viable words" },
];

// Same lifetime rules as the model picker: trigger and menu outlive redraws.
const wrap = el("div", "picker-wrap");
const trigger = el("button", "picker brevity-trigger");
const menu = el("div", "picker-menu brevity-menu");
wrap.append(trigger);

trigger.addEventListener("click", toggleBrevityMenu);

document.addEventListener("pointerdown", (event) => {
  if (event.target instanceof Node && wrap.contains(event.target)) return;
  closeBrevityMenu();
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeBrevityMenu();
});

export function renderBrevity(current: Conversation): HTMLElement {
  const effective: BrevityLevel =
    current.brevity ?? state.status?.brevity_default ?? "off";
  trigger.replaceChildren(
    el("span", "picker-provider", "brevity "),
    `${effective} ▾`,
  );
  trigger.classList.toggle("off", effective === "off");
  if (!state.brevityMenu) {
    menu.remove();
    return wrap;
  }
  menu.replaceChildren(
    ...LEVELS.map(({ level, hint }) => {
      const item = el("button", "picker-item");
      item.append(
        el("span", "picker-mark", level === effective ? "●" : ""),
        el("span", "picker-name", level),
        el("span", "picker-meta", hint),
      );
      item.addEventListener("click", () => void chooseBrevity(level));
      return item;
    }),
  );
  wrap.append(menu);
  return wrap;
}
