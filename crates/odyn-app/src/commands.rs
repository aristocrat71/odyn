//! The frontend's whole surface. Every command is a wrapper: shaping for the
//! wire happens here, everything else happens in odyn-core.

use std::sync::Arc;

use odyn_core::brain::{self, Ask, InjectedContext};
use odyn_core::brevity::Brevity;
use odyn_core::chat::{ChatError, ChatProvider, Message, Role, ToolDef, Usage};
use odyn_core::config::{BrainConfig, ProviderConfig};
use odyn_core::embed::{self, load_embedder};
use odyn_core::notes;
use odyn_core::providers::ollama::OllamaProvider;
use odyn_core::providers::openai_compat::OpenAiCompatProvider;
use odyn_core::providers::{ollama, openai_compat};
use odyn_core::storage::{Conversation as StoredConversation, StorageError};
use odyn_core::tools::{self, TurnError, TurnEvent};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::{AppState, Ready, Stream};

const NEW_TITLE: &str = "new conversation";
const TITLE_CHARS: usize = 40;
pub(crate) const INTERRUPTED: &str = " (interrupted)";
const NO_MODEL: &str = "no model set · pick one";
const CHAT_EVENT: &str = "chat-event";

#[derive(serde::Serialize)]
pub struct Conversation {
    id: i64,
    title: String,
    provider: String,
    model: String,
    updated_at: i64,
    /// The conversation's explicit choice; `None` follows `[style] brevity`.
    brevity: Option<Brevity>,
}

#[derive(serde::Serialize)]
pub struct ConversationView {
    id: i64,
    title: String,
    provider: String,
    model: String,
    turns: usize,
    /// `None` until a provider reports usage; never invented.
    tokens: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct MessageView {
    id: i64,
    role: Role,
    content: String,
    /// Assistant rows only: the slugs injected for the question this answers.
    used: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct SearchHit {
    conversation_id: i64,
    title: String,
    message_id: i64,
    role: Role,
    /// Matched terms sit between `\u{1}` and `\u{2}`; the view marks them.
    snippet: String,
}

#[derive(serde::Serialize)]
pub struct LedgerItem {
    id: String,
    tokens: i64,
    content: String,
}

/// What the composer ledger renders, built by the send path's own `build_context`.
#[derive(serde::Serialize)]
pub struct ContextPreview {
    /// Whether the draft mentions `/brain`; false means the send injects nothing.
    active: bool,
    memories: Vec<LedgerItem>,
    tokens: i64,
    cap_tokens: u32,
    /// soul.md's standing cost, on every turn; 0 when there is none.
    soul_tokens: i64,
    soul_over: bool,
    system_message: String,
}

#[derive(serde::Serialize)]
pub struct ProviderGroup {
    name: String,
    kind: &'static str,
    reachable: bool,
    models: Vec<Model>,
}

#[derive(serde::Serialize)]
pub struct Model {
    pub(crate) name: String,
    /// On-disk size; only Ollama reports it, and it is never invented.
    size_bytes: Option<u64>,
    /// Whether the model can call tools; `None` when nothing reported it.
    tools: Option<bool>,
}

#[derive(serde::Serialize)]
pub struct Status {
    /// `[style] brevity` — what a conversation without its own choice uses.
    brevity_default: Brevity,
}

/// One shape for the whole stream: keyed on `request_id`, switched on `kind`.
#[derive(Clone, serde::Serialize)]
pub(crate) struct Event {
    pub(crate) request_id: u64,
    #[serde(flatten)]
    pub(crate) body: Body,
}

#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Body {
    /// What was injected for this reply, before its first delta.
    Context {
        used: Vec<String>,
        tokens: i64,
        soul: i64,
    },
    Delta {
        text: String,
    },
    Saved {
        slug: String,
    },
    Updated {
        slug: String,
    },
    Deleted {
        slug: String,
    },
    Linked {
        from: String,
        to: String,
    },
    Unlinked {
        from: String,
        to: String,
    },
    Reminded {
        text: String,
        due_at: i64,
    },
    Done {
        usage: Option<Usage>,
        interrupted: bool,
    },
    Error {
        message: String,
        /// The provider's own words; logged to the webview console, never rendered.
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl Body {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            detail: None,
        }
    }
}

/// What a reply ended as. An interruption is not a failure: it keeps its text.
enum Outcome {
    Done(Option<Usage>),
    Interrupted,
    Failed(String),
}

#[tauri::command]
pub async fn list_conversations(state: State<'_, AppState>) -> Result<Vec<Conversation>, String> {
    let ready = state.ready()?;
    let rows = ready.storage().list_conversations().map_err(say)?;
    Ok(rows.into_iter().map(Conversation::from).collect())
}

#[tauri::command]
pub async fn create_conversation(state: State<'_, AppState>) -> Result<Conversation, String> {
    let ready = state.ready()?;
    let storage = ready.storage();
    let last = storage.latest_conversation().map_err(say)?;
    let (provider, model) = inherited(&ready, last);
    let row = storage
        .create_conversation(NEW_TITLE, &provider, &model)
        .map_err(say)?;
    Ok(Conversation::from(row))
}

/// A new chat opens on the last one's target. A provider that has since left
/// the config falls back to the default.
fn inherited(
    ready: &crate::state::Ready,
    last: Option<odyn_core::storage::Conversation>,
) -> (String, String) {
    if let Some(row) = last {
        if ready.config.providers.contains_key(&row.provider) {
            return (row.provider, row.model);
        }
    }
    let provider = ready.registry.default_provider_name().to_string();
    // No default model is a real state for Ollama; the picker fills it in.
    let model = ready
        .config
        .default_model(&provider)
        .unwrap_or_default()
        .to_string();
    (provider, model)
}

#[tauri::command]
pub async fn rename_conversation(
    state: State<'_, AppState>,
    id: i64,
    title: String,
) -> Result<(), String> {
    let ready = state.ready()?;
    let renamed = ready.storage().rename_conversation(id, &title);
    renamed.map_err(say)
}

#[tauri::command]
pub async fn delete_conversation(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let ready = state.ready()?;
    let deleted = ready.storage().delete_conversation(id);
    deleted.map_err(say)
}

/// An explicit level for this conversation: it affects the next send, never the past.
#[tauri::command]
pub async fn set_conversation_brevity(
    state: State<'_, AppState>,
    conversation_id: i64,
    brevity: Brevity,
) -> Result<(), String> {
    let ready = state.ready()?;
    let set = ready
        .storage()
        .set_conversation_brevity(conversation_id, brevity);
    set.map_err(say)
}

#[tauri::command]
pub async fn set_conversation_model(
    state: State<'_, AppState>,
    conversation_id: i64,
    provider: String,
    model: String,
) -> Result<(), String> {
    let ready = state.ready()?;
    let set = ready
        .storage()
        .set_conversation_model(conversation_id, &provider, &model);
    set.map_err(say)
}

#[tauri::command]
pub async fn get_conversation(
    state: State<'_, AppState>,
    id: i64,
) -> Result<ConversationView, String> {
    let ready = state.ready()?;
    let row = conversation(&ready, id)?;
    let stored = ready.storage().messages(id).map_err(say)?;
    Ok(ConversationView {
        id: row.id,
        title: row.title,
        provider: row.provider,
        model: row.model,
        // A turn is a question and its answer, so count the questions.
        turns: stored
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        tokens: stored
            .iter()
            .flat_map(|message| [message.input_tokens, message.output_tokens])
            .flatten()
            .reduce(|total, count| total + count),
    })
}

/// Injections are recorded against the question; the trace line renders under
/// the answer, so each assistant row carries the ids of the user row before it.
#[tauri::command]
pub async fn messages(
    state: State<'_, AppState>,
    conversation_id: i64,
) -> Result<Vec<MessageView>, String> {
    let ready = state.ready()?;
    let storage = ready.storage();
    let rows = storage.messages(conversation_id).map_err(say)?;
    let slugs: std::collections::HashMap<i64, String> = storage
        .list_memories()
        .map_err(say)?
        .into_iter()
        .map(|memory| (memory.id, memory.slug))
        .collect();
    let mut by_question: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    for injection in storage.injections(conversation_id).map_err(say)? {
        let Some(message_id) = injection.message_id else {
            continue;
        };
        if let Some(slug) = slugs.get(&injection.memory_id) {
            by_question
                .entry(message_id)
                .or_default()
                .push(slug.clone());
        }
    }

    let mut question = None;
    Ok(rows
        .into_iter()
        .map(|row| {
            let used = match row.role {
                Role::User => {
                    question = Some(row.id);
                    Vec::new()
                }
                Role::Assistant => question
                    .take()
                    .and_then(|id| by_question.remove(&id))
                    .unwrap_or_default(),
                Role::System | Role::Tool => Vec::new(),
            };
            MessageView {
                id: row.id,
                role: row.role,
                content: row.content,
                used,
            }
        })
        .collect())
}

/// Full-text search across every conversation's messages, best match first.
#[tauri::command]
pub async fn search_messages(
    state: State<'_, AppState>,
    query: String,
) -> Result<Vec<SearchHit>, String> {
    let ready = state.ready()?;
    let hits = ready.storage().search_messages(&query, 40).map_err(say)?;
    Ok(hits
        .into_iter()
        .map(|hit| SearchHit {
            conversation_id: hit.conversation_id,
            title: hit.title,
            message_id: hit.message_id,
            role: hit.role,
            snippet: hit.snippet,
        })
        .collect())
}

/// Answers with the id the reply's events carry. `retry` re-runs a turn whose
/// question is already stored, so a failed stream is not asked twice.
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: i64,
    text: String,
    retry: bool,
) -> Result<u64, String> {
    let ready = state.ready()?;
    let row = conversation(&ready, conversation_id)?;
    // A `/brain` mention turns recall on; the transcript and the model both
    // see the message without it.
    let mut ask = brain::parse_ask(&text);
    if !retry {
        let storage = ready.storage();
        storage
            .append_message(conversation_id, Role::User, &ask.message, None, None)
            .map_err(say)?;
        if row.title == NEW_TITLE {
            storage
                .rename_conversation(conversation_id, &title_from(&ask.message))
                .map_err(say)?;
        }
    }
    let rows = ready.storage().messages(conversation_id).map_err(say)?;
    // The question's row: the trace and the injections record hang off it.
    let question_id = rows
        .iter()
        .rev()
        .find(|row| row.role == Role::User)
        .map(|row| row.id);
    // A retry resends the stored, already-cleaned question: whether the
    // original turn recalled is what its injection record says.
    if retry && !ask.recall {
        let injections = ready.storage().injections(conversation_id).map_err(say)?;
        ask.recall = injections
            .iter()
            .any(|injection| injection.message_id == question_id);
    }
    // Everything before the question feeds retrieval; the question is `ask`.
    let prior: Vec<Message> = rows
        .iter()
        .take_while(|row| Some(row.id) != question_id)
        .map(|row| Message::new(row.role, row.content.clone()))
        .collect();

    let (request_id, stream) = state.streams.open(conversation_id);
    if row.model.is_empty() {
        return fail(&app, &state, request_id, NO_MODEL.to_string());
    }
    let provider = match ready.registry.provider(&row.provider) {
        Ok(provider) => provider,
        Err(err) => return fail(&app, &state, request_id, err.to_string()),
    };
    let brain_dir = match notes::brain_dir(ready.config.brain.path.as_deref()) {
        Ok(dir) => dir,
        Err(err) => return fail(&app, &state, request_id, err.to_string()),
    };
    let brevity = row.brevity.unwrap_or(ready.config.style.brevity);
    let save_temperature = ready.config.brain.save_temperature;
    let provider_config = ready.config.providers.get(&row.provider).cloned();
    let task = tauri::async_runtime::spawn(run(
        app.clone(),
        request_id,
        Arc::clone(&stream),
        provider,
        provider_config,
        row.model,
        prior,
        ask,
        question_id,
        brevity,
        brain_dir,
        save_temperature,
    ));
    stream.attach(task);
    Ok(request_id)
}

#[tauri::command]
pub async fn cancel_message(
    app: AppHandle,
    state: State<'_, AppState>,
    request_id: u64,
) -> Result<(), String> {
    let ready = state.ready()?;
    // An entry that is gone belongs to a reply that already finished.
    let Some(stream) = state.streams.close(request_id) else {
        return Ok(());
    };
    stream.abort();
    settle(&app, &ready, request_id, &stream, None, true);
    Ok(())
}

#[tauri::command]
pub async fn status(state: State<'_, AppState>) -> Result<Status, String> {
    let brevity_default = state.ready()?.config.style.brevity;
    Ok(Status { brevity_default })
}

/// Every configured provider, whether it answers or not: a picker that hides
/// what is down explains nothing. Probed on each call.
#[tauri::command]
pub async fn providers_overview(state: State<'_, AppState>) -> Result<Vec<ProviderGroup>, String> {
    let configured: Vec<(String, ProviderConfig)> = {
        let ready = state.ready()?;
        ready
            .config
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect()
    };
    let groups = configured
        .into_iter()
        .map(|(name, provider)| group(name, provider));
    Ok(futures::future::join_all(groups).await)
}

async fn group(name: String, provider: ProviderConfig) -> ProviderGroup {
    let (reachable, models) = match &provider {
        ProviderConfig::OpenAiCompat {
            base_url,
            default_model,
            ..
        } => {
            let key = provider.api_key(&name).ok().flatten();
            served(base_url, key, default_model.as_deref()).await
        }
        ProviderConfig::Ollama {
            base_url,
            keep_alive,
        } => installed(base_url, keep_alive.clone()).await,
    };
    ProviderGroup {
        name,
        kind: provider.kind(),
        reachable,
        models,
    }
}

/// The listing doubles as the reachability answer. `default_model` always joins
/// it, so a conversation's model is never missing from its own menu.
pub(crate) async fn served(
    base_url: &str,
    api_key: Option<String>,
    default_model: Option<&str>,
) -> (bool, Vec<Model>) {
    let named = |mut names: Vec<String>| -> Vec<Model> {
        if let Some(default) = default_model {
            if !names.iter().any(|name| name == default) {
                names.push(default.to_string());
            }
        }
        // The menu's own order, not the endpoint's: free models lead it.
        openai_compat::order_models(&mut names);
        names
            .into_iter()
            .map(|name| Model {
                name,
                size_bytes: None,
                tools: None,
            })
            .collect()
    };
    let Ok(provider) = OpenAiCompatProvider::new(base_url, api_key, Vec::new()) else {
        return (false, named(Vec::new()));
    };
    match provider.list_models().await {
        Ok(models) => (true, named(models)),
        // The endpoint answered, just not with a listing: still reachable.
        Err(ChatError::Api { .. }) => (true, named(Vec::new())),
        Err(_) => (false, named(Vec::new())),
    }
}

/// The installed list doubles as the reachability answer; `ping` bounds the
/// wait on a dead endpoint.
pub(crate) async fn installed(base_url: &str, keep_alive: Option<String>) -> (bool, Vec<Model>) {
    if !ollama::ping(base_url).await {
        return (false, Vec::new());
    }
    let Ok(provider) = OllamaProvider::new(base_url, keep_alive) else {
        return (false, Vec::new());
    };
    let Ok(models) = provider.list_models().await else {
        return (false, Vec::new());
    };
    let models = models
        .into_iter()
        .map(|model| Model {
            tools: model.calls_tools(),
            name: model.name,
            size_bytes: Some(model.size_bytes),
        })
        .collect();
    (true, models)
}

/// The one failure every small-Ollama user hits: a tool-earning mention sent
/// at a model that cannot call tools. Known only when the daemon says so;
/// anything unknown lets the attempt proceed.
pub(crate) const NO_TOOLS: &str = "this model cannot call tools — the mention needs one that can";

pub(crate) async fn lacks_tools(config: &ProviderConfig, model: &str) -> bool {
    let ProviderConfig::Ollama {
        base_url,
        keep_alive,
    } = config
    else {
        return false;
    };
    let (reachable, models) = installed(base_url, keep_alive.clone()).await;
    if !reachable {
        return false;
    }
    models
        .into_iter()
        .find(|served| served.name == model)
        .and_then(|served| served.tools)
        == Some(false)
}

/// Owns one reply from the first token to the stored row.
#[allow(clippy::too_many_arguments)]
async fn run(
    app: AppHandle,
    request_id: u64,
    stream: Arc<Stream>,
    provider: Box<dyn ChatProvider>,
    provider_config: Option<ProviderConfig>,
    model: String,
    prior: Vec<Message>,
    ask: Ask,
    question_id: Option<i64>,
    brevity: Brevity,
    brain_dir: std::path::PathBuf,
    save_temperature: f32,
) {
    // Refused before recall runs: an embed plus a doomed request helps no one.
    let earns_tools = ask.writes() || ask.remind;
    if let Some(config) = provider_config.filter(|_| earns_tools) {
        if lacks_tools(&config, &model).await {
            app.state::<AppState>().streams.close(request_id);
            emit(&app, request_id, Body::error(NO_TOOLS));
            return;
        }
    }
    let context = build_context(&app, prior.clone(), ask.clone(), brevity).await;
    if let Some(context) = &context {
        record(&app, &stream, question_id, context);
        emit(&app, request_id, context_body(context));
    }
    let mut history = Vec::with_capacity(prior.len() + 2);
    // A brevity directive alone still has to reach the model.
    if let Some(context) = context.filter(|context| !context.system_message.is_empty()) {
        history.push(Message::new(Role::System, context.system_message));
    }
    history.extend(prior);
    history.push(Message::new(Role::User, ask.message));
    let tools = tools::offered(
        ask.memorize,
        ask.update,
        ask.delete,
        ask.link,
        ask.unlink,
        ask.remind,
    );

    let outcome = drive(
        &app,
        request_id,
        &stream,
        provider.as_ref(),
        &model,
        history,
        &tools,
        &brain_dir,
        save_temperature,
    )
    .await;
    let state = app.state::<AppState>();
    // A closed entry means a cancel already finished this reply.
    if state.streams.close(request_id).is_none() {
        return;
    }
    let Ok(ready) = state.ready() else {
        return;
    };
    match outcome {
        Outcome::Done(usage) => settle(&app, &ready, request_id, &stream, usage, false),
        Outcome::Interrupted => settle(&app, &ready, request_id, &stream, None, true),
        Outcome::Failed(message) => emit(&app, request_id, Body::error(message)),
    }
}

/// Lends the tool loop a reminder writer instead of the storage handle: one
/// lock per statement, and nothing held across the loop's awaits.
pub(crate) fn reminder_sink(
    app: &AppHandle,
) -> impl FnMut(&str, i64, Option<&str>) -> Result<i64, String> + Send + '_ {
    move |text, due_at, repeat| {
        let ready = app.state::<AppState>().inner().ready()?;
        let stored = ready.storage().add_reminder(text, due_at, repeat);
        stored
            .map(|reminder| reminder.id)
            .map_err(|err| err.to_string())
    }
}

pub(crate) fn context_body(context: &InjectedContext) -> Body {
    Body::Context {
        used: context
            .memories
            .iter()
            .map(|memory| memory.slug.clone())
            .collect(),
        tokens: context.tokens,
        soul: context.soul_tokens,
    }
}

/// Mirrors the brain folder into the index; blocking contexts only. One storage lock per
/// statement, NEVER across the embed — a held guard self-deadlocks and once froze the app.
pub(crate) fn sync_index(ready: &Ready) -> Result<(), String> {
    let config = &ready.config.brain;
    let wanted = config.model.canonical();
    let swapping = !ready
        .storage()
        .index_matches(&wanted)
        .map_err(|err| err.to_string())?;

    let dir = notes::brain_dir(config.path.as_deref()).map_err(|err| err.to_string())?;
    let notes = notes::read_notes(&dir).map_err(|err| err.to_string())?;
    // A swap invalidates every vector, so the old index says nothing useful
    // about staleness.
    let stale: Vec<String> = if swapping {
        notes.iter().map(|note| note.slug.clone()).collect()
    } else {
        let plan = ready
            .storage()
            .note_sync_plan(&notes)
            .map_err(|err| err.to_string())?;
        if !plan.changed {
            return Ok(());
        }
        plan.stale
    };

    let mut embedder = if swapping || !stale.is_empty() {
        Some(load_embedder(&ready.config, &config.model).map_err(|err| err.to_string())?)
    } else {
        None
    };
    if swapping {
        let dim = match config.model.known_dim() {
            Some(dim) => dim,
            None => embed::probe_dim(
                embedder
                    .as_deref_mut()
                    .expect("an embedder is loaded whenever a swap is in flight"),
            )
            .map_err(|err| err.to_string())?,
        };
        ready
            .storage()
            .rebuild_index(&wanted, dim)
            .map_err(|err| err.to_string())?;
    }

    // The embed runs between the locks, never under one.
    let embeddings = match embedder.as_deref_mut() {
        Some(embedder) => {
            brain::embed_notes(&notes, &stale, embedder).map_err(|err| err.to_string())?
        }
        None => Vec::new(),
    };
    ready
        .storage()
        .sync_notes(&notes, &embeddings)
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// Memory is opt-in and additive here too: no `/brain`, no injection. The
/// soul note rides every turn; a brain failure falls back to a soulless,
/// uninjected turn rather than a failed one.
pub(crate) async fn build_context(
    app: &AppHandle,
    prior: Vec<Message>,
    ask: Ask,
    brevity: Brevity,
) -> Option<InjectedContext> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let ready = handle.state::<AppState>().inner().ready().ok()?;
        // The folder is the truth: recall reads the files as they are now.
        if ask.any() {
            if let Err(err) = sync_index(&ready) {
                eprintln!("odyn: brain folder not synced: {err}");
            }
        }
        let context = if ask.any() {
            // One lock per statement — see `sync_index`.
            let storage = ready.storage();
            brain::build_context(
                Some(&storage),
                &ready.config.brain,
                &prior,
                &ask,
                brevity,
                || load_embedder(&ready.config, &ready.config.brain.model),
            )
        } else {
            brain::build_context(None, &ready.config.brain, &prior, &ask, brevity, || {
                Err(embed::EmbedError::Load(
                    "no trigger, no embedder".to_string(),
                ))
            })
        };
        Some(context.unwrap_or_else(|_| brain::empty_context(brevity, &ask)))
    })
    .await
    .ok()
    .flatten()
}

/// An injection record that cannot be written must not block the reply; the
/// ledger heals on the next turn.
fn record(app: &AppHandle, stream: &Stream, question_id: Option<i64>, context: &InjectedContext) {
    if context.is_empty() {
        return;
    }
    let state = app.state::<AppState>();
    let Ok(ready) = state.ready() else {
        return;
    };
    let _ = ready.storage().record_injections(
        Some(stream.conversation_id),
        question_id,
        &context.memory_ids(),
    );
}

/// The composer ledger's data source — the same `build_context` and history a
/// send would use, so the line previews exactly what it would inject.
#[tauri::command]
pub async fn context_preview(
    app: AppHandle,
    conversation_id: Option<i64>,
    draft: String,
) -> Result<ContextPreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let ask = brain::parse_ask(&draft);
        let (prior, chosen): (Vec<Message>, Option<Brevity>) = match conversation_id {
            Some(id) => {
                // One lock per statement to prevent self-deadlock.
                let brevity = conversation(&ready, id)?.brevity;
                let prior = ready
                    .storage()
                    .messages(id)
                    .map_err(say)?
                    .into_iter()
                    .map(|row| Message::new(row.role, row.content))
                    .collect();
                (prior, brevity)
            }
            None => (Vec::new(), None),
        };
        let brevity = chosen.unwrap_or(ready.config.style.brevity);
        let brain_config = ready.config.brain.clone();
        // A draft without triggers previews what it would send: the soul alone.
        if !ask.any() {
            let context = brain::build_context(None, &brain_config, &prior, &ask, brevity, || {
                Err(embed::EmbedError::Load(
                    "no trigger, no embedder".to_string(),
                ))
            })
            .map_err(|err| err.to_string())?;
            return Ok(preview(context, &brain_config, false));
        }
        sync_index(&ready)?;
        // One lock per statement — see `sync_index`.
        let storage = ready.storage();
        let context =
            brain::build_context(Some(&storage), &brain_config, &prior, &ask, brevity, || {
                load_embedder(&ready.config, &brain_config.model)
            })
            .map_err(|err| err.to_string())?;
        Ok(preview(context, &brain_config, true))
    })
    .await
    .map_err(|err| err.to_string())?
}

fn preview(context: InjectedContext, config: &BrainConfig, active: bool) -> ContextPreview {
    ContextPreview {
        active,
        memories: context
            .memories
            .iter()
            .map(|memory| LedgerItem {
                id: memory.slug.clone(),
                tokens: memory.tokens,
                content: memory.content.clone(),
            })
            .collect(),
        tokens: context.tokens,
        cap_tokens: config.cap_tokens,
        soul_tokens: context.soul_tokens,
        soul_over: context.soul_tokens > i64::from(config.soul_cap_tokens),
        system_message: context.system_message,
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    app: &AppHandle,
    request_id: u64,
    stream: &Stream,
    provider: &dyn ChatProvider,
    model: &str,
    history: Vec<Message>,
    tools: &[ToolDef],
    brain_dir: &std::path::Path,
    save_temperature: f32,
) -> Outcome {
    let mut sink = reminder_sink(app);
    let mut effects = tools::Effects {
        brain_dir,
        set_reminder: &mut sink,
    };
    let driven = tools::run_turn(
        provider,
        model,
        history,
        tools,
        &mut effects,
        save_temperature,
        |event| {
            match event {
                TurnEvent::Delta(delta) => {
                    stream.push(delta);
                    emit(
                        app,
                        request_id,
                        Body::Delta {
                            text: delta.to_string(),
                        },
                    );
                }
                TurnEvent::Saved(slug) => emit(
                    app,
                    request_id,
                    Body::Saved {
                        slug: slug.to_string(),
                    },
                ),
                TurnEvent::Updated(slug) => emit(
                    app,
                    request_id,
                    Body::Updated {
                        slug: slug.to_string(),
                    },
                ),
                TurnEvent::Deleted(slug) => emit(
                    app,
                    request_id,
                    Body::Deleted {
                        slug: slug.to_string(),
                    },
                ),
                TurnEvent::Linked { from, to } => emit(
                    app,
                    request_id,
                    Body::Linked {
                        from: from.to_string(),
                        to: to.to_string(),
                    },
                ),
                TurnEvent::Unlinked { from, to } => emit(
                    app,
                    request_id,
                    Body::Unlinked {
                        from: from.to_string(),
                        to: to.to_string(),
                    },
                ),
                TurnEvent::Reminded { text, due_at } => emit(
                    app,
                    request_id,
                    Body::Reminded {
                        text: text.to_string(),
                        due_at,
                    },
                ),
            }
            Ok(())
        },
    )
    .await;
    match driven {
        Ok(reply) => Outcome::Done(reply.usage),
        Err(TurnError::Chat(ChatError::Cancelled)) => Outcome::Interrupted,
        Err(TurnError::Chat(err)) => Outcome::Failed(format!("stream failed: {}", describe(&err))),
        Err(TurnError::Write(err)) => Outcome::Failed(err.to_string()),
    }
}

/// The one place a reply becomes a row: an interrupted answer is stored like a
/// finished one.
fn settle(
    app: &AppHandle,
    ready: &Ready,
    request_id: u64,
    stream: &Stream,
    usage: Option<Usage>,
    interrupted: bool,
) {
    let mut text = stream.text();
    if text.trim().is_empty() {
        emit(app, request_id, Body::Done { usage, interrupted });
        return;
    }
    if interrupted {
        text.push_str(INTERRUPTED);
    }
    let stored = ready.storage().append_message(
        stream.conversation_id,
        Role::Assistant,
        &text,
        usage.map(|usage| usage.input_tokens),
        usage.map(|usage| usage.output_tokens),
    );
    let body = match stored {
        Ok(_) => Body::Done { usage, interrupted },
        Err(err) => Body::error(err.to_string()),
    };
    emit(app, request_id, body);
}

/// A send that never reaches a provider still answers on the event channel.
fn fail(
    app: &AppHandle,
    state: &AppState,
    request_id: u64,
    message: String,
) -> Result<u64, String> {
    state.streams.close(request_id);
    emit(app, request_id, Body::error(message));
    Ok(request_id)
}

fn emit(app: &AppHandle, request_id: u64, body: Body) {
    let _ = app.emit(CHAT_EVENT, Event { request_id, body });
}

/// `ChatError`'s own prefixes would read as "stream failed: network error: …".
pub(crate) fn describe(err: &ChatError) -> String {
    match err {
        ChatError::Network(message) | ChatError::Parse(message) => message.clone(),
        ChatError::Api { status, message } => format!("provider returned {status}: {message}"),
        ChatError::Cancelled => "cancelled".to_string(),
    }
}

/// A conversation title has to fit one sidebar line, so: one line, 40 chars.
pub(crate) fn title_from(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_CHARS)
        .collect()
}

fn conversation(ready: &Ready, id: i64) -> Result<StoredConversation, String> {
    ready
        .storage()
        .list_conversations()
        .map_err(say)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or_else(|| StorageError::ConversationNotFound(id).to_string())
}

impl From<StoredConversation> for Conversation {
    fn from(row: StoredConversation) -> Self {
        Self {
            id: row.id,
            title: row.title,
            provider: row.provider,
            model: row.model,
            updated_at: row.updated_at,
            brevity: row.brevity,
        }
    }
}

fn say(err: StorageError) -> String {
    err.to_string()
}
