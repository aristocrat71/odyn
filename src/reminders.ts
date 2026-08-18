import type { ReminderRow } from "./api";
import { el } from "./dom";
import { dueLabel, untilLabel } from "./due";
import { cancelReminder, state } from "./state";

export function renderReminders(): HTMLElement {
  const root = el("div", "reminders");
  const list = state.reminders.list;
  if (list === null) return root;
  if (list.pending.length === 0 && list.past.length === 0) {
    root.append(empty());
    return root;
  }
  if (list.pending.length > 0) {
    root.append(section("waiting", list.pending.map(pendingRow)));
  }
  if (list.past.length > 0) {
    root.append(section("shown", list.past.map(pastRow)));
  }
  return root;
}

function empty(): HTMLElement {
  const box = el("div", "reminders-empty");
  box.append(
    el("div", "reminders-rune", "ᛟ"),
    el("div", undefined, "nothing to remind you of yet"),
    el("div", "reminders-hint", "mention /reminder in a message to set one"),
  );
  return box;
}

function section(name: string, rows: HTMLElement[]): HTMLElement {
  const box = el("div", "reminders-section");
  box.append(el("div", "reminders-label", name));
  for (const row of rows) box.append(row);
  return box;
}

function pendingRow(row: ReminderRow): HTMLElement {
  const line = el("div", "rem-row");
  line.append(el("span", "rem-mark epi", "◔"), el("span", "rem-text", row.text));
  if (row.repeat) line.append(el("span", "rem-every", row.repeat));
  line.append(
    el("span", "rem-until", untilLabel(row.due_at)),
    el("span", "rem-at", dueLabel(row.due_at)),
  );
  const actions = el("span", "rem-actions");
  const remove = el("button", "rem-delete", "✕");
  remove.addEventListener("click", () => void cancelReminder(row.id));
  actions.append(remove);
  line.append(actions);
  return line;
}

function pastRow(row: ReminderRow): HTMLElement {
  const line = el("div", "rem-row past");
  line.append(
    el("span", "rem-mark", "·"),
    el("span", "rem-text", row.text),
    el("span", "rem-at", dueLabel(row.due_at)),
  );
  return line;
}
