//! The frontend's whole surface. Every command is a wrapper: shaping for the
//! wire happens here, everything else happens in odyn-core.

use odyn_core::brain::{self, Ask, InjectedContext};
use odyn_core::brevity::Brevity;
use odyn_core::chat::{ChatError, Message, Usage};
use odyn_core::config::ProviderConfig;
use odyn_core::embed::{self, load_embedder};
use odyn_core::notes;
use odyn_core::providers::ollama::OllamaProvider;
use odyn_core::providers::openai_compat::OpenAiCompatProvider;
use odyn_core::providers::{ollama, openai_compat};
use tauri::{AppHandle, Manager};

use crate::state::{AppState, Ready};

#[derive(serde::Serialize)]
pub struct Model {
    pub(crate) name: String,
    /// On-disk size; only Ollama reports it, and it is never invented.
    size_bytes: Option<u64>,
    /// Whether the model can call tools; `None` when nothing reported it.
    tools: Option<bool>,
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

pub(crate) fn describe(err: &ChatError) -> String {
    match err {
        ChatError::Network(message) | ChatError::Parse(message) => message.clone(),
        ChatError::Api { status, message } => format!("provider returned {status}: {message}"),
        ChatError::Cancelled => "cancelled".to_string(),
    }
}
