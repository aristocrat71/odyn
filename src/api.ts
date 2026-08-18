import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type BrevityLevel = "off" | "lite" | "full" | "ultra";

export type Conversation = {
  id: number;
  title: string;
  provider: string;
  model: string;
  // Unix epoch seconds.
  updated_at: number;
  // The conversation's explicit choice; null follows the [style] default.
  brevity: BrevityLevel | null;
  // The agent workspace folder; null is a normal conversation.
  workspace: string | null;
};

export type ConversationView = Conversation & {
  turns: number;
  tokens: number | null;
};

export type Message = {
  id: number;
  role: "system" | "user" | "assistant";
  content: string;
  // Assistant rows: the slugs injected for the question this answers.
  used: string[];
  // Assistant rows: the tool actions this reply ran, kept for five days.
  commands: string[];
};

export type SearchHit = {
  conversation_id: number;
  title: string;
  message_id: number;
  role: "user" | "assistant";
  // Matched terms sit between the U+0001 and U+0002 markers.
  snippet: string;
};

export type Usage = { input_tokens: number; output_tokens: number };

export type ChatEvent = { request_id: number } & (
  | { kind: "context"; used: string[]; tokens: number; soul: number }
  | { kind: "delta"; text: string }
  | { kind: "saved"; slug: string }
  | { kind: "updated"; slug: string }
  | { kind: "deleted"; slug: string }
  | { kind: "linked"; from: string; to: string }
  | { kind: "unlinked"; from: string; to: string }
  | { kind: "reminded"; text: string; due_at: number }
  | { kind: "scheduled"; prompt: string; next_at: number }
  | { kind: "approval"; approval_id: number; command: string }
  | { kind: "agentcall"; tool: string; detail: string }
  | { kind: "agentout"; text: string; truncated: boolean }
  | { kind: "round"; used: number; budget: number }
  | { kind: "done"; usage: Usage | null; interrupted: boolean }
  | { kind: "error"; message: string }
);

export type ApprovalVerdict = "run" | "always" | "deny";

export type LedgerItem = { id: string; tokens: number; content: string };

export type ContextPreview = {
  // False when the draft has no /brain mention: the send would inject nothing.
  active: boolean;
  memories: LedgerItem[];
  tokens: number;
  cap_tokens: number;
  // soul.md's standing cost, injected on every turn; 0 when there is none.
  soul_tokens: number;
  soul_over: boolean;
  system_message: string;
};

export type Model = {
  name: string;
  // On-disk size, which only Ollama reports.
  size_bytes: number | null;
  // Whether the model can call tools; null when nothing reported it.
  tools: boolean | null;
};

export type ProviderGroup = {
  name: string;
  kind: "openai_compat" | "ollama";
  reachable: boolean;
  models: Model[];
};

export type Status = {
  brevity_default: BrevityLevel;
};

export const listConversations = (): Promise<Conversation[]> =>
  invoke("list_conversations");

export const createConversation = (): Promise<Conversation> =>
  invoke("create_conversation");

export const renameConversation = (id: number, title: string): Promise<void> =>
  invoke("rename_conversation", { id, title });

export const deleteConversation = (id: number): Promise<void> =>
  invoke("delete_conversation", { id });

export const setConversationBrevity = (
  conversationId: number,
  brevity: BrevityLevel,
): Promise<void> =>
  invoke("set_conversation_brevity", { conversationId, brevity });

export const setConversationModel = (
  conversationId: number,
  provider: string,
  model: string,
): Promise<void> =>
  invoke("set_conversation_model", { conversationId, provider, model });

export const getConversation = (id: number): Promise<ConversationView> =>
  invoke("get_conversation", { id });

export const messages = (conversationId: number): Promise<Message[]> =>
  invoke("messages", { conversationId });

export const searchMessages = (query: string): Promise<SearchHit[]> =>
  invoke("search_messages", { query });

export const sendMessage = (
  conversationId: number,
  text: string,
  retry: boolean,
): Promise<number> => invoke("send_message", { conversationId, text, retry });

export const cancelMessage = (requestId: number): Promise<void> =>
  invoke("cancel_message", { requestId });

// Empty path clears; anything else must be an existing folder.
export const setWorkspace = (
  conversationId: number,
  path: string,
): Promise<Conversation> => invoke("set_workspace", { conversationId, path });

export const approveCommand = (
  approvalId: number,
  verdict: ApprovalVerdict,
): Promise<void> => invoke("approve_command", { approvalId, verdict });

export const contextPreview = (
  conversationId: number | null,
  draft: string,
): Promise<ContextPreview> =>
  invoke("context_preview", { conversationId, draft });

export type MemorySort = "recent" | "hits" | "created";

export type MemoryRow = {
  id: number;
  slug: string;
  content: string;
  tokens: number;
  hits: number;
  last_injected_at: number | null;
  created_at: number;
};

export type BrainOverview = {
  count: number;
  top_k: number;
  cap_tokens: number;
  path: string;
  model: string;
  // Whether that model sends note text off the machine.
  model_remote: boolean;
  // The width the index was built at; 0 before anything is built.
  dim: number;
  save_temperature: number;
  min_relevance: number;
};

export type EmbedOption = {
  id: string;
  backend: "builtin" | "ollama" | "provider";
  // Known ahead of time only for the bundled models.
  dim: number | null;
  description: string;
  remote: boolean;
};

export const brainOverview = (): Promise<BrainOverview> =>
  invoke("brain_overview");

export const embedCatalog = (): Promise<EmbedOption[]> =>
  invoke("embed_catalog");

// Writes the config key and re-indexes; slow by nature.
export const brainSetModel = (model: string): Promise<BrainOverview> =>
  invoke("brain_set_model", { model });

export const brainSetSaveTemperature = (value: number): Promise<BrainOverview> =>
  invoke("brain_set_save_temperature", { value });

export const brainSetTopK = (value: number): Promise<BrainOverview> =>
  invoke("brain_set_top_k", { value });

export const brainSetMinRelevance = (value: number): Promise<BrainOverview> =>
  invoke("brain_set_min_relevance", { value });

export const brainMemories = (
  sort: MemorySort,
  offset: number,
): Promise<MemoryRow[]> => invoke("brain_memories", { sort, offset });

export const brainSearch = (query: string): Promise<MemoryRow[]> =>
  invoke("brain_search", { query });

export const brainAddNote = (content: string): Promise<MemoryRow> =>
  invoke("brain_add_note", { content });

export const brainUpdateNote = (
  slug: string,
  content: string,
): Promise<MemoryRow> => invoke("brain_update_note", { slug, content });

export const brainDeleteNote = (slug: string): Promise<void> =>
  invoke("brain_delete_note", { slug });

export type GraphNode = {
  id: number;
  display_id: string;
  content: string;
  hits: number;
  x: number;
  y: number;
};

export type GraphEdge = {
  a: number;
  b: number;
  kind: "link" | "similarity" | "coinjection";
  weight: number;
};

export type Graph = { nodes: GraphNode[]; edges: GraphEdge[] };

export const brainGraph = (): Promise<Graph> => invoke("brain_graph");

export type ConfigFile = { path: string; text: string };

export type ReminderRow = {
  id: number;
  text: string;
  due_at: number;
  // Null while it is still waiting.
  fired_at: number | null;
  // The every-phrase of a repeating reminder; null is one-shot.
  repeat: string | null;
};

export type ScheduleRow = {
  id: number;
  prompt: string;
  repeat: string;
  next_at: number;
  last_run_at: number | null;
  // What the last run failed with; null after a clean one.
  last_error: string | null;
};

export type ReminderList = {
  pending: ReminderRow[];
  past: ReminderRow[];
  schedules: ScheduleRow[];
};

export const remindersList = (): Promise<ReminderList> => invoke("reminders_list");

export const reminderDelete = (id: number): Promise<void> =>
  invoke("reminder_delete", { id });

export const scheduleDelete = (id: number): Promise<void> =>
  invoke("schedule_delete", { id });

export const configFile = (): Promise<ConfigFile> => invoke("config_file");

export const openConfig = (): Promise<void> => invoke("open_config");

export type ProviderEntry = {
  name: string;
  kind: "openai_compat" | "ollama";
  base_url: string;
  default: boolean;
  default_model: string | null;
  keep_alive: string | null;
  // The key itself never crosses to the frontend — only whether one is there.
  key_stored: boolean;
  key_env: string | null;
  key_env_set: boolean;
};

export type ProviderDraft = {
  name: string;
  kind: string;
  base_url: string;
  // Blank means "unchanged" for a provider that already stores a key.
  api_key: string | null;
  api_key_env: string | null;
  default_model: string | null;
  keep_alive: string | null;
  make_default: boolean;
};

// A provider Odyn already knows the endpoint for: everything but the key.
export type CatalogItem = {
  id: string;
  label: string;
  kind: "openai_compat" | "ollama";
  base_url: string;
  needs_key: boolean;
  keys_url: string;
  // Key shapes only this provider issues, so a pasted key names its own.
  key_prefixes: string[];
  configured: boolean;
};

export type Connected = {
  name: string;
  model: string | null;
  models: number;
  // Why the model list is empty, when the endpoint would not give one.
  note: string | null;
  providers: ProviderEntry[];
};

export const providersConfig = (): Promise<ProviderEntry[]> =>
  invoke("providers_config");

export const providerCatalog = (): Promise<CatalogItem[]> =>
  invoke("provider_catalog");

export const providerConnect = (
  id: string,
  apiKey: string,
  makeDefault: boolean,
): Promise<Connected> =>
  invoke("provider_connect", { id, apiKey, makeDefault });

export const openKeysPage = (id: string): Promise<void> =>
  invoke("open_keys_page", { id });

export const providerSave = (draft: ProviderDraft): Promise<ProviderEntry[]> =>
  invoke("provider_save", { draft });

export const providerRemove = (name: string): Promise<ProviderEntry[]> =>
  invoke("provider_remove", { name });

export const setDefaultProvider = (name: string): Promise<ProviderEntry[]> =>
  invoke("set_default_provider", { name });

export const reloadConfig = (): Promise<void> => invoke("reload_config");

export const onChatEvent = (handle: (event: ChatEvent) => void): void => {
  void listen<ChatEvent>("chat-event", (event) => handle(event.payload));
};

export const status = (): Promise<Status> => invoke("status");

export const spotlightStatus = (): Promise<string | null> =>
  invoke("spotlight_status");

export const spotlightToggle = (): Promise<void> => invoke("spotlight_toggle");

export const providersOverview = (): Promise<ProviderGroup[]> =>
  invoke("providers_overview");
