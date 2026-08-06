//! `odyn mem`: the brain's front door on the command line.

use std::io::Write;

use odyn_core::embed::load_default_embedder;
use odyn_core::storage::{normalize_content, Memory, MemoryTier, Storage};

use crate::session::{write_failure, Failure};

/// Semantic search shows more than the injection top-k: browsing is not
/// injecting.
const SEARCH_LIMIT: usize = 20;

#[derive(clap::Subcommand)]
pub enum Action {
    /// remember something; episodic unless --core
    Add {
        content: String,
        /// store as an always-injected core memory
        #[arg(long)]
        core: bool,
    },
    /// list memories, oldest first
    List {
        /// show only one tier
        #[arg(long, value_enum)]
        tier: Option<Tier>,
    },
    /// find the episodic memories closest in meaning to the query
    Search { query: String },
    /// delete a memory by id (e-0142, c-01, or a plain number)
    Rm { id: String },
    /// replace a memory's content by id; episodic memories are re-embedded
    Edit { id: String, content: String },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Tier {
    Core,
    Episodic,
}

impl From<Tier> for MemoryTier {
    fn from(tier: Tier) -> Self {
        match tier {
            Tier::Core => MemoryTier::Core,
            Tier::Episodic => MemoryTier::Episodic,
        }
    }
}

pub fn run(action: Action) -> Result<(), Failure> {
    let storage = Storage::open_default()
        .map_err(|err| Failure::run(format!("could not open the database: {err}")))?;
    let mut out = anstream::stdout().lock();
    match action {
        Action::Add { content, core } => {
            let memory = if core {
                storage.add_memory(MemoryTier::Core, &content, None)
            } else {
                let content = normalize_content(&content);
                let embedding = embed_one(&content)?;
                storage.add_memory(MemoryTier::Episodic, &content, Some(&embedding))
            }
            .map_err(|err| Failure::run(format!("could not add the memory: {err}")))?;
            writeln!(out, "{}", line(&memory)).map_err(write_failure)
        }
        Action::List { tier } => {
            let memories = storage
                .list_memories(tier.map(MemoryTier::from))
                .map_err(|err| Failure::run(format!("could not list memories: {err}")))?;
            for memory in memories {
                writeln!(out, "{}", line(&memory)).map_err(write_failure)?;
            }
            Ok(())
        }
        Action::Search { query } => {
            let count = storage
                .count_memories(Some(MemoryTier::Episodic))
                .map_err(|err| Failure::run(format!("could not search memories: {err}")))?;
            if count == 0 {
                return Ok(());
            }
            let embedding = embed_one(&query)?;
            let neighbors = storage
                .knn_episodic(&embedding, SEARCH_LIMIT)
                .map_err(|err| Failure::run(format!("could not search memories: {err}")))?;
            for (memory, _) in neighbors {
                writeln!(out, "{}", line(&memory)).map_err(write_failure)?;
            }
            Ok(())
        }
        Action::Rm { id } => {
            let id = parse_id(&id)?;
            storage
                .delete_memory(id)
                .map_err(|err| Failure::run(format!("could not delete the memory: {err}")))?;
            writeln!(out, "deleted {id}").map_err(write_failure)
        }
        Action::Edit { id, content } => {
            let id = parse_id(&id)?;
            let current = storage
                .memory(id)
                .map_err(|err| Failure::run(format!("could not edit the memory: {err}")))?;
            let embedding = match current.tier {
                MemoryTier::Core => None,
                MemoryTier::Episodic => Some(embed_one(&normalize_content(&content))?),
            };
            let memory = storage
                .update_memory(id, &content, embedding.as_deref())
                .map_err(|err| Failure::run(format!("could not edit the memory: {err}")))?;
            writeln!(out, "{}", line(&memory)).map_err(write_failure)
        }
    }
}

/// Loads the model, embeds one text, drops the model.
fn embed_one(text: &str) -> Result<Vec<f32>, Failure> {
    let mut embedder = load_default_embedder()
        .map_err(|err| Failure::run(format!("could not load the embedding model: {err}")))?;
    embedder
        .embed(&[text])
        .map_err(|err| Failure::run(format!("could not embed the content: {err}")))?
        .pop()
        .ok_or_else(|| Failure::run("the embedder returned no vector"))
}

/// The prefix is cosmetic — ids are unique across tiers — so `e-0142`,
/// `c-01` and `142` all name the same row space.
fn parse_id(id: &str) -> Result<i64, Failure> {
    let digits = id.trim().trim_start_matches("c-").trim_start_matches("e-");
    digits
        .parse::<i64>()
        .map_err(|_| Failure::config(format!("not a memory id: {id}")))
}

fn line(memory: &Memory) -> String {
    format!(
        "{}  {} tk  {}",
        memory.display_id(),
        memory.tokens,
        memory.content
    )
}
