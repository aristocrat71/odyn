# Changelog

All notable changes to Odyn are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
Odyn uses [semantic versioning](https://semver.org/).

**This file is published.** The release workflow extracts the section matching
the tag being built and uses it as the GitHub release body, which `tauri-action`
also copies into `latest.json` — so it becomes the release notes every installed
copy of Odyn sees when it checks for updates. A tag with no matching section here
fails the release before anything is built. Write the entry as you merge, not at
tag time.

## [0.3.0] - 2026-08-31

Spotlight is the whole of Odyn now. Chat is gone, the dashboard is a control
panel for the brain, reminders, providers and config, and the panel that used to
be one question and one answer holds a thread for as long as you keep it.

### Added

- **Follow-ups in spotlight.** An answered ask stays in the panel and the next
  one is read against it, so "where is it used?" is a question the model can
  answer. The last six exchanges are kept, and recall reads them too: a `/brain`
  follow-up that names nothing still retrieves against what was being talked
  about. The thread lives in memory and is never written down — Esc ends it and
  empties the panel, `⌘⌫` does the same without hiding it, and hiding the panel
  any other way keeps it. A question asked while an answer is still streaming
  replaces it, and an ask that never got an answer is not kept.
- **Recurring reminders.** `every day 09:00`, `every monday 9:30`, `every 45m` —
  the reminder re-arms itself after each firing instead of being marked shown.
  One that came due while the machine slept fires once and re-arms from now,
  never as a backlog. The reminders view shows the recurrence and the next time
  it will fire; cancelling one works as it did.
- **Answers render as markdown.** Headings, nested lists, tables, quotes, rules,
  links and inline emphasis, on top of the fenced code that was already there —
  parsed as the stream flows, with every block but the growing tail kept as it
  was, so a long answer never re-parses whole. Nothing is ever set as HTML, so a
  model cannot write into the panel; only `http(s)` is ever a link, and clicking
  one leaves through the backend rather than by navigating the window.
- **A model that cannot call tools is named before the turn runs.** A mention
  that earns a tool — `/memory`, `/reminder` and the rest — aimed at an Ollama
  model whose daemon reports no `tools` capability is refused with `this model
  cannot call tools — the mention needs one that can`, instead of answering and
  quietly saving nothing. Only when the daemon says so: one too old to report
  capabilities lets the attempt proceed.

### Removed

- **Chat, conversations, the home view and the guide.** Asking happens in
  spotlight; the dashboard keeps brain, reminders, providers and config. The
  database drops its `conversations` and `messages` tables on first launch, and
  any conversation 0.2.0 stored goes with them. Spotlight's `⌘⏎` goes too — it
  promoted an answer into a conversation, and there is nothing to promote into.
- **`[style]`.** Brevity is a spotlight setting now, so `[spotlight] brevity` is
  the only one. A `[style]` section left in your `odyn.toml` parses and is
  ignored, as `[memory]` already was: a stale section must not brick the app.

### Fixed

- **A delta from a replaced ask no longer lands in the next answer.** Asking
  again while an answer was still streaming could leave the tail of the old
  stream attached to the new one. Each ask now owns its own stream, and anything
  from one that was replaced is dropped.

## [0.2.0] - 2026-08-12

Odyn can hold a time for you now, and it says so out loud when the time comes.

### Added

- **Reminders, on `/reminder`.** Mention it and the model is handed
  `set_reminder` for that turn: "remind me to call mum in 20 minutes" becomes a
  row with a time on it. It is the one mention that reads nothing from the
  brain — a reminder is not written from memory — so the turn never loads the
  embedder and recalls no notes. The time is given either as minutes from now or
  as a date and time, and minutes are preferred, since they need no reference
  clock and so cannot land on the wrong day. A time already past, or one more
  than five years out, comes back as an error the model can read and correct
  rather than as a reminder set wrong.
- **A due reminder takes over spotlight.** The panel appears with it — no field,
  no footer, just the reminder and a dismiss button — and a chime loops until
  you dismiss it, so one that arrives while you are elsewhere is still heard.
  Odyn checks every 20 seconds while it is running, which is what the tray icon
  is for: closing the window hides it, it does not quit. Nothing fires while
  Odyn is quit, and nothing is lost either — anything that came due meanwhile is
  shown at the next launch, because a reminder is marked shown only once the
  panel has taken it.
- **A reminders view.** `/view-reminders`, or the sidebar: what is still
  waiting, soonest first, with how far off each one is, and the last fifty
  already shown below that. Hovering a waiting row gives you a ✕ to cancel it.
  Reminders live in the database rather than the brain folder — they are state
  with a deadline, not memories, and they never enter recall.

### Fixed

- **Spotlight keeps its exchange when it is hidden.** A click away conceals the
  panel rather than emptying it, so a re-summon finds the answer still there.
  Esc and promotion remain the only two things that end an exchange — which is
  also what keeps a due reminder on screen until you dismiss it.

## [0.1.0] - 2026-08-08

The first build you can install. Odyn is a personal AI harness: a desktop app
over a pure-Rust core that talks only to open-weight models — any
OpenAI-compatible endpoint and a local Ollama — and never to a closed-weight
provider.

macOS on Apple Silicon for now — the embedding runtime behind the brain has no
Intel macOS build to link against. The app is not notarized yet either, so a
`.dmg` you download by hand needs one Gatekeeper override; the install script
handles that for you. Once it is installed, Odyn updates itself.

### Added

- **Chat, against your own endpoint.** Conversations with streaming answers,
  per-conversation model and brevity, and a mono status line fused to the
  composer. Reasoning is never part of an answer: a model that streams its
  thinking as `<think>…</think>` has it stripped as the stream flows.
- **Providers, connected with a key.** The providers view knows the endpoint and
  the model list for OpenRouter, Groq, Cerebras, DeepSeek, Together, Mistral,
  OpenCode Zen and a local Ollama, so a key is the whole of what it asks for.
  Paste one and a shape only one provider issues names its own; anything else is
  one click on a tile. Free models are listed first, everywhere.
- **Spotlight, on `⌥Space`.** A borderless panel that answers without taking
  focus from whatever you were doing, and promotes its answer into a full
  conversation when you want to keep going. On Wayland, where no such window can
  be summoned reliably, it falls back to an ordinary centered window.
- **The brain.** A folder of markdown notes that stays out of the context window
  until asked. Mention `/brain` and Odyn walks its memory graph — wikilinks,
  embedding similarity, shared use — for the notes that answer you, accounted for
  token by token before anything is sent. Every other message reaches the model
  bare. `/memory` saves a note, `/update-memory` rewrites one, `/delete-memory`
  moves one to `.trash`, `/link-memory` and `/unlink-memory` draw and take back a
  wikilink. One tool per mention — the choice between them is yours, not the
  model's.
- **A brain view, with the graph.** Read, search, add and edit notes; see what
  links to what; and switch the embedding model — bundled fastembed, a local
  Ollama, or a configured endpoint — from a picker that re-indexes on the spot.
- **`odyn.toml`, written and read as one thing.** The file is created from a
  built-in template the first time it is read, then parsed as the running
  configuration, so the file and the process can never disagree. Unknown keys are
  rejected by name. The settings views preserve your comments and formatting and
  validate the whole file before writing.
- **A menu bar app.** Odyn lives in the tray: **Open Odyn**, **Check for
  updates…**, **Quit**. Closing the dashboard only hides it, because the
  spotlight hotkey has to keep answering.
- **Auto-update.** The tray checks at launch, downloads what it finds, and offers
  a restart. Every build is signed, and an installed copy only accepts an update
  that verifies against the key baked into it.
