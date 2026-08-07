import { el } from "./dom";

// Loaded as its own chunk by view.ts: every string below stays out of the main
// bundle until the guide is opened.

const MOD = navigator.platform.startsWith("Mac") ? "⌘" : "Ctrl";
// `Alt+Space` in the config is one chord with two names: Option on macOS.
const SUMMON = navigator.platform.startsWith("Mac") ? "⌥Space" : "Alt+Space";

type Section = { id: string; title: string; body: () => HTMLElement[] };

export function renderGuide(): HTMLElement {
  const doc = el("div", "guide-doc");
  doc.append(contents());
  for (const section of SECTIONS) {
    const block = el("section", "guide-section");
    block.id = section.id;
    block.append(el("h2", "guide-h", section.title), ...section.body());
    doc.append(block);
  }
  return doc;
}

function contents(): HTMLElement {
  const nav = el("nav", "guide-toc");
  for (const section of SECTIONS) {
    const link = el("a", undefined, section.title);
    link.href = `#${section.id}`;
    nav.append(link);
  }
  return nav;
}

// Inline `code` spans, the only markup the guide needs. Chat messages go
// through markdown.ts; this is one rule, not a renderer.
function inline(text: string): (string | HTMLElement)[] {
  return text
    .split(/`([^`]+)`/)
    .map((part, at) => (at % 2 === 0 ? part : el("code", "guide-code", part)));
}

function p(text: string): HTMLElement {
  const line = el("p", "guide-p");
  line.append(...inline(text));
  return line;
}

function list(items: string[]): HTMLElement {
  const box = el("ul", "guide-list");
  for (const item of items) {
    const line = el("li");
    line.append(...inline(item));
    box.append(line);
  }
  return box;
}

/// Two columns of plain text — a term and what it does. No table chrome.
function rows(pairs: [string, string][]): HTMLElement {
  const box = el("div", "guide-rows");
  for (const [term, meaning] of pairs) {
    const desc = el("div", "guide-desc");
    desc.append(...inline(meaning));
    box.append(el("div", "guide-term", term), desc);
  }
  return box;
}

const pre = (text: string): HTMLElement => el("pre", "guide-pre", text);

const sub = (text: string): HTMLElement => el("h3", "guide-sub", text);

const SECTIONS: Section[] = [
  {
    id: "what",
    title: "what odyn is",
    body: () => [
      p(
        "Odyn is a personal AI harness: a desktop app and an `odyn` command-line binary " +
          "over one Rust core, sharing one config file, one database and one brain. It " +
          "talks only to open-weight models — any OpenAI-compatible endpoint and a local " +
          "Ollama. Memory is two-tier: core memories that are always injected, and " +
          "episodic memories retrieved by meaning for the turn at hand, both accounted " +
          "for token by token before anything is sent.",
      ),
    ],
  },
  {
    id: "start",
    title: "getting started",
    body: () => [
      p(
        "The config file is written from a built-in template the first time it is read, " +
          "then parsed as the running configuration — the file on disk and the running " +
          "process can never disagree. Unknown keys are rejected by name.",
      ),
      p("`odyn config path` prints where it lives on this machine:"),
      rows([
        ["macOS", "`~/Library/Application Support/odyn/odyn.toml`"],
        ["Linux", "`$XDG_CONFIG_HOME/odyn/odyn.toml`, else `~/.config/odyn/odyn.toml`"],
        ["Windows", "`%APPDATA%\\odyn\\config\\odyn.toml`"],
        ["override", "`ODYN_CONFIG` — a full path to the file, replacing the default location"],
      ]),
      p("The template, with its comment block trimmed:"),
      pre(
        `default_provider = "ollama"

[providers.ollama]
kind = "ollama"
base_url = "http://localhost:11434"
keep_alive = "5m"

# [providers.deepseek]
# kind = "openai_compat"
# base_url = "https://api.deepseek.com"
# api_key_env = "DEEPSEEK_API_KEY"
# default_model = "deepseek-chat"

[memory]
core_budget_tokens = 500
episodic_top_k = 6
episodic_cap_tokens = 900
similarity_edge_threshold = 0.78

[style]
brevity = "off"

[spotlight]
hotkey = "Alt+Space"
brevity = "full"`,
      ),
      rows([
        [
          "default_provider",
          "which `[providers.*]` entry new conversations and sessions use. Must name a configured provider.",
        ],
        [
          "[providers.ollama]",
          "`kind = \"ollama\"`. `base_url` defaults to `http://localhost:11434`. `keep_alive` is how long Ollama keeps a model in RAM; `\"0\"` unloads it immediately.",
        ],
        [
          "[providers.<name>]",
          "any number of `kind = \"openai_compat\"` entries. `base_url` is the endpoint root, `default_model` the model new sessions start on. The name is yours: it is what `--provider` and `/model` refer to.",
        ],
        [
          "api_key",
          "the key itself, which is what the providers view writes when you paste one. It wins over `api_key_env`.",
        ],
        [
          "api_key_env",
          "names an environment variable holding the key instead, for a file that must never contain one. Omit both for keyless endpoints; no auth header is sent then. Either is read only when that provider is actually built, so an unset key stops only that provider.",
        ],
      ]),
      p("First run, from the repository root:"),
      pre(`bun install
bun run tauri dev                       # the desktop app

cargo install --path crates/odyn-cli    # installs odyn into ~/.cargo/bin
odyn chat`),
      p(
        "The app opens on an empty chat. Pick a model in the top right, and type. The CLI " +
          "reads the same config and the same database.",
      ),
    ],
  },
  {
    id: "providers",
    title: "providers",
    body: () => [
      p(
        "The providers view lists what is configured and connects what is not. Odyn " +
          "already knows the endpoint and the model list for the open-weight providers " +
          "in its catalog — OpenRouter, Groq, Cerebras, DeepSeek, Together, Mistral, " +
          "OpenCode Zen and a local Ollama — so a key is the whole of what you have to " +
          "give it.",
      ),
      sub("connecting"),
      list([
        "Paste a key. A key whose shape belongs to one provider and no other — `sk-or-` is OpenRouter's, `gsk_` is Groq's — names its own; anything else is one click on a tile.",
        "`connect` asks the endpoint what models it serves, starts you on a sensible one, and writes `[providers.<name>]` into `odyn.toml`. The name is the catalog's, so `--provider openrouter` works from the CLI immediately.",
        "A key the endpoint rejects is the one refusal: nothing is written, and the key stays in the field to be fixed. An endpoint that is merely unreachable still connects — being offline now says nothing about whether the key is good.",
        "`use by default` points `default_provider` at it. It starts ticked when nothing but a local Ollama is configured, since the first key is usually the one meant to answer.",
        "Connecting an endpoint that is already there replaces its key, which is how a rotated one gets in.",
      ]),
      sub("everything else"),
      p(
        "`+ custom endpoint` is the long way round: name, kind, base url, key or key " +
          "env, and default model, for any OpenAI-compatible endpoint the catalog has " +
          "never heard of. It writes the same table the catalog does, and so does a " +
          "text editor — nothing about the file depends on how it got there.",
      ),
    ],
  },
  {
    id: "chat",
    title: "chat",
    body: () => [
      rows([
        ["Enter", "send. `Shift+Enter` puts in a newline; the composer grows from one row to six."],
        [
          "streaming",
          "text renders as it arrives, with a teal block cursor at the end until the stream closes.",
        ],
        [
          "Esc",
          "cancels a running stream. The partial answer is kept, stored, and marked `(interrupted)`.",
        ],
        [
          "retry",
          "a failed stream leaves its partial text on screen with `stream failed: … · retry` under it. Retry re-runs the turn against the stored question, so nothing is asked twice.",
        ],
        [
          "rename",
          "double-click a conversation in the sidebar. Enter commits, Esc cancels; an empty or unchanged title is a cancel.",
        ],
        ["delete", "the `✕` on a conversation row, revealed on hover or focus."],
      ]),
      p(
        "Under the title, a conversation shows `N turns · X.Xk tokens`. The token count " +
          "appears only when the provider reports usage; an invented number would be worse " +
          "than a missing one.",
      ),
      sub("model picker"),
      p(
        "Top right, reading `provider / model ▾`. It lists every " +
          "configured provider whether it answers or not — one that is down is labelled " +
          "`· offline` and its models are dimmed and unclickable, because a picker that " +
          "hides what is down explains nothing. Ollama entries carry their on-disk size. " +
          "Reachability is re-probed every 30 seconds while the menu is open. Choosing a " +
          "model writes it onto the conversation.",
      ),
      sub("brevity"),
      p(
        "Beside the picker, reading `brevity <level> ▾`. Each level injects one " +
          "fixed directive under a `## Style` heading in the same system message as memory; " +
          "every level tells the model to reproduce code blocks, shell commands, file paths, " +
          "identifiers and error messages byte-exact.",
      ),
      rows([
        ["off", "natural prose — nothing is injected at all, not even the heading."],
        ["lite", "trim the filler: no preamble, hedging or restating. Full sentences stay."],
        ["full", "tight fragments over sentences, substance only, no summary closings."],
        ["ultra", "minimum viable words. Telegraphic. One line where one line works."],
      ]),
      p(
        "A conversation with no choice of its own follows `[style] brevity` from the config. " +
          "Picking a level stores it on that conversation, which then overrides the default " +
          "from the next send on — never retroactively.",
      ),
    ],
  },
  {
    id: "spotlight",
    title: "spotlight",
    body: () => [
      p(
        "One field, summoned anywhere, for a question that does not deserve a conversation. " +
          "Asks are ephemeral: nothing is stored unless you promote the exchange.",
      ),
      rows([
        [
          SUMMON,
          "the global hotkey — the default value of `[spotlight] hotkey` (`Alt+Space`, which is Option+Space on macOS), in Tauri accelerator syntax. Pressing it again hides the window.",
        ],
        [`${MOD}K`, "the same toggle, from the main window."],
        [
          "Enter",
          "ask. The answer streams below the field, with a one-line ledger between the two and a `◈ used …` trace under a finished answer that used memories.",
        ],
        ["Esc", "dismiss. The exchange is dropped with the window."],
        [
          `${MOD}+Enter`,
          "promote: the exchange becomes a saved conversation and the main window opens on it. Mid-stream, the partial answer is kept and marked `(interrupted)`.",
        ],
      ]),
      p("Spotlight answers with its own target, never the last conversation's:"),
      rows([
        ["[spotlight] provider", "falls back to `default_provider` when unset."],
        [
          "[spotlight] model",
          "falls back to that provider's `default_model`. With neither, an ask fails with `no spotlight model`.",
        ],
        [
          "[spotlight] brevity",
          "defaults to `full` and is independent of `[style] brevity`, because a spotlight answer should be terse.",
        ],
      ]),
      p(
        "The window is normally frameless, transparent and always on top, placed on the " +
          "monitor holding the cursor with its field at 38% of the screen height. Wayland " +
          "has no reliable way to summon such a window, so on Linux with `WAYLAND_DISPLAY` " +
          "set it falls back to an ordinary centered window — same behaviour otherwise. " +
          "`ODYN_SPOTLIGHT_FALLBACK=1` forces that fallback on any platform, which is how " +
          "to see it without a Wayland session.",
      ),
    ],
  },
  {
    id: "brain",
    title: "the brain",
    body: () => [
      sub("core"),
      p(
        "Core memories are facts about you that are worth paying for on every single " +
          "turn: they are injected whole, every time, and are never embedded. Core is never " +
          "truncated — over its budget it still goes in complete and the overrun is flagged, " +
          "because silently dropping one would make the ledger a lie.",
      ),
      sub("episodic"),
      p(
        "Episodic memories are everything else, retrieved only when they are relevant. " +
          "The retrieval query is the last two turns of the conversation — four messages, " +
          "injected system messages excluded — joined with the message you are about to " +
          "send. That text is embedded, the nearest `episodic_top_k` (default 6) episodic " +
          "memories come back closest-first, and they are kept in that order until the next " +
          "one would push past `episodic_cap_tokens` (default 900). Token counts everywhere " +
          "are a chars/4 approximation, computed when a memory is written.",
      ),
      sub("the ledger"),
      p(
        "One mono line fused to the top of the composer, which is not a " +
          "readout of what was sent — it is built by the same call the send makes:",
      ),
      pre("CONTEXT   ● core 342   ◈ e-0142 61   ◈ e-0087 48        488 / 1,400 tk"),
      list([
        "`● core {tk}` in amber, one teal `◈ {id} {tk}` per retrieved episodic memory, token counts dim.",
        "Past five episodic chips the tail collapses to `◈ +3 more 122`; clicking expands it.",
        "Hovering a chip shows that memory's full text.",
        "The total on the right is against `core_budget_tokens + episodic_cap_tokens` — 1,400 with the defaults. The core chip turns red when core alone is over its budget.",
        "It refreshes at least 400ms after the last keystroke. The line is exactly what the model sees.",
      ]),
      sub("trace lines"),
      p(
        "Under any answer that used episodic memories: `◈ used e-0142 " +
          "e-0087`. Ids print as `c-01` for core and `e-0142` for episodic; the prefix is " +
          "cosmetic, so `e-0142`, `c-0142` and `142` all name the same memory.",
      ),
      sub("from the command line"),
      rows([
        ["odyn mem add <CONTENT>", "remember something as episodic: normalized to one line, then embedded."],
        ["odyn mem add <CONTENT> --core", "store as core instead — always injected, never embedded."],
        ["odyn mem list", "every memory, oldest first, as `<id>  <tokens> tk  <content>`."],
        ["odyn mem list --tier core|episodic", "one tier only."],
        [
          "odyn mem search <QUERY>",
          "the episodic memories closest in meaning, up to 20 — browsing is deliberately wider than injection.",
        ],
        ["odyn mem rm <ID>", "delete by id."],
        ["odyn mem edit <ID> <CONTENT>", "replace the content. Episodic memories are re-embedded."],
      ]),
      sub("brain view — list mode"),
      p(
        "The header states the brain: `214 episodic · bge-small " +
          "· top-k 6 · cap 900 tk`. The core column carries an inline budget bar, " +
          "`342 ——▓▓▓——— 500 tk`, and one plain row per memory; hover reveals `✎` and `✕`, " +
          "and editing happens in place — the row becomes an input, Enter commits, Esc " +
          "cancels. `+ add core memory` adds one the same way. The episodic column has a " +
          "semantic search that runs the same embedding pipeline as chat retrieval and " +
          "`odyn mem search`, so all three agree on order; results replace the list rather " +
          "than filter it, and clearing the field restores browsing. The sort word cycles " +
          "`recent` → `hits` → `created`. A memory injected in the last five minutes carries " +
          "a teal `injected 2m ago` tag. The list pages in at 50 as you scroll.",
      ),
      sub("brain view — graph mode"),
      p(
        "Core nodes are amber, radius 11, always labelled. " +
          "Episodic nodes are teal and grow with use: radius 5.5 + hits × 0.45, capped at 14 " +
          "hits, labelled with their id from about 0.8× zoom. A solid faint edge means " +
          "embedding similarity at or above `similarity_edge_threshold` (default 0.78). A " +
          "dashed teal edge means co-injection: the two memories were retrieved together at " +
          "least twice. Scroll zooms, anchored at the cursor, between 0.15× and 6×; drag " +
          "pans; `+`, `−` and `fit` sit bottom right. Hover shows a tooltip, click pins it, " +
          "double-click centers that node. The layout is force-directed, computed in Rust, " +
          "cached in the database and invalidated on every memory or injection write. It is " +
          "deterministic — the same brain always draws the same map.",
      ),
      sub("the model download"),
      p(
        "Embedding uses bge-small-en-v1.5, 384 dimensions, about " +
          "100MB, fetched into `<data dir>/models/` the first time an embedding is actually " +
          "needed, with a progress line. It is loaded, used and dropped again, so the " +
          "weights never sit resident. Retrieval on a brain with no episodic memories skips " +
          "the embedder entirely: an empty brain never downloads anything.",
      ),
    ],
  },
  {
    id: "context",
    title: "context inspection",
    body: () => [
      p(
        "`odyn ask --show-context` and `odyn chat --show-context` print the context before " +
          "the answer. In text mode it goes to stderr, so a piped answer stays clean:",
      ),
      pre(`----- context -----
## Core profile
- [c-01] name is Mitul
- [c-02] prefers terse replies

## Relevant memories
- [e-0142] went to CERN in june

## Style
Prefer tight fragments over full sentences. …
----- tokens (chars/4 approximation) -----
c-01 4
c-02 6
e-0142 8
core 10/500 tk, episodic 8/900 tk
-------------------`),
      list([
        "Everything between the two rules is the system message verbatim — the exact bytes the model receives, not a summary of them.",
        "`## Core profile` and `## Relevant memories` list one `- [id] content` line per memory, in injection order. An empty section is omitted entirely.",
        "`## Style` is last and appears only when brevity is not `off`.",
        "Then one `<id> <tokens>` line per injected memory, and the two totals against `core_budget_tokens` and `episodic_cap_tokens`.",
        "With nothing to inject the whole block is one line: `----- context: empty -----`.",
        "Token counts are a chars/4 approximation, not the provider's tokenizer. They are consistent, not exact.",
        "Under `--json`, this becomes one more event on the stream instead: `{\"type\":\"context\",\"system\":…,\"items\":[…]}`.",
      ]),
    ],
  },
  {
    id: "keys",
    title: "keyboard",
    body: () => [
      rows([
        [`${MOD}K`, "spotlight, from anywhere in the app."],
        [`${MOD}N`, "new conversation."],
        [`${MOD}1`, "chat view."],
        [`${MOD}2`, "brain view."],
        ["Enter", "send the composer; commit a rename or an inline memory edit."],
        ["Shift+Enter", "newline in the composer."],
        [
          "Esc",
          "cancel a running stream; close the model or brevity menu; cancel a rename or memory edit; close the sidebar overlay on a narrow window.",
        ],
        ["↑ ↓", "walk the model picker once it is open."],
      ]),
      p(
        "Every app shortcut is a modifier combo, so a focused input can never swallow one. " +
          "In the spotlight window: Enter asks, " +
          `${MOD}+Enter promotes, Esc dismisses.`,
      ),
    ],
  },
  {
    id: "cli",
    title: "cli reference",
    body: () => [
      pre("odyn [--provider NAME] [--model NAME] <COMMAND>"),
      p(
        "`--provider` and `--model` are global and may appear before or after the " +
          "subcommand. `--provider` defaults to `default_provider`; `--model` defaults to " +
          "that provider's `default_model`. Exit codes: `0` success, `1` a provider or " +
          "runtime failure, `2` anything to fix in `odyn.toml` or in the flags.",
      ),
      rows([
        [
          "odyn ask [PROMPT]",
          "one question, streamed to stdout, then exit. With no `PROMPT` the prompt is read from stdin.",
        ],
        [
          "  --json",
          "NDJSON instead of plain text: `{\"type\":\"delta\",…}` per chunk, then `{\"type\":\"done\",\"usage\":…}`. Errors arrive as `{\"type\":\"error\",…}` on stdout, so consumers never read stderr.",
        ],
        [
          "  --save",
          "keep the exchange as a conversation, creating the database if needed. Without it, `ask` opens an existing database and otherwise stays ephemeral — asking never conjures a database.",
        ],
        ["  --show-context", "print the injected context before the answer."],
        ["  --brevity LEVEL", "`off`, `lite`, `full` or `ultra`, overriding `[style] brevity` for this run."],
        [
          "odyn chat",
          "the REPL, over the same streaming path. No provider or database failure inside the loop is fatal: it is reported and the prompt comes back. Ctrl-C clears the line, Ctrl-D leaves.",
        ],
        ["  --show-context", "print the injected context before each answer."],
        ["  --brevity LEVEL", "the starting level for this session."],
        ["  /model", "print the current `provider / model`."],
        ["  /model <MODEL>", "switch model on the current provider."],
        [
          "  /model <PROVIDER>/<MODEL>",
          "switch both — only when the left side names a configured provider, since model names carry slashes of their own.",
        ],
        ["  /new", "start a new conversation; the one being left is saved first."],
        ["  /brevity", "print the current level."],
        ["  /brevity <LEVEL>", "switch level for the rest of the session."],
        ["  /quit", "leave."],
        ["odyn config path", "print the path of `odyn.toml`."],
        ["odyn config get <KEY>", "print the value at a dotted key, e.g. `providers.ollama.base_url`."],
        [
          "odyn config set <KEY> <VALUE>",
          "set a dotted key. Numbers and booleans are stored as such, anything else as a string; comments and layout survive, and an invalid result leaves the file untouched.",
        ],
        ["odyn mem …", "`add`, `list`, `search`, `rm`, `edit` — see the brain section above."],
      ]),
    ],
  },
  {
    id: "privacy",
    title: "data & privacy",
    body: () => [
      p(
        "Everything Odyn knows is on this machine, in three places, none of them inside the " +
          "checkout:",
      ),
      rows([
        ["config", "`<config dir>/odyn.toml`"],
        ["database", "`<data dir>/odyn.db` — SQLite in WAL mode, plus its `-wal` and `-shm` sidecars"],
        ["embedding model", "`<data dir>/models/`"],
      ]),
      p(
        "On macOS both directories are `~/Library/Application Support/odyn`; on Linux they " +
          "are `$XDG_CONFIG_HOME/odyn` and `$XDG_DATA_HOME/odyn`; on Windows " +
          "`%APPDATA%\\odyn\\config` and `%APPDATA%\\odyn\\data`.",
      ),
      list([
        "Outbound traffic goes to the providers you configured, and nowhere else — plus one download of the embedding model, once, the first time an embedding is needed.",
        "A key you paste is written to `odyn.toml` as `api_key`, and nowhere else — never to a log, and never over the wire except to the provider it belongs to. `api_key_env` keeps it out of the file entirely by naming an environment variable instead; either is read only when that provider is actually built.",
        "Conversations, memories and injection records live only in `odyn.db`. Deleting a conversation takes its messages with it.",
        "Spotlight asks are never stored unless you promote them; dismissing drops the exchange.",
        "`odyn ask` without `--save` will not create a database on a machine that never saved anything.",
      ]),
      p("Both locations can be moved:"),
      rows([
        ["ODYN_CONFIG", "full path to the config file, replacing the default location."],
        ["ODYN_DB", "full path to the database file, replacing the default location."],
        ["ODYN_SPOTLIGHT_FALLBACK", "`1` forces the spotlight fallback window on any platform."],
      ]),
    ],
  },
  {
    id: "trouble",
    title: "troubleshooting",
    body: () => [
      rows([
        [
          "a red dot in the footer",
          "the named provider did not answer a probe. A second dot appears only when a local Ollama is configured beside a different default provider. Both are re-probed every 30 seconds; the number beside them is this process's resident memory.",
        ],
        [
          "a red line under the footer",
          "the global hotkey could not be registered — usually another app owns the combination. Change `[spotlight] hotkey`. Spotlight still opens with " +
            `${MOD}K from the main window.`,
        ],
        [
          "empty model picker",
          "every group lists what its endpoint names: Ollama what it has installed, an OpenAI-compatible entry what its `/models` answers. A group marked `· offline` reached nothing — start Ollama, or check the `base_url`. An endpoint that answers but serves no listing shows only its `default_model`.",
        ],
        [
          "stream failed: …",
          "click `retry` under the partial answer. The question is already stored, so retrying does not ask it again.",
        ],
        [
          "no model set · pick one",
          "the conversation has no model yet — Ollama has no `default_model` to inherit. Picking one in the picker clears the line.",
        ],
        [
          "an error line above the view",
          "a config or database failure, reported in place rather than in a dialog. Memory is additive: when the brain cannot run, the turn still goes out uninjected, and nothing is recorded for it.",
        ],
        [
          "no spotlight model",
          "set `[spotlight] model`, or give the spotlight provider a `default_model`.",
        ],
        [
          "a red core chip",
          "core memories exceed `core_budget_tokens`. They are still injected whole — trim them in the brain view, or raise the budget.",
        ],
        [
          "the first embedding hangs",
          "it is the one-time model download, about 100MB into `<data dir>/models/`. Later embeds load from disk.",
        ],
      ]),
    ],
  },
];
