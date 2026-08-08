// The memory triggers, in one place. `brain::parse_ask` decides what they
// mean; every surface that completes or routes them must agree, and a surface
// that misses one dead-ends the message in its command parser.
export const MENTIONS = [
  "/brain",
  "/memory",
  "/update-memory",
  "/delete-memory",
  "/link-memory",
];

// Longest-first, so `/update-memory` is never read as `/memory`.
const NAMES = MENTIONS.map((mention) => mention.slice(1))
  .sort((a, b) => b.length - a.length)
  .join("|");
const PATTERN = new RegExp(`(^|\\s)/(${NAMES})([\\s.,;:!?]|$)`, "i");

// A memory mention is a message, never a command.
export const mentionAsk = (text: string): boolean => PATTERN.test(text);
