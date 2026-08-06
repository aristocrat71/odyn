//! The frontend's whole surface. Every command is a wrapper: shaping for the
//! wire happens here, everything else happens in odyn-core.

use std::sync::Arc;

use futures::StreamExt;
use odyn_core::brain::{self, InjectedContext};
use odyn_core::brevity::Brevity;
use odyn_core::chat::{ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, Usage};
use odyn_core::config::{MemoryConfig, ProviderConfig};
use odyn_core::embed::load_default_embedder;
use odyn_core::providers::ollama::OllamaProvider;
use odyn_core::providers::{ollama, openai_compat};
use odyn_core::storage::{Conversation as StoredConversation, MemoryTier, StorageError};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
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
    /// `None` until a provider reports usage: an invented number would be worse
    /// than a missing one.
    tokens: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct MessageView {
    role: Role,
    content: String,
    /// Assistant rows only: the episodic ids injected for the question this
    /// answers — the `◈ used …` trace line.
    used: Vec<String>,
}

/// One ledger chip: a memory and the tokens it costs.
#[derive(serde::Serialize)]
pub struct LedgerItem {
    id: String,
    tokens: i64,
    content: String,
}

/// What the composer ledger renders — built by `brain::build_context`, the
/// same call the send path makes, which is the whole point.
#[derive(serde::Serialize)]
pub struct ContextPreview {
    core: Vec<LedgerItem>,
    episodic: Vec<LedgerItem>,
    core_tokens: i64,
    episodic_tokens: i64,
    /// core budget + episodic cap: the denominator on the right of the line.
    cap_tokens: u32,
    over_budget: bool,
    system_message: String,
}

/// One configured provider and what it can serve right now.
#[derive(serde::Serialize)]
pub struct ProviderGroup {
    name: String,
    kind: &'static str,
    reachable: bool,
    models: Vec<Model>,
}

#[derive(serde::Serialize)]
pub struct Model {
    name: String,
    /// On-disk size, which only Ollama reports. Config names no context length
    /// for API models, and an invented one would be worse than none.
    size_bytes: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct Status {
    provider_name: String,
    provider_reachable: bool,
    /// `None` when no local Ollama is configured beside the default provider,
    /// which is also the case when Ollama *is* the default: one dot, not two.
    ollama_reachable: Option<bool>,
    rss_bytes: u64,
    /// `[style] brevity` — what a conversation without its own choice uses.
    brevity_default: Brevity,
}

/// One shape for the whole stream: the frontend keys on `request_id` and
/// switches on `kind`.
#[derive(Clone, serde::Serialize)]
pub(crate) struct Event {
    pub(crate) request_id: u64,
    #[serde(flatten)]
    pub(crate) body: Body,
}

#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Body {
    /// What was injected for this reply, before its first delta: the episodic
    /// ids for the trace line, and the totals the spotlight's one-line ledger
    /// shows.
    Context {
        used: Vec<String>,
        core_tokens: i64,
        episodic_tokens: i64,
    },
    Delta {
        text: String,
    },
    Done {
        usage: Option<Usage>,
        interrupted: bool,
    },
    Error {
        message: String,
    },
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
    let provider = ready.registry.default_provider_name();
    // No default model is a real state for Ollama; the picker fills it in.
    let model = ready.config.default_model(provider).unwrap_or_default();
    let row = ready
        .storage()
        .create_conversation(NEW_TITLE, provider, model)
        .map_err(say)?;
    Ok(Conversation::from(row))
}

#[tauri::command]
pub async fn rename_conversation(
    state: State<'_, AppState>,
    id: i64,
    title: String,
) -> Result<(), String> {
    let ready = state.ready()?;
    ready.storage().rename_conversation(id, &title).map_err(say)
}

#[tauri::command]
pub async fn delete_conversation(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let ready = state.ready()?;
    ready.storage().delete_conversation(id).map_err(say)
}

/// An explicit level for this conversation, written immediately; it affects
/// the next send, never the past.
#[tauri::command]
pub async fn set_conversation_brevity(
    state: State<'_, AppState>,
    conversation_id: i64,
    brevity: Brevity,
) -> Result<(), String> {
    let ready = state.ready()?;
    ready
        .storage()
        .set_conversation_brevity(conversation_id, brevity)
        .map_err(say)
}

#[tauri::command]
pub async fn set_conversation_model(
    state: State<'_, AppState>,
    conversation_id: i64,
    provider: String,
    model: String,
) -> Result<(), String> {
    let ready = state.ready()?;
    ready
        .storage()
        .set_conversation_model(conversation_id, &provider, &model)
        .map_err(say)
}

#[tauri::command]
pub async fn get_conversation(
    state: State<'_, AppState>,
    id: i64,
) -> Result<ConversationView, String> {
    let ready = state.ready()?;
    let row = conversation(ready, id)?;
    let stored = ready.storage().messages(id).map_err(say)?;
    Ok(ConversationView {
        id: row.id,
        title: row.title,
        provider: row.provider,
        model: row.model,
        // A turn is a question and its answer, so the questions are what to count.
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
    let episodic: std::collections::HashMap<i64, String> = storage
        .list_memories(Some(MemoryTier::Episodic))
        .map_err(say)?
        .into_iter()
        .map(|memory| (memory.id, memory.display_id()))
        .collect();
    let mut by_question: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    for injection in storage.injections(conversation_id).map_err(say)? {
        let Some(message_id) = injection.message_id else {
            continue;
        };
        if let Some(id) = episodic.get(&injection.memory_id) {
            by_question.entry(message_id).or_default().push(id.clone());
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
                Role::System => Vec::new(),
            };
            MessageView {
                role: row.role,
                content: row.content,
                used,
            }
        })
        .collect())
}

/// Answers with the id the reply's events carry. `retry` re-runs a turn whose
/// question is already stored, so a failed stream is retried without asking it
/// twice.
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: i64,
    text: String,
    retry: bool,
) -> Result<u64, String> {
    let ready = state.ready()?;
    let row = conversation(ready, conversation_id)?;
    if !retry {
        let storage = ready.storage();
        storage
            .append_message(conversation_id, Role::User, &text, None, None)
            .map_err(say)?;
        if row.title == NEW_TITLE {
            storage
                .rename_conversation(conversation_id, &title_from(&text))
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
    // Everything before the question feeds retrieval; the question is `text`.
    let prior: Vec<Message> = rows
        .iter()
        .take_while(|row| Some(row.id) != question_id)
        .map(|row| Message::new(row.role, row.content.clone()))
        .collect();

    let (request_id, stream) = ready.streams.open(conversation_id);
    if row.model.is_empty() {
        return fail(&app, ready, request_id, NO_MODEL.to_string());
    }
    let provider = match ready.registry.provider(&row.provider) {
        Ok(provider) => provider,
        Err(err) => return fail(&app, ready, request_id, err.to_string()),
    };
    let brevity = row.brevity.unwrap_or(ready.config.style.brevity);
    let task = tauri::async_runtime::spawn(run(
        app.clone(),
        request_id,
        Arc::clone(&stream),
        provider,
        row.model,
        prior,
        text,
        question_id,
        brevity,
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
    let Some(stream) = ready.streams.close(request_id) else {
        return Ok(());
    };
    stream.abort();
    settle(&app, ready, request_id, &stream, None, true);
    Ok(())
}

#[tauri::command]
pub async fn status(state: State<'_, AppState>) -> Result<Status, String> {
    let (provider_name, default, ollama, brevity_default) = {
        let ready = state.ready()?;
        let brevity_default = ready.config.style.brevity;
        let name = ready.registry.default_provider_name().to_string();
        let default = ready
            .config
            .providers
            .get(&name)
            .ok_or_else(|| format!("no provider named `{name}` is configured"))?
            .clone();
        let ollama = ready
            .config
            .providers
            .iter()
            .find(|(other, provider)| {
                **other != name && matches!(provider, ProviderConfig::Ollama { .. })
            })
            .map(|(_, provider)| provider.clone());
        (name, default, ollama, brevity_default)
    };

    let provider_reachable = ping(&default).await;
    let ollama_reachable = match ollama {
        Some(provider) => Some(ping(&provider).await),
        None => None,
    };
    Ok(Status {
        provider_name,
        provider_reachable,
        ollama_reachable,
        rss_bytes: rss_bytes(),
        brevity_default,
    })
}

/// Every configured provider, in config order, whether it answers or not:
/// a picker that hides what is down explains nothing. Probed on each call, so
/// the reachability shown is the reachability now.
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
        } => (
            openai_compat::ping(base_url).await,
            default_model
                .iter()
                .map(|model| Model {
                    name: model.clone(),
                    size_bytes: None,
                })
                .collect(),
        ),
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

/// The installed list doubles as the reachability answer: the menu can only
/// offer what Ollama names. `ping` is what bounds the wait on a dead endpoint.
async fn installed(base_url: &str, keep_alive: Option<String>) -> (bool, Vec<Model>) {
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
            name: model.name,
            size_bytes: Some(model.size_bytes),
        })
        .collect();
    (true, models)
}

/// Owns one reply from the first token to the stored row.
#[allow(clippy::too_many_arguments)]
async fn run(
    app: AppHandle,
    request_id: u64,
    stream: Arc<Stream>,
    provider: Box<dyn ChatProvider>,
    model: String,
    prior: Vec<Message>,
    question: String,
    question_id: Option<i64>,
    brevity: Brevity,
) {
    let context = build_context(&app, prior.clone(), question.clone(), brevity).await;
    if let Some(context) = &context {
        record(&app, &stream, question_id, context);
        emit(&app, request_id, context_body(context));
    }
    let mut history = Vec::with_capacity(prior.len() + 2);
    // Checked on the message, not the memories: a brevity directive alone
    // still has to reach the model.
    if let Some(context) = context.filter(|context| !context.system_message.is_empty()) {
        history.push(Message::new(Role::System, context.system_message));
    }
    history.extend(prior);
    history.push(Message::new(Role::User, question));

    let outcome = drive(
        &app,
        request_id,
        &stream,
        provider.as_ref(),
        &model,
        &history,
    )
    .await;
    let state = app.state::<AppState>();
    let Ok(ready) = state.ready() else {
        return;
    };
    // A closed entry means a cancel already finished this reply.
    if ready.streams.close(request_id).is_none() {
        return;
    }
    match outcome {
        Outcome::Done(usage) => settle(&app, ready, request_id, &stream, usage, false),
        Outcome::Interrupted => settle(&app, ready, request_id, &stream, None, true),
        Outcome::Failed(message) => emit(&app, request_id, Body::Error { message }),
    }
}

pub(crate) fn context_body(context: &InjectedContext) -> Body {
    Body::Context {
        used: context
            .episodic
            .iter()
            .map(|memory| memory.display_id())
            .collect(),
        core_tokens: context.core_tokens,
        episodic_tokens: context.episodic_tokens,
    }
}

/// Memory is additive in the GUI too: a brain failure means an uninjected
/// turn, not a failed one — the ledger preview is where the error shows.
/// The embed is CPU work, so it runs off the async workers.
pub(crate) async fn build_context(
    app: &AppHandle,
    prior: Vec<Message>,
    question: String,
    brevity: Brevity,
) -> Option<InjectedContext> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let ready = handle.state::<AppState>().inner().ready().ok()?;
        brain::build_context(
            &ready.storage(),
            &ready.config.memory,
            &prior,
            &question,
            brevity,
            load_default_embedder,
        )
        .ok()
    })
    .await
    .ok()
    .flatten()
}

/// An injection record that cannot be written must not block the reply; the
/// ledger heals on the next successful turn.
fn record(app: &AppHandle, stream: &Stream, question_id: Option<i64>, context: &InjectedContext) {
    if context.is_empty() {
        return;
    }
    let state = app.state::<AppState>();
    let Ok(ready) = state.ready() else {
        return;
    };
    let _ = ready.storage().record_injections(
        stream.conversation_id,
        question_id,
        &context.memory_ids(),
    );
}

/// The composer ledger's data source — the same `build_context` the send path
/// uses, on the same history, so the line previews exactly what a send now
/// would inject.
#[tauri::command]
pub async fn context_preview(
    app: AppHandle,
    conversation_id: Option<i64>,
    draft: String,
) -> Result<ContextPreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let (prior, chosen): (Vec<Message>, Option<Brevity>) = match conversation_id {
            Some(id) => {
                // One lock per statement to prevent self-deadlock.
                let brevity = conversation(ready, id)?.brevity;
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
        let memory = &ready.config.memory;
        let context = brain::build_context(
            &ready.storage(),
            memory,
            &prior,
            &draft,
            chosen.unwrap_or(ready.config.style.brevity),
            load_default_embedder,
        )
        .map_err(|err| err.to_string())?;
        Ok(preview(context, memory))
    })
    .await
    .map_err(|err| err.to_string())?
}

fn preview(context: InjectedContext, memory: &MemoryConfig) -> ContextPreview {
    let chip = |memory: &odyn_core::storage::Memory| LedgerItem {
        id: memory.display_id(),
        tokens: memory.tokens,
        content: memory.content.clone(),
    };
    ContextPreview {
        core: context.core.iter().map(chip).collect(),
        episodic: context.episodic.iter().map(chip).collect(),
        core_tokens: context.core_tokens,
        episodic_tokens: context.episodic_tokens,
        cap_tokens: memory.core_budget_tokens + memory.episodic_cap_tokens,
        over_budget: context.core_over_budget,
        system_message: context.system_message,
    }
}

async fn drive(
    app: &AppHandle,
    request_id: u64,
    stream: &Stream,
    provider: &dyn ChatProvider,
    model: &str,
    history: &[Message],
) -> Outcome {
    let mut events = provider.chat_stream(ChatRequest::new(history, model));
    while let Some(event) = events.next().await {
        match event {
            Ok(ChatEvent::TextDelta(delta)) => {
                stream.push(&delta);
                emit(app, request_id, Body::Delta { text: delta });
            }
            Ok(ChatEvent::Done { usage }) => return Outcome::Done(usage),
            Err(ChatError::Cancelled) => return Outcome::Interrupted,
            Err(err) => return Outcome::Failed(format!("stream failed: {}", describe(&err))),
        }
    }
    Outcome::Done(None)
}

/// The one place a reply becomes a row: the end of a stream and a cancel both
/// land here, so an interrupted answer is stored like a finished one.
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
        // Nothing was said, so there is nothing to keep.
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
        Err(err) => Body::Error {
            message: err.to_string(),
        },
    };
    emit(app, request_id, body);
}

/// A send that never reaches a provider still answers on the event channel, so
/// the frontend renders every failure in the same place.
fn fail(app: &AppHandle, ready: &Ready, request_id: u64, message: String) -> Result<u64, String> {
    ready.streams.close(request_id);
    emit(app, request_id, Body::Error { message });
    Ok(request_id)
}

/// A window that has gone away cannot be told anything, and there is nothing to
/// be done about it here.
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

async fn ping(provider: &ProviderConfig) -> bool {
    match provider {
        ProviderConfig::OpenAiCompat { base_url, .. } => openai_compat::ping(base_url).await,
        ProviderConfig::Ollama { base_url, .. } => ollama::ping(base_url).await,
    }
}

/// The RAM number in the footer is a brag, so it is measured, not estimated:
/// this process only, refreshed on demand.
fn rss_bytes() -> u64 {
    let Ok(pid) = sysinfo::get_current_pid() else {
        return 0;
    };
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, |process| process.memory())
}

impl From<StoredConversation> for Conversation {
    fn from(row: StoredConversation) -> Self {
        Self {
            id: row.id,
            title: row.title,
            provider: row.provider,
            model: row.model,
            brevity: row.brevity,
        }
    }
}

fn say(err: StorageError) -> String {
    err.to_string()
}
