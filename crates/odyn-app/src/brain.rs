//! Brain view commands: list-mode data and the core column's inline edits.

use odyn_core::embed::load_default_embedder;
use odyn_core::storage::{EpisodicSort, MemoryStats, MemoryTier, StorageError};
use tauri::{AppHandle, Manager, State};

use crate::state::AppState;

const PAGE: usize = 50;
/// Same breadth as `odyn mem search`, whose order the GUI must reproduce.
const SEARCH_LIMIT: usize = 20;

#[derive(serde::Serialize)]
pub struct MemoryRow {
    id: i64,
    display_id: String,
    content: String,
    tokens: i64,
    hits: i64,
    last_injected_at: Option<i64>,
    created_at: i64,
}

#[derive(serde::Serialize)]
pub struct BrainOverview {
    episodic_count: i64,
    top_k: u32,
    cap_tokens: u32,
    core_budget_tokens: u32,
    core_tokens: i64,
    core: Vec<MemoryRow>,
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

impl From<Sort> for EpisodicSort {
    fn from(sort: Sort) -> Self {
        match sort {
            Sort::Recent => EpisodicSort::Recent,
            Sort::Hits => EpisodicSort::Hits,
            Sort::Created => EpisodicSort::Created,
        }
    }
}

impl From<MemoryStats> for MemoryRow {
    fn from(stats: MemoryStats) -> Self {
        Self {
            id: stats.memory.id,
            display_id: stats.memory.display_id(),
            tokens: stats.memory.tokens,
            created_at: stats.memory.created_at,
            content: stats.memory.content,
            hits: stats.hits,
            last_injected_at: stats.last_injected_at,
        }
    }
}

#[tauri::command]
pub fn brain_overview(state: State<'_, AppState>) -> Result<BrainOverview, String> {
    let ready = state.ready()?;
    let storage = ready.storage();
    let core = storage.list_memories(Some(MemoryTier::Core)).map_err(say)?;
    let core_tokens = core.iter().map(|memory| memory.tokens).sum();
    Ok(BrainOverview {
        episodic_count: storage
            .count_memories(Some(MemoryTier::Episodic))
            .map_err(say)?,
        top_k: ready.config.memory.episodic_top_k,
        cap_tokens: ready.config.memory.episodic_cap_tokens,
        core_budget_tokens: ready.config.memory.core_budget_tokens,
        core_tokens,
        core: rows(storage.stats_for(core).map_err(say)?),
        model: "bge-small",
    })
}

#[tauri::command]
pub fn brain_episodic(
    state: State<'_, AppState>,
    sort: Sort,
    offset: u32,
) -> Result<Vec<MemoryRow>, String> {
    let ready = state.ready()?;
    let page = ready
        .storage()
        .episodic_overview(sort.into(), PAGE, offset as usize)
        .map_err(say)?;
    Ok(rows(page))
}

/// The same embedding pipeline as chat retrieval and `odyn mem search`, so
/// all three return the same order for the same query.
#[tauri::command]
pub async fn brain_search(app: AppHandle, query: String) -> Result<Vec<MemoryRow>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        if ready
            .storage()
            .count_memories(Some(MemoryTier::Episodic))
            .map_err(say)?
            == 0
        {
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
            .knn_episodic(&embedding, SEARCH_LIMIT)
            .map_err(say)?
            .into_iter()
            .map(|(memory, _)| memory)
            .collect();
        Ok(rows(storage.stats_for(found).map_err(say)?))
    })
    .await
    .map_err(|err| err.to_string())?
}

#[tauri::command]
pub fn brain_add_core(state: State<'_, AppState>, content: String) -> Result<MemoryRow, String> {
    let ready = state.ready()?;
    let storage = ready.storage();
    let memory = storage
        .add_memory(MemoryTier::Core, &content, None)
        .map_err(say)?;
    single(storage.stats_for(vec![memory]).map_err(say)?)
}

#[tauri::command]
pub fn brain_update_core(
    state: State<'_, AppState>,
    id: i64,
    content: String,
) -> Result<MemoryRow, String> {
    let ready = state.ready()?;
    let storage = ready.storage();
    let memory = storage.update_memory(id, &content, None).map_err(say)?;
    single(storage.stats_for(vec![memory]).map_err(say)?)
}

#[tauri::command]
pub fn brain_delete_memory(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    state.ready()?.storage().delete_memory(id).map_err(say)
}

/// The cached graph, or a fresh compute — KNN sweeps and 300 layout
/// iterations are CPU work, so they run off the async workers.
#[tauri::command]
pub async fn brain_graph(app: AppHandle) -> Result<odyn_core::graph::Graph, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let ready = app.state::<AppState>().inner().ready()?;
        let storage = ready.storage();
        odyn_core::graph::brain_graph(&storage, ready.config.memory.similarity_edge_threshold)
            .map_err(|err| err.to_string())
    })
    .await
    .map_err(|err| err.to_string())?
}

fn rows(stats: Vec<MemoryStats>) -> Vec<MemoryRow> {
    stats.into_iter().map(MemoryRow::from).collect()
}

fn single(stats: Vec<MemoryStats>) -> Result<MemoryRow, String> {
    stats
        .into_iter()
        .next()
        .map(MemoryRow::from)
        .ok_or_else(|| "the memory vanished while being read back".to_string())
}

fn say(err: StorageError) -> String {
    err.to_string()
}
