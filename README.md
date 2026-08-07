# Odyn

Odyn is a personal AI harness: a Rust core, a Tauri desktop app, and an `odyn`
CLI that share one brain. It talks only to open-weight models — any
OpenAI-compatible endpoint and local Ollama — and never to closed-weight
providers. Its memory is two-tier: core memories that are always injected, and
episodic memories retrieved by meaning for the turn at hand, both accounted for
token by token before anything is sent.

## Layout

| Path | What it is |
| --- | --- |
| `crates/odyn-core` | All logic: config, providers, SQLite storage, the brain. |
| `crates/odyn-cli` | The `odyn` binary. |
| `crates/odyn-app` | Tauri v2 desktop app. `tauri.conf.json` lives here; there is no `src-tauri/`. |
| `crates/odyn-vec` | Registers the statically linked sqlite-vec extension. |
| `src/`, `index.html`, `spotlight.html` | Vanilla-TypeScript frontend, built by Vite. |

## Prerequisites

- **Rust stable.** `rust-toolchain.toml` pins the `stable` channel; rustup picks
  it up. This is all the CLI needs.
- **[bun](https://bun.sh).** Builds the frontend and provides the Tauri CLI.
  Only needed for the desktop app.
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

The first use of episodic memory downloads the embedding model
(bge-small-en-v1.5, ~100 MB) into the data directory. Nothing else in the build
or the tests touches the network beyond crates.io and npm.

## Build and run

### CLI

```sh
cargo build --release                   # target/release/odyn
cargo install --path crates/odyn-cli    # installs `odyn` into ~/.cargo/bin
odyn chat
```

### Desktop app

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

The app lives in the tray under its own icon, with two items: **Open Odyn**
brings the dashboard back, **Quit** ends the process. Closing the dashboard
window only hides it — the spotlight hotkey has to keep answering — so Quit is
the way out. If the tray cannot be created the close button quits instead,
rather than leaving odyn running with no way to reach it.

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

Edit it by hand, or through `odyn config get` / `odyn config set`, which
preserve comments and formatting and validate the whole file before writing.

### Top level

| Key | Default | Meaning |
| --- | --- | --- |
| `default_provider` | `"ollama"` | Which `[providers.*]` entry new sessions use. Must name a configured provider. |

### `[providers.<name>]`

Any number of entries. `kind` selects the shape; the name is yours and is what
`--provider` and `/model` refer to.

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
`[providers.<name>]` under the catalog's own name, so `--provider openrouter`
works from the CLI straight away.

A key the endpoint rejects is the one refusal — nothing is written. An endpoint
that is merely unreachable still connects, since being offline now says nothing
about whether the key is good. `+ custom endpoint` writes the same table by
hand for anything the catalog has never heard of.

Every model menu — the chat picker and spotlight's — lists the free models
first, then the rest, alphabetical within each half, and connecting starts you
on a free model when the endpoint serves one. Free means the endpoint said so,
in the id: OpenRouter suffixes `:free`, OpenCode Zen `-free`. Nothing else is
inferred, and nothing is badged — the id already says it.

### `[memory]`

| Key | Default | Meaning |
| --- | --- | --- |
| `core_budget_tokens` | `500` | Token budget for core memories. Core is never truncated: over budget it is still injected whole and the overrun is reported. |
| `episodic_top_k` | `6` | How many nearest episodic memories retrieval considers per turn. Must be at least 1. |
| `episodic_cap_tokens` | `900` | Hard cap on injected episodic tokens. Retrieved memories are kept closest-first until the next one would exceed it. |
| `similarity_edge_threshold` | `0.78` | Cosine similarity at or above which the brain graph draws an edge between two memories. Greater than 0 and at most 1. |

Token counts are a chars/4 approximation, in the config and in every ledger
that reports them.

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
on every surface — spotlight, chat, and `odyn ask`.

## CLI reference

```
odyn [--provider NAME] [--model NAME] <COMMAND>
```

`--provider` and `--model` are global and may appear before or after the
subcommand. `--provider` defaults to `default_provider`; `--model` defaults to
that provider's `default_model`. `-h`/`--help` and `-V`/`--version` are
available everywhere.

Exit codes: `0` success, `1` provider or runtime failure, `2` anything to fix in
`odyn.toml` or in the flags.

### `odyn ask [PROMPT]`

One question, streamed to stdout, then exit. With no `PROMPT`, the prompt is
read from stdin.

| Flag | Effect |
| --- | --- |
| `--json` | Emit NDJSON instead of plain text: `{"type":"delta","text":…}` per chunk, then `{"type":"done","usage":…}`. Errors arrive as `{"type":"error","message":…}` on stdout, so consumers never have to read stderr. |
| `--save` | Keep the exchange as a conversation. Creates the database if it does not exist; without it, `ask` opens an existing database read-only and stays ephemeral. |
| `--show-context` | Print the injected memory context before the answer: the system message verbatim, then per-item token counts and the core/episodic totals. Goes to stderr in text mode so a piped answer stays clean; in `--json` mode it is one more event, `{"type":"context",…}`. |
| `--brevity LEVEL` | `off`, `lite`, `full` or `ultra`. Overrides `[style] brevity` for this invocation. |

### `odyn chat`

The REPL, over the same streaming path as `ask`. Nothing inside the loop is
fatal: a failure is reported and the prompt comes back. Ctrl-C clears the line,
Ctrl-D leaves.

| Flag | Effect |
| --- | --- |
| `--show-context` | Print the injected context before each answer. |
| `--brevity LEVEL` | Starting style for this session. |

| Command | Effect |
| --- | --- |
| `/model` | Print the current `provider / model`. |
| `/model <MODEL>` | Switch model on the current provider. |
| `/model <PROVIDER>/<MODEL>` | Switch both, when the left side names a configured provider. |
| `/new` | Start a new conversation; the old one is saved first. |
| `/brevity` | Print the current level. |
| `/brevity <LEVEL>` | Switch to `off`, `lite`, `full` or `ultra` for the rest of the session. |
| `/quit` | Leave. |

### `odyn config`

| Command | Effect |
| --- | --- |
| `odyn config path` | Print the path of `odyn.toml`. |
| `odyn config get <KEY>` | Print the value at a dotted key, e.g. `providers.ollama.base_url`. |
| `odyn config set <KEY> <VALUE>` | Set a dotted key. Numbers and booleans are stored as such, anything else as a string. Comments and layout survive; an invalid result leaves the file untouched. |

### `odyn mem`

| Command | Effect |
| --- | --- |
| `odyn mem add <CONTENT>` | Remember something as an episodic memory: content is normalized and embedded. |
| `odyn mem add <CONTENT> --core` | Store as a core memory instead — always injected, never embedded. |
| `odyn mem list` | List all memories, oldest first, as `<id>  <tokens> tk  <content>`. |
| `odyn mem list --tier <core\|episodic>` | Restrict to one tier. |
| `odyn mem search <QUERY>` | The episodic memories closest in meaning to the query, up to 20 — browsing is deliberately wider than injection. |
| `odyn mem rm <ID>` | Delete by id. |
| `odyn mem edit <ID> <CONTENT>` | Replace the content. Episodic memories are re-embedded. |

Ids print as `c-01` (core) and `e-0142` (episodic); the prefix is cosmetic, so
`e-0142`, `c-0142` and `142` all name the same memory.

## Data locations

The config file and the database are independent of each other and of the
checkout.

| What | Where |
| --- | --- |
| Config | `<config dir>/odyn.toml` |
| Database | `<data dir>/odyn.db` (SQLite in WAL mode, plus its `-wal`/`-shm` sidecars) |
| Embedding model | `<data dir>/models/` |

| Platform | Config dir | Data dir |
| --- | --- | --- |
| macOS | `~/Library/Application Support/odyn` | `~/Library/Application Support/odyn` |
| Linux | `$XDG_CONFIG_HOME/odyn`, else `~/.config/odyn` | `$XDG_DATA_HOME/odyn`, else `~/.local/share/odyn` |
| Windows | `%APPDATA%\odyn\config` | `%APPDATA%\odyn\data` |

`odyn config path` prints the resolved config path on the machine you are on.

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
warnings`, `cargo test --workspace` and `bun run build`. Everything else in the
table below is untested.

| Platform | CLI | Desktop app (`tauri dev`) | Bundle (`tauri build`) | Spotlight |
| --- | --- | --- | --- | --- |
| macOS (Apple Silicon) | builds | untested | untested | untested |
| macOS (Intel) | untested | untested | untested | untested |
| Windows | untested | untested | untested | untested |
| Linux, X11 | untested | untested | untested | untested |
| Linux, Wayland | untested | untested | untested | untested — expected to use the fallback window |

## License

MIT. See `LICENSE`.
