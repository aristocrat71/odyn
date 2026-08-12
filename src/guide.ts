import { el } from "./dom";

// Loaded as its own chunk by view.ts: every string below stays out of the main
// bundle until the guide is opened.

const MOD = navigator.platform.startsWith("Mac") ? "⌘" : "Ctrl";
// `Ctrl+Space` in the config; macOS spells the same chord with a glyph.
const SUMMON = navigator.platform.startsWith("Mac") ? "⌃Space" : "Ctrl+Space";

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
        "Odyn is a personal AI harness: a desktop app over a Rust core, with one " +
          "config file, one database and one brain. It " +
          "talks only to open-weight models — any OpenAI-compatible endpoint and a local " +
          "Ollama. The brain is a folder of markdown notes, and it stays out of your " +
          "context window until you ask: mention `/brain` in a message and Odyn walks " +
          "its memory graph for the notes that answer you, accounted for token by token " +
          "before anything is sent. Every other message reaches the model bare.",
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
      p("Where it lives on this machine:"),
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

[brain]
# path = "~/odyn-brain"
model = "bge-small"
top_k = 6
cap_tokens = 1200
similarity_edge_threshold = 0.78
min_relevance = 0.3
save_temperature = 0.3

[style]
brevity = "off"

[spotlight]
hotkey = "Ctrl+Space"
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
          "any number of `kind = \"openai_compat\"` entries. `base_url` is the endpoint root, `default_model` the model new sessions start on. The name is yours: it is what the model picker groups models under.",
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
bun run tauri dev`),
      p("The app opens on an empty chat. Pick a model in the top right, and type."),
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
        "`connect` asks the endpoint what models it serves, starts you on a sensible one, and writes `[providers.<name>]` into `odyn.toml` under the catalog's own name.",
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
        [
          "find one",
          "the sidebar lists the seven most recently answered and puts the total beside the `CONVERSATIONS` heading; the heading opens all of them with a search on top. The search is fuzzy — letters have to appear in a title in order, not next to each other — and the letters that earned each hit are marked in teal. Arrows move, Enter opens, Esc clears. An older conversation you open takes the sidebar's last slot until something newer pushes it out.",
        ],
      ]),
      p(
        "Under the title, a conversation shows `N turns · X.Xk tokens`. The token count " +
          "appears only when the provider reports usage; an invented number would be worse " +
          "than a missing one.",
      ),
      sub("model picker"),
      p(
        "Two menus in the composer's footer, `provider <name> ▾` and `model <name> ▾`, " +
          "because one list of every provider's whole catalog is too long to find anything " +
          "in. The provider menu lists every configured provider whether it answers or not " +
          "— one that is down is labelled `offline` and its models are dimmed and " +
          "unclickable, because a picker that hides what is down explains nothing. The " +
          "model menu shows that provider's models, free ones first, Ollama entries with " +
          "their on-disk size. Switching provider keeps the model when the new provider " +
          "serves it too. Reachability is re-probed every 30 seconds while a menu is open. " +
          "Choosing a model writes it onto the conversation.",
      ),
      sub("brevity"),
      p(
        "Beside the picker in the same footer, reading `brevity <level> ▾`. Each level injects one " +
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
          "Asks are ephemeral: the exchange is stored only if you promote it. Recalled notes do earn their hits either way, so the brain graph learns from spotlight use.",
      ),
      rows([
        [
          SUMMON,
          "the global hotkey — the default value of `[spotlight] hotkey` (`Ctrl+Space`, Control+Space on every platform), in Tauri accelerator syntax. Pressing it again hides the window.",
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
      sub("a folder of notes"),
      p(
        "The brain is a folder of markdown files — the brain view names the spot, and " +
          "`path` under `[brain]` moves it anywhere, an Obsidian vault included. One " +
          "file is one memory: the file's name is its id, the text is the memory, and " +
          "YAML frontmatter is tolerated but never injected. Write files with any " +
          "editor or agent; Odyn re-reads the folder on every recall and re-embeds " +
          "only what changed. Deleting a file deletes the memory. The database is " +
          "just an index derived from the folder — the files never lie.",
      ),
      sub("links"),
      p(
        "`[[another-note]]` inside a note is a deliberate edge in the brain graph, " +
          "Obsidian-style — `[[note|alias]]` and `[[note#heading]]` resolve to the " +
          "same place, case-insensitively. Links are the strongest edges the recall " +
          "walk follows, so wiring two notes together is telling Odyn they belong " +
          "in context together.",
      ),
      sub("recall — the /brain mention"),
      p(
        "Nothing is injected by default. A message mentioning `/brain` — anywhere in " +
          "it — recalls for that one turn; the token itself is stripped before the " +
          "model or the transcript sees the message. The query is the last two turns " +
          "plus your cleaned message, embedded once. The nearest `top_k` notes seed a " +
          "walk over the brain graph — links strongest, then embedding similarity, " +
          "then co-use — and the final order blends how well a note matches the " +
          "question with how firmly it sits in the walked neighborhood of notes that " +
          "do. Notes scoring under `min_relevance` of the best match are dropped, at " +
          "most `top_k` are kept, and the rest fill in rank order until the next " +
          "would push past `cap_tokens` (default 1200). A bare `/brain` recalls on " +
          "the conversation history alone.",
      ),
      sub("saving, updating, deleting, linking — the memory mentions"),
      p(
        "Mention `/memory` and the model is handed one tool for that turn: " +
          "`save_memory`, which writes a new `.md` note into the brain folder. " +
          "`/update-memory` hands it `update_memory`, which rewrites the matching " +
          "note in place — same slug, same graph edges, new fact. " +
          "`/delete-memory` hands it `delete_memory`, which moves the note into " +
          "`.trash` inside the brain folder — out of recall, but recoverable " +
          "until you empty it. `/link-memory` hands it `link_memory`, which " +
          "writes a `[[wikilink]]` into one note pointing at another — the " +
          "strongest edge the recall walk knows, for connecting two notes that " +
          "were saved apart, and `/unlink-memory` hands it `unlink_memory` to " +
          "take that edge back out — on the `See also` line the name goes, in a " +
          "sentence the brackets are unwrapped so the words survive. " +
          "One tool per mention, on purpose: a small model " +
          "asked to choose between them picks wrong, so the choice stays with " +
          "you. Every such turn also recalls the notes nearest your message — " +
          "that is how the model knows what `[[slug]]` links to write, or which " +
          "note your change belongs to — and wider than `/brain` recalls, since " +
          "the note to rewrite or forget is often not among the best few answers " +
          "to what you said: no `min_relevance` floor, no `top_k` limit, only " +
          "`cap_tokens`. Those turns are also handed every memory's name, so a " +
          "note whose content did not fit can still be named to a tool. " +
          "A `✎ saved`, `✎ updated`, `✕ deleted`, `⌇ linked` or `⌇ unlinked` trace " +
          "names the note " +
          "under the " +
          "reply; each note is a plain file you can edit or delete like any " +
          "other. A call that succeeds ends the turn on odyn's own one-line " +
          "confirmation — the model does not get to talk over your notes after a " +
          "write. It never touches the brain without these mentions, and all of " +
          "them need a model that supports tool calls (llama3.2 does).",
      ),
      sub("reminders"),
      p(
        "Mention `/reminder` and the model is handed `set_reminder` for that " +
          "turn: \"remind me to call mum in 20 minutes\" becomes a row with a " +
          "time on it. It is the one mention that reads nothing from the brain — " +
          "a reminder is not written from memory — so the turn never loads the " +
          "embedder and recalls no notes. The model gives the time either as " +
          "minutes from now or as a date and time; minutes are preferred, since " +
          "they need no reference clock and so cannot land on the wrong day, and " +
          "the current local time is stated in the prompt for when they do not " +
          "fit. A time already past, or one years away, comes back as an error " +
          "the model can read and correct rather than a reminder set wrong.",
      ),
      p(
        "The reminders view — `/view-reminders`, or the sidebar — lists what is " +
          "still waiting, soonest first, with how far off each one is, and the " +
          "last fifty already shown below that. Hovering a waiting row gives you " +
          "a ✕ to cancel it; unlike a note there is no `.trash`, because setting " +
          "one again costs a sentence.",
      ),
      p(
        "When one comes due, spotlight appears with it and takes over the panel " +
          "— no field, no footer, just the reminder and a dismiss button, so " +
          "there is nothing to do but read it. Odyn checks every 20 " +
          "seconds or so while it is running, which is what the tray icon is for: " +
          "closing the window hides it, it does not quit. Nothing fires while odyn " +
          "is actually quit — but nothing is lost either, because anything that " +
          "came due meanwhile is shown the next time it starts. A reminder is " +
          "marked shown only once the panel has taken it, so one you never saw " +
          "survives to the next launch. Reminders live in the database rather " +
          "than the brain folder: they are state with a deadline, not memories, " +
          "and they never enter recall.",
      ),
      sub("the ledger"),
      p(
        "One mono line fused to the top of the composer, which is not a " +
          "readout of what was sent — it is built by the same call the send makes:",
      ),
      pre("CONTEXT   ◈ reading the brain                          109 / 1,200 tk"),
      list([
        "Without a trigger in the draft it reads `/brain recalls memory · /memory saves one` — and that is the truth: the send injects nothing.",
        "With `/brain` it reads `◈ reading the brain` in teal. Which notes were recalled is named by the `◈ used` trace under the answer.",
        "The total on the right is against `cap_tokens`. It refreshes at least 400ms after the last keystroke. The line is exactly what the model sees.",
      ]),
      sub("trace lines"),
      p(
        "Under any answer that recalled: `◈ used cern-trip espresso-order` — the " +
          "note slugs injected for the question above it.",
      ),
      sub("brain view — list mode"),
      p(
        "The count sits beside the title; the line under it states the brain — " +
          "`top-k 6 · min-relevance 0.3 · cap 1200 tk` — with top-k and min-relevance " +
          "editable in place, and the folder's path under the column label. " +
          "One row per note; hover " +
          "reveals `✎` and `✕`, and editing happens in place — the row becomes an " +
          "input, Enter commits, Esc cancels. `+ add a note` writes a new file the " +
          "same way. The semantic search runs the same embedding pipeline as recall, " +
          "so the two agree on order; results replace the " +
          "list rather than filter it, and clearing the field restores browsing. The " +
          "sort word cycles `recent` → `hits` → `created`. A note recalled in the " +
          "last five minutes carries a teal `injected 2m ago` tag. The list pages in " +
          "at 50 as you scroll.",
      ),
      sub("brain view — graph mode"),
      p(
        "What the map draws is what recall traverses. Nodes are teal and grow with " +
          "use: radius 5.5 + hits × 0.45, capped at 14 hits, labelled with their slug " +
          "from about 0.8× zoom. A solid teal edge is an authored `[[link]]`. A faint " +
          "edge means embedding similarity at or above `similarity_edge_threshold` " +
          "(default 0.78). A dashed teal edge means co-use: the two notes were " +
          "recalled together at least twice. Scroll zooms, anchored at the cursor, " +
          "between 0.15× and 6×; drag pans; `+`, `−` and `fit` sit bottom right. " +
          "Hover shows a tooltip, click pins it, double-click centers that node. The " +
          "layout is force-directed, computed in Rust, cached in the database and " +
          "invalidated on every sync or recall. It is deterministic — the same brain " +
          "always draws the same map.",
      ),
      sub("the embedding model"),
      p(
        "`[brain] model` decides what turns notes and questions into vectors. " +
          "The picker in the brain view's header writes it and re-indexes on the spot; " +
          "the same value works written into `odyn.toml` by hand. Three backends:",
      ),
      rows([
        [
          "a bare name",
          "bundled fastembed — local, offline, no setup. Short aliases like `bge-small`, `nomic-v1.5` and `jina-code` where they read better, and otherwise any name fastembed knows: the whole catalog is reachable, the aliases are only a convenience.",
        ],
        [
          "ollama:<model>",
          "the local Ollama daemon over `/api/embed`. Also local and offline, and nothing has to be loaded into Odyn's own process. The picker lists what your daemon has that it tags `embedding`.",
        ],
        [
          "<provider>:<model>",
          "a configured OpenAI-compatible endpoint. **This one sends every note's text to that provider**, and recall stops working offline. The picker marks it, and the brain header keeps saying so while it is active.",
        ],
      ]),
      p(
        "Changing the model re-embeds every note — vectors from two models are not " +
          "comparable, not even at equal width. The vector table is rebuilt at the new " +
          "model's dimension, read from fastembed's catalog for a bundled model and " +
          "measured by embedding one short string for the others. Notes are files, so " +
          "nothing is at risk: rows keep their ids, hit counts and recall history, and " +
          "only the vectors are rebuilt. An index whose model no longer matches the " +
          "config rebuilds itself on the next recall, however the key was changed.",
      ),
      p(
        "Odyn asks every model for an 8192-token window and each clamps that to its own " +
          "maximum, so a long-context model really does read long notes while a " +
          "512-token one still stops at 512. Text past the window is truncated before " +
          "embedding and cannot affect whether that note is recalled.",
      ),
      p(
        "A bundled model is fetched into `<data dir>/models/` the first time an " +
          "embedding is actually needed, with a progress line — bge-small is about " +
          "100MB. It is loaded, used and dropped again, so the weights never sit " +
          "resident. An empty brain never loads an embedder, and a `/brain` mention on " +
          "one never downloads anything.",
      ),
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
        [
          "Tab",
          "take the completion offered under the caret: a `/` command in the front-door or spotlight field, the `/brain` mention in the composer. → does it too, from the end of the line.",
        ],
      ]),
      p(
        "Every app shortcut is a modifier combo, so a focused input can never swallow one. " +
          "In the spotlight window: Enter asks, " +
          `${MOD}+Enter promotes, Esc dismisses.`,
      ),
    ],
  },
  {
    id: "privacy",
    title: "data & privacy",
    body: () => [
      p(
        "Everything Odyn knows is on this machine, in four places, none of them inside the " +
          "checkout:",
      ),
      rows([
        ["config", "`<config dir>/odyn.toml`"],
        ["brain", "`<data dir>/brain/` — the folder of `.md` notes, unless `[brain] path` points elsewhere"],
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
        "Your notes are embedded on this machine by default, whether by the bundled model or by Ollama. The one exception is a `<provider>:` embedding model, which sends every note's full text to that endpoint — the brain view says so whenever one is active, and it is never the default.",
        "A key you paste is written to `odyn.toml` as `api_key`, and nowhere else — never to a log, and never over the wire except to the provider it belongs to. `api_key_env` keeps it out of the file entirely by naming an environment variable instead; either is read only when that provider is actually built.",
        "Memories are the files in the brain folder; `odyn.db` holds conversations, the recall records, and an index derived from those files. Deleting a note's file deletes the memory; deleting a conversation takes its messages with it.",
        "Spotlight asks are stored only when promoted; dismissing drops the exchange. What does persist is the brain's hit ledger: which notes were recalled, never what was asked.",
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
          "a red line at the foot of the sidebar",
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
          "the model never answers from memory",
          "recall is opt-in: mention `/brain` in the message. The ledger above the composer says whether the next send recalls.",
        ],
        [
          "the first embedding hangs",
          "it is the one-time model download, about 100MB into `<data dir>/models/`. Later embeds load from disk.",
        ],
      ]),
    ],
  },
];
