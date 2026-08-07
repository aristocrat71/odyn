//! Brain view commands: the note pool, search, and edits — every write goes
//! through the folder, never straight into the index.

use odyn_core::config::{config_path, ProviderConfig};
use odyn_core::config_edit;
use odyn_core::embed::{self, load_embedder, EmbedOption};
use odyn_core::notes;
use odyn_core::providers::ollama;
use odyn_core::storage::{MemorySort, MemoryStats, StorageError};
use tauri::{AppHandle, Manager, State};

use crate::commands::sync_index;
use crate::state::{AppState, Ready};

const PAGE: usize = 50;
/// Same breadth as `odyn mem search`, whose order the GUI must reproduce.
const SEARCH_LIMIT: usize = 20;

#[derive(serde::Serialize)]
pub struct MemoryRow {
    id: i64,
    slug: String,
    content: String,
    tokens: i64,
    hits: i64,
    last_injected_at: Option<i64>,
    created_at: i64,
}

#[derive(serde::Serialize)]
pub struct BrainOverview {
    count: i64,
    top_k: u32,
    cap_tokens: u32,
    path: String,
    model: String,
    /// Whether that model sends note text off the machine.
    model_remote: bool,
    /// The width the index was actually built at; 0 before anything is built.
    dim: usize,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    Recent,
    Hits,
    Created,
}

impl From<Sort> for MemorySort {
    fn from(sort: Sort) -> Self {
        match sort {
            Sort::Recent => MemorySort::Recent,
            Sort::Hits => MemorySort::Hits,
            Sort::Created => MemorySort::Created,
        }
    }
}

impl From<MemoryStats> for MemoryRow {
    fn from(stats: MemoryStats) -> Self {
        Self {
            id: stats.memory.id,
            slug: stats.memory.slug,
            tokens: stats.memory.tokens,
            created_at: stats.memory.created_at,
            content: stats.memory.content,
            hits: stats.hits,
            last_injected_at: stats.last_injected_at,
        }
    }
}

/// Re-reads the folder, so a file dropped in by any editor appears here. A
/// folder that cannot be synced still shows what the index has.
#[tauri::command]
pub async fn brain_overview(app: AppHandle) -> Result<BrainOverview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        let dir =
            notes::brain_dir(ready.config.brain.path.as_deref()).map_err(|err| err.to_string())?;
        let count = ready.storage().count_memories().map_err(say)?;
        let dim = ready.storage().index_dim().map_err(say)?;
        Ok(BrainOverview {
            count,
            top_k: ready.config.brain.top_k,
            cap_tokens: ready.config.brain.cap_tokens,
            path: dir.display().to_string(),
            model: ready.config.brain.model.canonical(),
            model_remote: ready.config.brain.model.is_remote(),
            dim,
        })
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Everything selectable; remote entries are flagged — only those send note
/// text off-machine.
#[tauri::command]
pub async fn embed_catalog(state: State<'_, AppState>) -> Result<Vec<EmbedOption>, String> {
    let (ollama_url, providers) = {
        let ready = state.ready()?;
        let ollama = ready
            .config
            .providers
            .values()
            .find_map(|provider| match provider {
                ProviderConfig::Ollama { base_url, .. } => Some(base_url.clone()),
                ProviderConfig::OpenAiCompat { .. } => None,
            })
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        let configured: Vec<(String, ProviderConfig)> = ready
            .config
            .providers
            .iter()
            .filter(|(_, provider)| matches!(provider, ProviderConfig::OpenAiCompat { .. }))
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect();
        (ollama, configured)
    };

    let mut options = embed::builtin_catalog();
    for model in ollama::embedding_models(&ollama_url).await {
        let size = model.size_bytes / 1_048_576;
        options.push(EmbedOption {
            id: format!("ollama:{}", model.name),
            backend: "ollama",
            // Only a probe can answer, and that means loading the model.
            dim: None,
            description: format!("local, via ollama · {size} MB"),
            remote: false,
        });
    }
    for (name, provider) in providers {
        let ProviderConfig::OpenAiCompat {
            base_url,
            default_model,
            ..
        } = &provider
        else {
            continue;
        };
        let key = provider.api_key(&name).ok().flatten();
        let (_, models) = crate::commands::served(base_url, key, default_model.as_deref()).await;
        for model in models {
            options.push(EmbedOption {
                id: format!("{name}:{}", model.name),
                backend: "provider",
                dim: None,
                description: format!("sends note text to {name}"),
                remote: true,
            });
        }
    }
    Ok(options)
}

/// Writes `[brain] model` and re-indexes on the spot, so the answer reflects a
/// re-embedded brain. Long by nature: it may download a model.
#[tauri::command]
pub async fn brain_set_model(app: AppHandle, model: String) -> Result<BrainOverview, String> {
    let path = config_path().map_err(|err| err.to_string())?;
    config_edit::set(&path, "brain.model", model.trim()).map_err(|err| err.to_string())?;
    app.state::<AppState>().inner().reload()?;
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let ready = handle.state::<AppState>().inner().ready()?;
        sync_index(&ready)
    })
    .await
    .map_err(|err| err.to_string())??;
    brain_overview(app).await
}

#[tauri::command]
pub async fn brain_memories(
    app: AppHandle,
    sort: Sort,
    offset: u32,
) -> Result<Vec<MemoryRow>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        let page = ready
            .storage()
            .memories_overview(sort.into(), PAGE, offset as usize)
            .map_err(say)?;
        Ok(page.into_iter().map(MemoryRow::from).collect())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// The same pipeline as chat recall and `odyn mem search`: same query, same order.
#[tauri::command]
pub async fn brain_search(app: AppHandle, query: String) -> Result<Vec<MemoryRow>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        if ready.storage().count_memories().map_err(say)? == 0 {
            return Ok(Vec::new());
        }
        // The embed runs without the storage lock: a cold model takes seconds
        // and a send must not queue behind it.
        let mut embedder = load_embedder(&ready.config, &ready.config.brain.model)
            .map_err(|err| err.to_string())?;
        let embedding = embedder
            .embed(&[query.as_str()])
            .map_err(|err| err.to_string())?
            .pop()
            .ok_or_else(|| "the embedder returned no vector".to_string())?;
        let storage = ready.storage();
        let found = storage
            .knn(&embedding, SEARCH_LIMIT)
            .map_err(say)?
            .into_iter()
            .map(|(memory, _)| memory)
            .collect();
        let stats = storage.stats_for(found).map_err(say)?;
        Ok(stats.into_iter().map(MemoryRow::from).collect())
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn brain_add_note(app: AppHandle, content: String) -> Result<MemoryRow, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let dir =
            notes::brain_dir(ready.config.brain.path.as_deref()).map_err(|err| err.to_string())?;
        let slug = notes::write_note(&dir, None, &content).map_err(|err| err.to_string())?;
        sync_index(&ready)?;
        row_by_slug(&ready, &slug)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn brain_update_note(
    app: AppHandle,
    slug: String,
    content: String,
) -> Result<MemoryRow, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let dir =
            notes::brain_dir(ready.config.brain.path.as_deref()).map_err(|err| err.to_string())?;
        notes::update_note(&dir, &slug, &content).map_err(|err| err.to_string())?;
        sync_index(&ready)?;
        row_by_slug(&ready, &slug)
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn brain_delete_note(app: AppHandle, slug: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let dir =
            notes::brain_dir(ready.config.brain.path.as_deref()).map_err(|err| err.to_string())?;
        notes::delete_note(&dir, &slug).map_err(|err| err.to_string())?;
        sync_index(&ready)
    })
    .await
    .map_err(|err| err.to_string())?
}

/// KNN sweeps and 300 layout iterations are CPU work, so they run off-thread.
#[tauri::command]
pub async fn brain_graph(app: AppHandle) -> Result<odyn_core::graph::Graph, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        let storage = ready.storage();
        odyn_core::graph::brain_graph(&storage, ready.config.brain.similarity_edge_threshold)
            .map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

/// Reading views stay additive: an unsyncable folder still renders the index.
fn warn_on_sync_failure(ready: &Ready) {
    if let Err(err) = sync_index(ready) {
        eprintln!("odyn: brain folder not synced: {err}");
    }
}

fn row_by_slug(ready: &Ready, slug: &str) -> Result<MemoryRow, String> {
    let storage = ready.storage();
    let memory = storage
        .list_memories()
        .map_err(say)?
        .into_iter()
        .find(|memory| memory.slug == slug)
        .ok_or_else(|| format!("note `{slug}` did not survive the sync"))?;
    let stats = storage.stats_for(vec![memory]).map_err(say)?;
    stats
        .into_iter()
        .next()
        .map(MemoryRow::from)
        .ok_or_else(|| "the memory vanished while being read back".to_string())
}

fn say(err: StorageError) -> String {
    err.to_string()
}
