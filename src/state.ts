import * as api from "./api";

export const VIEWS = ["brain", "reminders", "providers", "config"] as const;

export type View = (typeof VIEWS)[number];

export const isView = (name: string): name is View =>
  (VIEWS as readonly string[]).includes(name);

export const state = {
  view: "brain" as View,
  hotkeyError: null as string | null,
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
  reminders: {
    list: null as api.ReminderList | null,
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

export function onChange(fn: () => void): void {
  render = fn;
}

export const refreshStatus = (): Promise<void> =>
  guard(async () => {
    state.hotkeyError = await api.spotlightStatus();
  });

export function setView(view: View): void {
  state.view = view;
  if (view === "brain") void loadBrain();
  // Re-read on every open, so an edit made outside the app is on screen.
  if (view === "config") void loadConfig();
  if (view === "reminders") void loadReminders();
  if (view === "providers") void loadProvidersConfig();
  render();
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

export const loadReminders = (): Promise<void> =>
  guard(async () => {
    state.reminders.list = await api.remindersList();
  });

export const cancelReminder = (id: number): Promise<void> =>
  guard(async () => {
    await api.reminderDelete(id);
    await loadReminders();
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
