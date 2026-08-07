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
};

export type ConversationView = Conversation & {
  turns: number;
  tokens: number | null;
};

export type Message = {
  role: "system" | "user" | "assistant";
  content: string;
  // Assistant rows: the note slugs injected for the question this answers.
  used: string[];
};

export type Usage = { input_tokens: number; output_tokens: number };

export type ChatEvent = { request_id: number } & (
  | { kind: "context"; used: string[]; tokens: number }
  | { kind: "delta"; text: string }
  | { kind: "done"; usage: Usage | null; interrupted: boolean }
  | { kind: "error"; message: string }
);

export type LedgerItem = { id: string; tokens: number; content: string };

export type ContextPreview = {
  // False when the draft has no /brain mention: the send would inject nothing.
  active: boolean;
  memories: LedgerItem[];
  tokens: number;
  cap_tokens: number;
  system_message: string;
};

export type Model = {
  name: string;
  // On-disk size, which only Ollama reports.
  size_bytes: number | null;
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

export const sendMessage = (
  conversationId: number,
  text: string,
  retry: boolean,
): Promise<number> => invoke("send_message", { conversationId, text, retry });

export const cancelMessage = (requestId: number): Promise<void> =>
  invoke("cancel_message", { requestId });

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
  // The brain folder of .md notes, spelled out.
  path: string;
  model: string;
};

export const brainOverview = (): Promise<BrainOverview> =>
  invoke("brain_overview");

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
