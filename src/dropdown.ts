import "./dropdown.css";

import { el } from "./dom";

// The one dropdown for the whole app: native selects render as the OS's own
// menus, which no design language can reach. The menu exists only while open.

export type DropdownItem = {
  value: string;
  /// Shown in the row; `value` stands in when absent.
  label?: string;
  hint?: string;
};

export type Dropdown = {
  root: HTMLElement;
  set: (items: DropdownItem[], selected: string) => void;
  value: () => string;
  setDisabled: (disabled: boolean) => void;
};

// One menu open at a time, closed by any click outside it: a single document
// listener serves every instance.
let openNow: { root: HTMLElement; close: () => void } | null = null;

document.addEventListener("pointerdown", (event) => {
  if (openNow === null) return;
  if (event.target instanceof Node && openNow.root.contains(event.target)) return;
  openNow.close();
});

/// Closes whichever menu is open. Callers with their own Escape semantics
/// check this first: the menu goes before the window.
export function closeOpenDropdown(): boolean {
  if (openNow === null) return false;
  openNow.close();
  return true;
}

export function dropdown(opts: {
  /// Static prefix inside the trigger, e.g. "provider".
  label?: string;
  /// What the trigger says with nothing to select.
  empty?: string;
  onPick: (value: string) => void;
}): Dropdown {
  const root = el("span", "drop-wrap");
  const trigger = el("button", "drop-trigger");
  trigger.type = "button";
  const menu = el("div", "drop-menu");
  root.append(trigger);

  let items: DropdownItem[] = [];
  let selected = "";

  const drawTrigger = (): void => {
    trigger.replaceChildren();
    if (opts.label !== undefined) trigger.append(el("span", "drop-label", opts.label));
    const shown = items.find((item) => item.value === selected);
    trigger.append(
      shown?.label ?? (selected !== "" ? selected : (opts.empty ?? "—")),
      el("span", "drop-caret", " ▾"),
    );
  };

  const close = (): void => {
    menu.remove();
    if (openNow?.root === root) openNow = null;
  };

  const pick = (value: string): void => {
    selected = value;
    drawTrigger();
    close();
    trigger.focus();
    opts.onPick(value);
  };

  const fill = (): void => {
    menu.replaceChildren(
      ...items.map((item) => {
        const row = el("button", "drop-item");
        row.type = "button";
        row.append(
          el("span", "drop-mark", item.value === selected ? "●" : ""),
          item.label ?? item.value,
        );
        if (item.hint !== undefined) row.append(el("span", "drop-hint", item.hint));
        row.addEventListener("click", () => pick(item.value));
        return row;
      }),
    );
  };

  const open = (): void => {
    if (items.length === 0) return;
    openNow?.close();
    fill();
    root.append(menu);
    openNow = { root, close };
    // Focus lands on the selection, so the arrows start from it.
    queueMicrotask(() => {
      const rows = menu.querySelectorAll<HTMLButtonElement>(".drop-item");
      const at = items.findIndex((item) => item.value === selected);
      rows[Math.max(at, 0)]?.focus();
    });
  };

  trigger.addEventListener("click", () => {
    if (menu.isConnected) close();
    else open();
  });

  root.addEventListener("keydown", (event) => {
    if (!menu.isConnected) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        open();
      }
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close();
      trigger.focus();
      return;
    }
    const delta = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
    if (delta === 0) return;
    event.preventDefault();
    const rows = [...menu.querySelectorAll<HTMLButtonElement>(".drop-item")];
    const at = rows.findIndex((row) => row === document.activeElement);
    rows[(at + delta + rows.length) % rows.length]?.focus();
  });

  drawTrigger();
  return {
    root,
    set: (next, value) => {
      items = next;
      selected = value;
      drawTrigger();
      if (menu.isConnected) fill();
    },
    value: () => selected,
    setDisabled: (disabled) => {
      trigger.disabled = disabled;
      if (disabled) close();
    },
  };
}
