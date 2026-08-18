# Additional feature plans

Where the open issues go from here, in build order. The first batch —
soul.md (#28), full markdown (#55), conversation FTS (#21), tool-capability
probe (#31) — shipped on `feat/improvement-features`. Everything below is
planned, not built.

## Next arc

### Recurring reminders and scheduled asks (#24)

Extends the freshest surface: the reminder rows, the clock thread in
`odyn-app/src/reminders.rs`, and the spotlight takeover.

- Migration: `ALTER TABLE reminders ADD repeat TEXT` — an `every`-phrase
  (`every day 09:00`, `every monday 9:30`, `every 45m`), NULL for one-shots.
  Parsing lives beside the existing time words in `core/reminder.rs`.
- Firing a repeating reminder computes the next `due_at` and re-arms instead
  of setting `fired_at`. Ticks missed while the Mac slept fire once and
  re-arm from now (catch-up clamped to one firing, never a backlog).
- `set_reminder` gains an optional `every` argument; the reminders view shows
  recurrence and the next fire time; cancel works unchanged.
- Scheduled asks: a sibling `schedules` table — prompt, provider, model,
  `every`, next_at, last_error. When due, the prompt runs through
  `tools::run_turn` and lands as a normal conversation (usage recorded,
  nothing hidden); the due notification row opens it. The morning brief is
  the canonical example.

### Agent mode: shell and file tools in a scoped workspace (#34)

The biggest capability jump; seven other issues stack on it.

- Per-conversation agent mode (or a `/do` mention) hands the model
  `read_file`, `write_file`, `edit_file`, `ls`, `glob`, `grep`, `bash` —
  all scoped to a user-picked workspace folder with path-containment checks.
  `tokio::process` for exec; no new dependencies.
- Command approval inline in chat: a mono row with the exact command,
  run / deny, plus a per-conversation allowlist. A hard blocklist floor no
  mode disables. Runaway guard: the same call repeating ~15× trips the turn.
- Raised tool-round budget for agent turns with a visible round counter; no
  mid-task pressure warnings, one wrap-up call at exhaustion.
- Follow-ups once it exists (separate issues, this order): checkpoints
  before edits (#45), detached background jobs (#44), steer-a-running-turn
  (#46), subagent delegation (#43), programmatic tool calling (#42),
  goal loop / heartbeat (#47), skills (#36).

## Prerequisites to slot in when their moment comes

### Guard-wrap retrieved content (#49) — before /web, documents, or MCP

- One wrapping helper all retrieved-content injection goes through:
  delimits web pages, document chunks and third-party tool output as data,
  not instructions, with markers the model is told never to echo.
- Taint rule for agent mode: after untrusted content enters a turn, a
  world-read tool and a memory-write tool are never both live; mutating
  calls require explicit confirmation regardless of allowlists.
- Invisible-unicode / injection scan on model-authored note writes in
  `notes.rs` (memory lands in the system prompt).

### Auxiliary model routing (#52) — right before the first background feature

- `[aux]` config table: `default = "ollama/llama3.2:3b"`, overridable per
  task (`suggest`, `tidy`, `compact`, `judge`). Unset task → aux default;
  no aux default → the conversation's model (today's behavior).
- One resolver in `config.rs`; every background feature routes through it
  from day one. Aux usage recorded per task.

## Then

### /web — mention-gated web search (#22)

- `[search]` config mirroring the provider shape; keyless first-class
  (self-hosted SearXNG is just a URL), Brave/Tavily as keyed options.
- `/web` earns a `web_search` tool for the turn — the trigger checklist in
  `brain.rs` + `src/mentions.ts` applies unchanged. Top-N snippets return as
  the tool result; the ledger itemizes them with token counts.
- Page fetching (if added) gets SSRF guards: DNS pinning, body budgets.
  Depends on #49 landing first.

### /research — iterative deep research (#23)

- Builds on /web: think → search → read → synthesize with a hard iteration
  cap, progress as plain mono lines in the chat, cancellable from the
  message, citations plus a ledger entry for every snippet that entered
  context. Runs survive view switches.

### Consent-first memory suggestions (#25)

- After a turn (idle machine, every N turns, aux model), one extraction call
  proposes memories as quiet chips under the answer: `remember? slug ✓ ✕`.
  Accept routes through the existing save path; dismiss latches per
  proposal. Off by default, one config key.
- Anti-capture rules encoded in the prompt: never persist
  environment-dependent failures or "tool X doesn't work"; "nothing to
  save" is a real option.

### Brain tidy (#26)

- A user-triggered `tidy` action in the brain view: an aux-model pass over
  the notes proposing merges, rewrites and trashes. Every proposal is a
  diff; apply is per-item; deletion is the existing `.trash/` move.
  Wikilinks to a merged note rewrite to the survivor.
- A fingerprint of the last-audited state short-circuits re-runs on an
  unchanged brain. `pinned: true` frontmatter is exempt from everything.

### Conversation compaction (#27)

- Opt-in per conversation. Past a threshold, older assistant turns fold
  into one running summary block; user messages and the first exchange stay
  verbatim — instructions are never paraphrased.
- The summary is a ledger item (`◈ summary N`), the header shows
  `compacted · n turns folded`, and storage is untouched — compaction
  changes what is sent, never what is kept. Summarization goes through the
  aux resolver.

### Document ingestion (#29)

- A `documents/` subfolder in the brain dir: files are extracted locally
  (PDF text layer, markdown, plain text — nothing else at first), chunked
  sentence-aware, embedded into the existing index as document chunks.
- Recall stays mention-gated; the ledger itemizes chunks like memories
  (`◈ paper.pdf#3 88`). Chunks never become graph nodes; the document can
  be one. No OCR, no Office formats until this proves out.

### Reminder delivery channels (#50)

- `[notify]`: an ntfy topic URL and/or an HMAC-signed webhook. Reminder rows
  gain an optional channel; default stays the spotlight takeover; delivery
  failure falls back to local and says so in the reminders view. Pairs with
  #24 — a morning brief on the phone is the point.

### API keys in the OS keychain (#53)

- New dependency to state first: `keyring` (macOS Keychain via the security
  framework; Linux/Windows backends when those builds happen).
- Pasted keys go to the keychain; `odyn.toml` holds `api_key_ref =
  "keychain"`. Existing plaintext keys migrate on first launch with a
  comment left behind. `api_key_env` stays; optional `api_key_cmd = "op
  read …"` for external managers.

## Deferred, deliberately

- **Telegram bridge (#37)** — the biggest strategic ceiling (odyn from your
  phone, long-polling, one-user allowlist), but it changes what odyn is;
  decide it on purpose, not as a queue slot.
- **MCP client (#35) → email (#58)** — email is gated on MCP, and MCP is a
  large client; revisit after agent mode proves the tool loop.
- **Vision (#38), voice in/out (#32, #41), compare mode (#39), forking
  (#40), local model advisor (#54), usage view (#33), export/backup (#30),
  calendar (#56), todos (#57), import (#59)** — all still wanted, none
  urgent; #59 now has its FTS prerequisite, #30 is the cheapest of the set
  if a quiet week wants a small one.
