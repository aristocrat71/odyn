// How far off a pending reminder is, in the coarsest unit that still reads.
export function untilLabel(dueAt: number): string {
  const seconds = dueAt - Math.floor(Date.now() / 1000);
  if (seconds <= 0) return "overdue";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `in ${Math.max(1, minutes)}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `in ${hours}h`;
  return `in ${Math.round(hours / 24)}d`;
}

// A reminder time as the user reads it: a clock today, a dated clock beyond.
export function dueLabel(dueAt: number): string {
  const at = new Date(dueAt * 1000);
  const now = new Date();
  const clock = at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const sameDay =
    at.getFullYear() === now.getFullYear() &&
    at.getMonth() === now.getMonth() &&
    at.getDate() === now.getDate();
  if (sameDay) return clock;
  return `${at.toLocaleDateString([], { month: "short", day: "numeric" })} ${clock}`;
}
