//! Brain view commands: the note pool, search, and edits — every write goes
//! through the folder, never straight into the index.

use odyn_core::embed::load_default_embedder;
use odyn_core::notes;
use odyn_core::storage::{MemorySort, MemoryStats, StorageError};
use tauri::{AppHandle, Manager};

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
    /// The brain folder, spelled out so the view can say where the files live.
    path: String,
    /// The embedding model's display name; fixed in v1.
    model: &'static str,
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

/// Opening the brain view re-reads the folder, so a file dropped in by any
/// editor or agent appears here without ceremony. A folder that cannot be
/// synced still shows what the index has.
#[tauri::command]
pub async fn brain_overview(app: AppHandle) -> Result<BrainOverview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        let dir =
            notes::brain_dir(ready.config.brain.path.as_deref()).map_err(|err| err.to_string())?;
        let count = ready.storage().count_memories().map_err(say)?;
        Ok(BrainOverview {
            count,
            top_k: ready.config.brain.top_k,
            cap_tokens: ready.config.brain.cap_tokens,
            path: dir.display().to_string(),
            model: "bge-small",
        })
    })
    .await
    .map_err(|err| err.to_string())?
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

/// The same embedding pipeline as chat recall and `odyn mem search`, so all
/// three return the same order for the same query.
#[tauri::command]
pub async fn brain_search(app: AppHandle, query: String) -> Result<Vec<MemoryRow>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        warn_on_sync_failure(&ready);
        if ready.storage().count_memories().map_err(say)? == 0 {
            return Ok(Vec::new());
        }
        // The embed happens without the storage lock: it can take seconds on
        // a cold model and a send must not queue behind it.
        let mut embedder = load_default_embedder().map_err(|err| err.to_string())?;
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

/// The cached graph, or a fresh compute — KNN sweeps and 300 layout
/// iterations are CPU work, so they run off the async workers.
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

/// Reading views stay additive: a folder that cannot be synced is reported
/// to the console, and the view renders what the index already has.
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
