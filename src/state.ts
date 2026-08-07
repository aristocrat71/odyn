import * as api from "./api";

export type View = "home" | "chat" | "brain" | "providers" | "config" | "guide";

export type Stream = {
  conversation: number;
  requestId: number | null;
  prompt: string;
  text: string;
  error: string;
  // The episodic ids the backend injected for this reply, for the trace line.
  used: string[];
};

// The backend's refusal when a conversation has no model. Picking one answers
// it, so the line goes away with the selection.
const NO_MODEL = "no model set · pick one";
const PICKER_REFRESH_MS = 30_000;

export const state = {
  // The front door: commands from here, not straight into a conversation.
  view: "home" as View,
  conversations: [] as api.Conversation[],
  selected: null as number | null,
  messages: [] as api.Message[],
  turns: 0,
  tokens: null as number | null,
  stream: null as Stream | null,
  picker: {
    open: false,
    loading: false,
    groups: [] as api.ProviderGroup[],
  },
  editing: null as number | null,
  draft: "",
  brevityMenu: false,
  status: null as api.Status | null,
  hotkeyError: null as string | null,
  ledger: {
    preview: null as api.ContextPreview | null,
    error: null as string | null,
    expanded: false,
  },
  brain: {
    mode: "list" as "list" | "graph",
    overview: null as api.BrainOverview | null,
    episodic: [] as api.MemoryRow[],
    sort: "recent" as api.EpisodicSort,
    exhausted: false,
    query: "",
    // `null` means browsing; a search replaces the list, not filters it.
    results: null as api.MemoryRow[] | null,
    editing: null as number | "new" | null,
    graph: null as api.Graph | null,
  },
  config: {
    file: null as api.ConfigFile | null,
  },
  providers: {
    entries: null as api.ProviderEntry[] | null,
    // `{ name: null }` is the add form; a string names the row being edited.
    editing: null as { name: string | null } | null,
    catalog: null as api.CatalogItem[] | null,
    // The catalog entry the connect panel is aimed at. A recognised key aims
    // it on its own; a tile is how the rest get aimed.
    pick: null as string | null,
    connecting: false,
    // What the last connection came to, shown until the next one starts.
    connected: null as string | null,
  },
  error: "",
};

let render = (): void => {};
let renderStream = (): void => {};

export function onChange(fn: () => void): void {
  render = fn;
}

// Deltas redraw one message, not the app, so they get their own hook.
export function onStream(fn: () => void): void {
  renderStream = fn;
}

export const load = (): Promise<void> =>
  guard(async () => {
    state.conversations = await api.listConversations();
    const first = state.conversations[0];
    if (state.selected === null && first !== undefined) await open(first.id);
  });

export const refreshStatus = (): Promise<void> =>
  guard(async () => {
    state.status = await api.status();
    state.hotkeyError = await api.spotlightStatus();
  });

export const selectConversation = (id: number): Promise<void> =>
  guard(async () => {
    await open(id);
    state.view = "chat";
  });

export const newConversation = (): Promise<void> =>
  guard(async () => {
    const created = await api.createConversation();
    state.conversations.unshift(created);
    await open(created.id);
    state.view = "chat";
  });

export const deleteConversation = (id: number): Promise<void> =>
  guard(async () => {
    await api.deleteConversation(id);
    state.conversations = state.conversations.filter((row) => row.id !== id);
    if (state.editing === id) state.editing = null;
    if (state.selected === id) {
      state.selected = null;
      state.messages = [];
      state.turns = 0;
      state.tokens = null;
    }
  });

export function setView(view: View): void {
  state.view = view;
  shut();
  if (view === "brain") void loadBrain();
  // Re-read on every open, so an edit made outside the app is on screen.
  if (view === "config") void loadConfig();
  if (view === "providers") void loadProvidersConfig();
  render();
}

export function togglePicker(): void {
  if (state.picker.open) {
    closePicker();
    return;
  }
  state.picker.open = true;
  // Reachability from a minute ago is not a fact, so nothing stale is shown.
  state.picker.loading = true;
  state.picker.groups = [];
  void loadProviders();
  timer = window.setInterval(() => void loadProviders(), PICKER_REFRESH_MS);
  render();
}

export function closePicker(): void {
  if (!state.picker.open) return;
  shut();
  render();
}

export const chooseModel = (provider: string, model: string): Promise<void> =>
  guard(async () => {
    const id = state.selected;
    if (id === null) return;
    await api.setConversationModel(id, provider, model);
    const row = state.conversations.find((candidate) => candidate.id === id);
    if (row !== undefined) {
      row.provider = provider;
      row.model = model;
    }
    const stream = state.stream;
    if (stream !== null && stream.conversation === id && stream.error === NO_MODEL) {
      state.stream = null;
    }
    shut();
  });

let timer: number | null = null;

function shut(): void {
  state.picker.open = false;
  state.brevityMenu = false;
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}

export function toggleBrevityMenu(): void {
  state.brevityMenu = !state.brevityMenu;
  render();
}

export function closeBrevityMenu(): void {
  if (!state.brevityMenu) return;
  state.brevityMenu = false;
  render();
}

/// Writes the column immediately; the level applies from the next send on.
export const chooseBrevity = (level: api.BrevityLevel): Promise<void> =>
  guard(async () => {
    const id = state.selected;
    if (id === null) return;
    await api.setConversationBrevity(id, level);
    const row = state.conversations.find((candidate) => candidate.id === id);
    if (row !== undefined) row.brevity = level;
    state.brevityMenu = false;
  });

// A refresh while the menu is open replaces the list without blanking it.
const loadProviders = (): Promise<void> =>
  guard(async () => {
    const groups = await api.providersOverview().finally(() => {
      state.picker.loading = false;
    });
    if (state.picker.open) state.picker.groups = groups;
  });

export function startRename(id: number): void {
  const row = state.conversations.find((candidate) => candidate.id === id);
  if (row === undefined) return;
  state.editing = id;
  state.draft = row.title;
  render();
}

// The input already shows the draft, so nothing has to be redrawn for it.
export function setDraft(title: string): void {
  state.draft = title;
}

export function cancelRename(): void {
  state.editing = null;
  render();
}

export async function commitRename(): Promise<void> {
  const id = state.editing;
  const title = state.draft.trim();
  state.editing = null;
  const row =
    id === null
      ? undefined
      : state.conversations.find((candidate) => candidate.id === id);
  // An empty or unchanged title is a cancel, not a write.
  if (id === null || row === undefined || title === "" || title === row.title) {
    render();
    return;
  }
  await guard(async () => {
    await api.renameConversation(id, title);
    row.title = title;
  });
}

export function watchStream(): void {
  api.onChatEvent(receive);
}

export const streaming = (): boolean =>
  state.stream !== null && state.stream.error === "";

export const send = (prompt: string): Promise<void> => start(prompt, false);

export function resend(): void {
  const stream = state.stream;
  if (stream !== null) void start(stream.prompt, true);
}

export function cancelStream(): void {
  const stream = state.stream;
  if (!streaming() || stream === null || stream.requestId === null) return;
  void api.cancelMessage(stream.requestId);
}

async function start(prompt: string, retry: boolean): Promise<void> {
  const conversation = state.selected;
  if (conversation === null) return;
  // A retry answers a question that is already stored, and already shown.
  if (!retry) state.messages.push({ role: "user", content: prompt, used: [] });
  const stream: Stream = {
    conversation,
    requestId: null,
    prompt,
    text: "",
    error: "",
    used: [],
  };
  state.stream = stream;
  render();
  await guard(async () => {
    const requestId = await api.sendMessage(conversation, prompt, retry);
    if (state.stream !== stream) return;
    stream.requestId = requestId;
    const queued = pending.filter((event) => event.request_id === requestId);
    pending = [];
    for (const event of queued) apply(event, stream);
  });
}

// An event can arrive before the id it belongs to gets back from the backend,
// so anything unmatched waits for that id instead of being dropped.
let pending: api.ChatEvent[] = [];

function receive(event: api.ChatEvent): void {
  const stream = state.stream;
  if (stream === null) return;
  if (stream.requestId === null) {
    pending.push(event);
    return;
  }
  if (event.request_id === stream.requestId) apply(event, stream);
}

function apply(event: api.ChatEvent, stream: Stream): void {
  if (event.kind === "context") {
    stream.used = event.used;
    render();
    return;
  }
  if (event.kind === "delta") {
    stream.text += event.text;
    renderStream();
    return;
  }
  // A failed stream keeps its partial text on screen, and its retry link.
  if (event.kind === "error") {
    stream.error = event.message;
    render();
    return;
  }
  state.stream = null;
  void guard(async () => {
    // The stored turn is the truth now: it carries the interrupted marker and
    // the token counts the crumbs read.
    if (state.selected === stream.conversation) await open(stream.conversation);
  });
}

async function open(id: number): Promise<void> {
  const opened = await api.getConversation(id);
  state.selected = opened.id;
  state.turns = opened.turns;
  state.tokens = opened.tokens;
  state.messages = await api.messages(id);
  // The first message titles a conversation, so the sidebar learns it here.
  const row = state.conversations.find((candidate) => candidate.id === id);
  if (row !== undefined) row.title = opened.title;
}

const BRAIN_PAGE = 50;
const SEARCH_DEBOUNCE_MS = 300;
let loadingMore = false;
let searchTimer: number | null = null;
let searchSeq = 0;

export const loadBrain = (): Promise<void> =>
  guard(async () => {
    state.brain.overview = await api.brainOverview();
    state.brain.episodic = await api.brainEpisodic(state.brain.sort, 0);
    state.brain.exhausted = state.brain.episodic.length < BRAIN_PAGE;
  });

export async function loadMoreEpisodic(): Promise<void> {
  const brain = state.brain;
  if (brain.exhausted || brain.results !== null || loadingMore) return;
  loadingMore = true;
  await guard(async () => {
    const page = await api.brainEpisodic(brain.sort, brain.episodic.length);
    brain.episodic.push(...page);
    if (page.length < BRAIN_PAGE) brain.exhausted = true;
  });
  loadingMore = false;
}

export const setBrainSort = (sort: api.EpisodicSort): Promise<void> =>
  guard(async () => {
    state.brain.sort = sort;
    state.brain.episodic = await api.brainEpisodic(sort, 0);
    state.brain.exhausted = state.brain.episodic.length < BRAIN_PAGE;
  });

export function setBrainMode(mode: "list" | "graph"): void {
  state.brain.mode = mode;
  if (mode === "graph") void loadBrainGraph();
  render();
}

export const loadBrainGraph = (): Promise<void> =>
  guard(async () => {
    state.brain.graph = await api.brainGraph();
  });

// The search input holds its own text; only results trigger a redraw.
export function scheduleBrainSearch(query: string): void {
  state.brain.query = query;
  if (searchTimer !== null) clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => {
    searchTimer = null;
    void runBrainSearch(query);
  }, SEARCH_DEBOUNCE_MS);
}

async function runBrainSearch(query: string): Promise<void> {
  const seq = ++searchSeq;
  if (query.trim() === "") {
    state.brain.results = null;
    render();
    return;
  }
  await guard(async () => {
    const results = await api.brainSearch(query);
    if (seq === searchSeq) state.brain.results = results;
  });
}

export function startMemoryEdit(editing: number | "new"): void {
  state.brain.editing = editing;
  render();
}

export function cancelMemoryEdit(): void {
  state.brain.editing = null;
  render();
}

export const commitMemoryEdit = (content: string): Promise<void> =>
  guard(async () => {
    const editing = state.brain.editing;
    state.brain.editing = null;
    const text = content.trim();
    // An empty edit is a cancel, matching how renames behave.
    if (editing === null || text === "") return;
    if (editing === "new") await api.brainAddCore(text);
    else await api.brainUpdateCore(editing, text);
    await loadBrain();
  });

export const removeMemory = (id: number): Promise<void> =>
  guard(async () => {
    await api.brainDeleteMemory(id);
    await loadBrain();
  });

export const loadConfig = (): Promise<void> =>
  guard(async () => {
    state.config.file = await api.configFile();
  });

export const loadProvidersConfig = (): Promise<void> =>
  guard(async () => {
    // What the last visit connected is not news on this one.
    state.providers.connected = null;
    state.providers.entries = await api.providersConfig();
    state.providers.catalog = await api.providerCatalog();
  });

// Which endpoint the key in the panel is for. Aiming is not connecting: the
// key stays where it is, and nothing is written until connect is asked for.
export function pickCatalogProvider(id: string | null): void {
  if (state.providers.pick === id) return;
  state.providers.pick = id;
  render();
}

/// One paste, one write: the endpoint is asked what it serves, the answer
/// picks the starting model, and the table lands in `odyn.toml`.
export const connectProvider = (
  id: string,
  apiKey: string,
  makeDefault: boolean,
): Promise<void> =>
  guard(async () => {
    state.providers.connecting = true;
    state.providers.connected = null;
    render();
    try {
      const result = await api.providerConnect(id, apiKey, makeDefault);
      state.providers.entries = result.providers;
      state.providers.catalog = await api.providerCatalog();
      state.providers.pick = null;
      state.providers.connected = summarise(result);
    } finally {
      state.providers.connecting = false;
    }
    void refreshStatus();
  });

function summarise(result: api.Connected): string {
  const models =
    result.models === 0 ? "" : ` · ${result.models} model${result.models === 1 ? "" : "s"}`;
  const model = result.model === null ? "" : ` · starting on ${result.model}`;
  const note = result.note === null ? "" : ` · ${result.note}`;
  return `${result.name} connected${models}${model}${note}`;
}

export const openKeysPage = (id: string): Promise<void> =>
  guard(() => api.openKeysPage(id));

export function startProviderEdit(name: string | null): void {
  state.providers.editing = { name };
  render();
}

export function cancelProviderEdit(): void {
  state.providers.editing = null;
  render();
}

// Every write answers with the reloaded list, so what the view shows is what
// the running app just adopted. The footer probes again on its own time.
export const saveProvider = (draft: api.ProviderDraft): Promise<void> =>
  guard(async () => {
    state.providers.entries = await api.providerSave(draft);
    state.providers.catalog = await api.providerCatalog();
    state.providers.editing = null;
    void refreshStatus();
  });

export const deleteProvider = (name: string): Promise<void> =>
  guard(async () => {
    state.providers.entries = await api.providerRemove(name);
    state.providers.catalog = await api.providerCatalog();
    void refreshStatus();
  });

export const chooseDefaultProvider = (name: string): Promise<void> =>
  guard(async () => {
    state.providers.entries = await api.setDefaultProvider(name);
    void refreshStatus();
  });

// The system's default program for the file; which one that is belongs to the
// OS, not to us.
export const openConfigInEditor = (): Promise<void> =>
  guard(() => api.openConfig());

const PREVIEW_DEBOUNCE_MS = 400;
let previewTimer: number | null = null;
let previewSeq = 0;

// The ledger previews what a send would inject right now: ≥400ms after the
// last keystroke, never applied out of order.
export function schedulePreview(draft: string): void {
  if (previewTimer !== null) clearTimeout(previewTimer);
  previewTimer = window.setTimeout(() => {
    previewTimer = null;
    void refreshPreview(draft);
  }, PREVIEW_DEBOUNCE_MS);
}

export async function refreshPreview(draft: string): Promise<void> {
  if (state.selected === null) return;
  const seq = ++previewSeq;
  try {
    const preview = await api.contextPreview(state.selected, draft);
    if (seq !== previewSeq) return;
    state.ledger.preview = preview;
    state.ledger.error = null;
  } catch (err) {
    if (seq !== previewSeq) return;
    state.ledger.error = typeof err === "string" ? err : String(err);
  }
  render();
}

export function expandLedger(): void {
  state.ledger.expanded = true;
  render();
}

// Every failure the backend reports — including a config or database that
// never loaded — ends up on one inline line instead of a dialog.
async function guard(run: () => Promise<void>): Promise<void> {
  try {
    state.error = "";
    await run();
  } catch (err) {
    state.error = typeof err === "string" ? err : String(err);
  }
  render();
}
