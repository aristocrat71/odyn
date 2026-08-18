# Odyn

Odyn is a personal AI harness: a Tauri desktop app over a pure-Rust core.
It talks only to open-weight models — any
OpenAI-compatible endpoint and local Ollama — and never to closed-weight
providers. The brain is a folder of markdown notes that stays out of the
context window until asked: mention `/brain` in a message and Odyn walks its
memory graph — wikilinks, embedding similarity, shared use — for the notes
that answer you, accounted for token by token before anything is sent. Every
other message reaches the model bare.

Give a conversation a workspace folder and it becomes an agent: the model gets
shell and file tools scoped to that folder. File tools are contained — they
cannot reach outside the workspace — while every bash command is shown to you
verbatim and waits for run / always / deny, over a blocklist floor that
refuses the catastrophic ones outright. The workspace is per-conversation
state, set from the chat header; nothing about it lives in the config file.

## Install

macOS on Apple Silicon:

```sh
curl -fsSL --connect-timeout 10 https://raw.githubusercontent.com/aristocrat71/odyn/main/install.sh | bash
```

That fetches the latest release, checks it against its published SHA-256, and
installs it to `/Applications`. Once installed, Odyn updates itself: the tray
checks at launch and offers a restart when it has staged a new version.

Or take the `.dmg` from the [releases
page](https://github.com/aristocrat71/odyn/releases/latest). Odyn isn't notarized
yet, so a hand-installed copy needs one **right-click → Open** — the script above
does that part for you.

Everywhere else, build from source (below). Intel Macs have no release build
because the bundled embedder's onnxruntime has no `x86_64-apple-darwin` binary to
link against.

## Layout

| Path | What it is |
| --- | --- |
| `crates/odyn-core` | All logic: config, providers, SQLite storage, the brain. |
| `crates/odyn-app` | Tauri v2 desktop app. `tauri.conf.json` lives here; there is no `src-tauri/`. |
| `crates/odyn-vec` | Registers the statically linked sqlite-vec extension. |
| `src/`, `index.html`, `spotlight.html` | Vanilla-TypeScript frontend, built by Vite. |

## Prerequisites

- **Rust stable.** `rust-toolchain.toml` pins the `stable` channel; rustup picks
  it up.
- **[bun](https://bun.sh).** Builds the frontend and provides the Tauri CLI.
- **Ollama** (optional). For local models. The default config points at
  `http://localhost:11434`.

Platform toolchains for the desktop app:

- macOS: Xcode Command Line Tools.
- Windows: MSVC build tools and WebView2 (preinstalled on Windows 11).
- Linux: the Tauri v2 system packages —

  ```sh
  sudo apt-get install -y \
    libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```

The first memory embedded downloads the embedding model
(bge-small-en-v1.5, ~100 MB) into the data directory. Nothing else in the build
or the tests touches the network beyond crates.io and npm.

## Build and run

```sh
bun install
bun run tauri dev      # Vite on http://localhost:1430 plus the Rust app
bun run tauri build    # release bundles under target/release/bundle
```

Run both from the repository root. `bun run tauri` uses the pinned
`@tauri-apps/cli` devDependency and locates `crates/odyn-app/tauri.conf.json` by
itself. `cargo tauri dev` / `cargo tauri build` are equivalent if you install the
same CLI as a cargo subcommand (`cargo install tauri-cli --version "^2"`);
nothing in the repository requires it.

`tauri dev` and `tauri build` run `bun run dev` / `bun run build` themselves, so
the frontend never has to be built by hand. A plain `cargo build --workspace`
compiles the app crate in dev mode and does not need `dist/` to exist.

The app lives in the tray under its own icon, with three items: **Open Odyn**
brings the dashboard back, **Check for updates…** is the whole update interface
(it relabels itself through downloading and lands on **Restart to finish
updating**; the launch check is silent unless it finds something), and **Quit**
ends the process. Closing the dashboard only
hides it — the spotlight hotkey has to keep answering — and on macOS the dock
icon goes with it, so odyn is a menu bar app until the dashboard is reopened. If
the tray cannot be created the close button quits instead, rather than leaving
odyn running with no way to reach it.

### Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bun run build
```

## Configuration

`odyn.toml` is written from a built-in template the first time it is read, then
parsed as the running configuration — the file and the process can never
disagree. Unknown keys are rejected by name. A key you paste into the providers
view is stored as `api_key`; to keep keys out of the file entirely, use
`api_key_env` instead, which names an environment variable read only when that
provider is actually used.

Edit it by hand, or through the app's settings views, which preserve comments
and formatting and validate the whole file before writing.

### Top level

| Key | Default | Meaning |
| --- | --- | --- |
| `default_provider` | `"ollama"` | Which `[providers.*]` entry new sessions use. Must name a configured provider. |

### `[providers.<name>]`

Any number of entries. `kind` selects the shape; the name is yours and is what
the model pickers group models under.

| Key | Applies to | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | both | — | `"ollama"` or `"openai_compat"`. Required. |
| `base_url` | both | `"http://localhost:11434"` for `ollama`, required for `openai_compat` | Endpoint root. Must not be empty. |
| `keep_alive` | `ollama` | unset | How long Ollama keeps the model in RAM, e.g. `"5m"`. `"0"` unloads it immediately. |
| `api_key` | `openai_compat` | unset | The key itself, which is what the providers view writes. Wins over `api_key_env`. |
| `api_key_env` | `openai_compat` | unset | Name of the environment variable holding the key instead. Omit both for keyless endpoints; no auth header is sent then. Must look like an environment variable name. |
| `default_model` | `openai_compat` | unset | Model new sessions start on. Without it, `--model` (or `/model`) is required. |

#### Connecting one

The providers view knows the endpoint and the model list for a handful of
open-weight providers — OpenRouter, Groq, Cerebras, DeepSeek, Together,
Mistral, OpenCode Zen and a local Ollama — so a key is the whole of what it
asks for. Paste one and a shape only one provider issues (`sk-or-`, `gsk_`,
`csk-`) names its own; anything else is one click on a tile. Connecting asks
the endpoint what it serves, starts you on a sensible model, and writes
`[providers.<name>]` under the catalog's own name.

A key the endpoint rejects is the one refusal — nothing is written. An endpoint
that is merely unreachable still connects, since being offline now says nothing
about whether the key is good. `+ custom endpoint` writes the same table by
hand for anything the catalog has never heard of.

Every model menu — the chat picker and spotlight's — lists the free models
first, then the rest, alphabetical within each half, and connecting starts you
on a free model when the endpoint serves one. Free means the endpoint said so,
in the id: OpenRouter suffixes `:free`, OpenCode Zen `-free`. Nothing else is
inferred, and nothing is badged — the id already says it.

### `[brain]`

The brain is a folder of markdown notes — one file per memory, the file stem
as its id, `[[wikilinks]]` as authored graph edges, YAML frontmatter tolerated
but never injected. Nothing is injected unless a message mentions `/brain`;
that turn's question then seeds a walk over the brain graph and the
best-ranked relevant notes are injected up to the cap. A message mentioning
`/memory` hands the model a `save_memory` tool for that turn, so "remember
this" becomes a new note; `/update-memory` hands it `update_memory` instead,
so "this changed" rewrites the note it belongs to; `/delete-memory` hands it
`delete_memory`, which moves the note to `.trash` in the brain folder; and
`/link-memory` hands it `link_memory`, which writes a `[[wikilink]]` into one
note pointing at another; and `/unlink-memory` hands it `unlink_memory`,
which takes that edge back out. `/reminder` is the one mention that reads
nothing: it hands the model `set_reminder`, which writes a row rather than a
note, so the turn never loads the embedder. A recurring ask ("every day at 9",
"every monday 9:30", "every 45 minutes") re-arms itself after each firing;
one missed while the machine slept fires once and re-arms from now, never as
a backlog. Due reminders take over the
spotlight panel until dismissed; `/view-reminders` lists what is waiting and
what has already been shown, and cancels any of the former. `/schedule` hands
the model `schedule_ask`: a prompt odyn runs on that schedule, each run
landing as a normal conversation announced in spotlight — the morning brief.
Scheduled runs are unattended, so they are handed no tools; a prompt carrying
tool mentions is refused, while `/brain` recall works. A turn with a memory tool recalls wider than `/brain` does
— no relevance floor, no `top_k` limit, just the token cap — and is also given
every memory's name, because its job is finding the right note rather than
answering from the best few. One tool per mention — the choice between them is yours, not the model's. A stale `[memory]` section from brain v1 still
parses and is ignored.

One filename is special: `soul.md` in the brain folder holds standing
instructions and is injected on every turn, mention or not — the one exception
to "nothing is injected unless asked", which is why the ledger prices it as
`● soul N` on every surface. It is not a memory: never recalled, indexed,
graphed or listed, and the model cannot write it.

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | data dir | Where the note files live. `~` expands; point it at an Obsidian vault if you like. |
| `model` | `bge-small` | Which model embeds notes and questions — see below. Changing it re-embeds the whole folder. |
| `top_k` | `6` | How many nearest notes seed the recall walk, and the most one `/brain` recall may inject. Must be at least 1. |
| `cap_tokens` | `1200` | Hard cap on injected tokens per recall. Ranked notes are kept until the next one would exceed it. |
| `min_relevance` | `0.3` | Only notes scoring at least this share of the best match are injected. `0` keeps everything the cap allows. Applies to `/brain`; a turn with a memory tool ignores it. |
| `save_temperature` | `0.3` | Sampling temperature for `/memory` save turns; lower is more literal. Between 0 and 2. |
| `soul_cap_tokens` | `400` | Soft budget for `soul.md` — standing instructions injected on every turn. Going over turns its ledger chip red; nothing is truncated. |
| `similarity_edge_threshold` | `0.78` | Cosine similarity at or above which the brain graph draws an edge between two notes. Greater than 0 and at most 1. |

Token counts are a chars/4 approximation, in the config and in every ledger
that reports them.

#### The embedding model

`model` names a backend and a model within it. The brain view's picker writes
this key for you and re-indexes on the spot; the value is what it wrote.

| Form | Backend | Example |
| --- | --- | --- |
| bare name | bundled fastembed — local, offline, no setup | `bge-small`, `nomic-v1.5`, `BGELargeENV15` |
| `ollama:<model>` | the local Ollama daemon, over `/api/embed` | `ollama:nomic-embed-text` |
| `<provider>:<model>` | a configured OpenAI-compatible endpoint | `zen:some-embed-model` |

Bundled models accept a short alias where one reads better (`bge-small`,
`bge-base`, `nomic-v1.5`, `e5-base`, `jina-code`, …) and otherwise any name
fastembed itself knows, so the whole catalog is reachable — the aliases are a
convenience, not the list of what is allowed. Ollama's picker entries are read
live from the daemon and filtered to models it tags `embedding`.

**A `<provider>:` model sends every note's text to that provider.** The other
two backends run on your machine and work offline; this one does not, and
recall stops working without a network. The picker marks it, and so does the
brain view's header while such a model is active.

Changing the model **re-embeds every note**, because vectors from two models
are not comparable — not even at equal width. The vector table is rebuilt at
the new model's dimension, which is read from fastembed's catalog for bundled
models and measured by embedding one short string for the others. Memory rows
survive, so ids, hit counts and recall history are kept; only the vectors are
rebuilt. An index whose model no longer matches the config is rebuilt on the
next recall, whether the key was changed from the UI or the file.

Odyn asks every model for an 8192-token input window and each one clamps that
to its own maximum, so a long-context model actually reads long notes while a
512-token model still stops at 512. Text past a model's window is truncated
before embedding and cannot influence whether that note is recalled.

### `[style]`

| Key | Default | Meaning |
| --- | --- | --- |
| `brevity` | `"off"` | Default answer style for new conversations: `off`, `lite`, `full` or `ultra`. Each level injects a fixed style directive; `off` injects nothing. |

### `[spotlight]`

| Key | Default | Meaning |
| --- | --- | --- |
| `hotkey` | `"Alt+Space"` | Global shortcut, in Tauri accelerator syntax — `Alt` is Option on macOS. A shortcut that cannot be registered is reported as status, never a crash. |
| `brevity` | `"full"` | Answer style for spotlight asks, independent of `[style]`. |
| `provider` | unset | Falls back to `default_provider`. Must name a configured provider. |
| `model` | unset | Falls back to that provider's `default_model`. With neither, spotlight asks fail with `no spotlight model`. |

When a model fails anyway — rate-limited, an unsupported request, a dropped
connection, or a reply with no text in it — the panel says `model unavailable`
and offers `⌘K`, rather than relaying the provider's raw error. The underlying
message goes to the webview console. Configuration mistakes are still named in
full: those are yours to fix, not the model's.

Reasoning is never part of an answer. Models that stream their thinking as a
`<think>…</think>` block inside the text have it removed as the stream flows,
on every surface — spotlight and chat alike.

## Data locations

The config file, the brain folder and the database are independent of each
other and of the checkout.

| What | Where |
| --- | --- |
| Config | `<config dir>/odyn.toml` |
| Brain | `<data dir>/brain/` — the folder of `.md` notes, unless `[brain] path` points elsewhere |
| Database | `<data dir>/odyn.db` (SQLite in WAL mode, plus its `-wal`/`-shm` sidecars) |
| Embedding model | `<data dir>/models/` |

| Platform | Config dir | Data dir |
| --- | --- | --- |
| macOS | `~/Library/Application Support/odyn` | `~/Library/Application Support/odyn` |
| Linux | `$XDG_CONFIG_HOME/odyn`, else `~/.config/odyn` | `$XDG_DATA_HOME/odyn`, else `~/.local/share/odyn` |
| Windows | `%APPDATA%\odyn\config` | `%APPDATA%\odyn\data` |

### Environment variables

| Variable | Effect |
| --- | --- |
| `ODYN_CONFIG` | Full path to the config file, replacing the default location. |
| `ODYN_DB` | Full path to the database file, replacing the default location. |
| `ODYN_SPOTLIGHT_FALLBACK` | `1` forces the spotlight fallback window on any platform. |
| `<api_key_env>` | Whatever variable a provider's `api_key_env` names. Read only when that provider is built, and only when it has no `api_key`. |

## Cross-platform notes

The spotlight window is normally borderless, transparent and always on top,
placed on the monitor holding the cursor. Wayland has no reliable way to summon
such a window, so on Linux with `WAYLAND_DISPLAY` set it falls back to an
ordinary centered window — same behaviour otherwise. `ODYN_SPOTLIGHT_FALLBACK=1`
forces that fallback anywhere, which is how to test it without a Wayland
session.

Verified on this machine (macOS, Apple Silicon): `cargo build --workspace`,
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace`, `bun run build`, and a signed release bundle
via `bun tauri build --target aarch64-apple-darwin`. Everything else in the table
below is untested.

| Platform | Desktop app (`tauri dev`) | Bundle (`tauri build`) | Spotlight |
| --- | --- | --- | --- |
| macOS (Apple Silicon) | untested | **builds** | untested |
| macOS (Intel) | untested | **cannot build** — `ort-sys` publishes no `x86_64-apple-darwin` binary | untested |
| Windows | untested | untested | untested |
| Linux, X11 | untested | untested | untested |
| Linux, Wayland | untested | untested | untested — expected to use the fallback window |

## License

MIT. See `LICENSE`.
