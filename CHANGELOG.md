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
