import * as api from "./api";

export const VIEWS = [
  "home",
  "chat",
  "conversations",
  "brain",
  "providers",
  "config",
  "guide",
] as const;

export type View = (typeof VIEWS)[number];

export const isView = (name: string): name is View =>
  (VIEWS as readonly string[]).includes(name);

export type PickerMenu = "provider" | "model" | null;

export type Stream = {
  conversation: number;
  requestId: number | null;
  prompt: string;
  text: string;
  error: string;
  // Note slugs the backend injected for this reply.
  used: string[];
  saved: string[];
  updated: string[];
  deleted: string[];
};

// The backend's refusal when a conversation has no model; picking one clears it.
const NO_MODEL = "no model set · pick one";
const PICKER_REFRESH_MS = 30_000;

export const state = {
  view: "home" as View,
  conversations: [] as api.Conversation[],
  selected: null as number | null,
  messages: [] as api.Message[],
  turns: 0,
  tokens: null as number | null,
  stream: null as Stream | null,
  picker: {
    open: null as PickerMenu,
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
  },
  brain: {
    mode: "list" as "list" | "graph",
    overview: null as api.BrainOverview | null,
    memories: [] as api.MemoryRow[],
    sort: "recent" as api.MemorySort,
    exhausted: false,
    query: "",
    // `null` means browsing; a search replaces the list, not filters it.
    results: null as api.MemoryRow[] | null,
    // The slug being edited in place; "new" is the add-note row.
    editing: null as string | null,
    graph: null as api.Graph | null,
    models: null as api.EmbedOption[] | null,
    // Set while a swap re-embeds the whole folder.
    swapping: false,
  },
  config: {
    file: null as api.ConfigFile | null,
  },
  providers: {
    entries: null as api.ProviderEntry[] | null,
    // `{ name: null }` is the add form; a string names the row being edited.
    editing: null as { name: string | null } | null,
    catalog: null as api.CatalogItem[] | null,
    connect: false,
    // The catalog entry the connect panel is aimed at.
    pick: null as string | null,
    connecting: false,
    // The last connection's summary, shown until the next one starts.
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

export function togglePicker(which: PickerMenu): void {
  if (state.picker.open === which) {
    closePicker();
    return;
  }
  // Only the first open probes: provider → model is the same listing twice.
  const first = state.picker.open === null;
  state.picker.open = which;
  if (first) {
    // Reachability from a minute ago is not a fact: nothing stale is shown.
    state.picker.loading = true;
    state.picker.groups = [];
    void loadProviders();
    timer = window.setInterval(() => void loadProviders(), PICKER_REFRESH_MS);
  }
  render();
}

export function closePicker(): void {
  if (state.picker.open === null) return;
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
  state.picker.open = null;
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
    if (state.picker.open !== null) state.picker.groups = groups;
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
    saved: [],
    updated: [],
    deleted: [],
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
  if (event.kind === "saved") {
    stream.saved.push(event.slug);
    render();
    return;
  }
  if (event.kind === "updated") {
    stream.updated.push(event.slug);
    render();
    return;
  }
  if (event.kind === "deleted") {
    stream.deleted.push(event.slug);
    render();
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
    state.conversations = await api.listConversations();
    // The stored turn is the truth now: interrupted marker and token counts.
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
    state.brain.memories = await api.brainMemories(state.brain.sort, 0);
    state.brain.exhausted = state.brain.memories.length < BRAIN_PAGE;
    // Probes every configured endpoint, so it lands after the list.
    void loadEmbedModels();
  });

const loadEmbedModels = (): Promise<void> =>
  guard(async () => {
    state.brain.models = await api.embedCatalog();
  });

/// Swapping re-embeds every note, so the view says so rather than look hung.
export const chooseSaveTemperature = (value: number): Promise<void> =>
  guard(async () => {
    if (value === state.brain.overview?.save_temperature) return;
    state.brain.overview = await api.brainSetSaveTemperature(value);
    render();
  });

export const chooseTopK = (value: number): Promise<void> =>
  guard(async () => {
    if (value === state.brain.overview?.top_k) return;
    state.brain.overview = await api.brainSetTopK(value);
    render();
  });

export const chooseMinRelevance = (value: number): Promise<void> =>
  guard(async () => {
    if (value === state.brain.overview?.min_relevance) return;
    state.brain.overview = await api.brainSetMinRelevance(value);
    render();
  });

export const chooseEmbedModel = (model: string): Promise<void> =>
  guard(async () => {
    if (model === state.brain.overview?.model) return;
    state.brain.swapping = true;
    render();
    try {
      state.brain.overview = await api.brainSetModel(model);
      state.brain.memories = await api.brainMemories(state.brain.sort, 0);
      state.brain.graph = null;
      if (state.brain.mode === "graph") await loadBrainGraph();
    } finally {
      state.brain.swapping = false;
    }
  });

export async function loadMoreMemories(): Promise<void> {
  const brain = state.brain;
  if (brain.exhausted || brain.results !== null || loadingMore) return;
  loadingMore = true;
  await guard(async () => {
    const page = await api.brainMemories(brain.sort, brain.memories.length);
    brain.memories.push(...page);
    if (page.length < BRAIN_PAGE) brain.exhausted = true;
  });
  loadingMore = false;
}

export const setBrainSort = (sort: api.MemorySort): Promise<void> =>
  guard(async () => {
    state.brain.sort = sort;
    state.brain.memories = await api.brainMemories(sort, 0);
    state.brain.exhausted = state.brain.memories.length < BRAIN_PAGE;
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

export function startMemoryEdit(editing: string): void {
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
    if (editing === null || text === "") return;
    if (editing === "new") await api.brainAddNote(text);
    else await api.brainUpdateNote(editing, text);
    await loadBrain();
  });

export const removeMemory = (slug: string): Promise<void> =>
  guard(async () => {
    await api.brainDeleteNote(slug);
    await loadBrain();
  });

export const loadConfig = (): Promise<void> =>
  guard(async () => {
    state.config.file = await api.configFile();
  });

export const loadProvidersConfig = (): Promise<void> =>
  guard(async () => {
    state.providers.connected = null;
    state.providers.connect = false;
    state.providers.entries = await api.providersConfig();
    state.providers.catalog = await api.providerCatalog();
  });

// Aiming is not connecting: nothing is written until connect is asked for.
export function pickCatalogProvider(id: string | null): void {
  if (state.providers.pick === id) return;
  state.providers.pick = id;
  render();
}

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
  state.providers.connect = false;
  render();
}

// Both write the same file, so only one of them is ever on screen.
export function openConnect(open: boolean): void {
  state.providers.connect = open;
  if (open) state.providers.editing = null;
  else state.providers.connected = null;
  render();
}

export function cancelProviderEdit(): void {
  state.providers.editing = null;
  render();
}

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

export const openConfigInEditor = (): Promise<void> =>
  guard(() => api.openConfig());

const PREVIEW_DEBOUNCE_MS = 400;
let previewTimer: number | null = null;
let previewSeq = 0;

// ≥400ms after the last keystroke, and never applied out of order.
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

// Every failure the backend reports ends up on one inline line, never a dialog.
async function guard(run: () => Promise<void>): Promise<void> {
  try {
    state.error = "";
    await run();
  } catch (err) {
    state.error = typeof err === "string" ? err : String(err);
  }
  render();
}
