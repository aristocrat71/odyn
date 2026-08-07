//! `odyn mem`: the brain folder's front door on the command line.
//!
//! Memories are markdown files; these commands write and remove them, then
//! mirror the folder into the retrieval index. Anything they can do, a plain
//! text editor in the brain folder does too.

use std::io::Write;

use odyn_core::brain;
use odyn_core::config::Config;
use odyn_core::embed::load_default_embedder;
use odyn_core::notes;
use odyn_core::storage::{Memory, Storage};

use crate::session::{config_failure, write_failure, Failure};

/// Semantic search shows more than one recall injects: browsing is not
/// injecting.
const SEARCH_LIMIT: usize = 20;

#[derive(clap::Subcommand)]
pub enum Action {
    /// remember something as a new note in the brain folder
    Add {
        content: String,
        /// the note's name; derived from the content when omitted
        #[arg(long)]
        name: Option<String>,
    },
    /// list notes, oldest first
    List,
    /// find the notes closest in meaning to the query
    Search { query: String },
    /// delete a note by name
    Rm { name: String },
    /// replace a note's content by name; it is re-embedded on sync
    Edit { name: String, content: String },
    /// print the brain folder's path
    Path,
}

pub fn run(action: Action) -> Result<(), Failure> {
    let config = Config::load().map_err(config_failure)?;
    let dir = notes::brain_dir(config.brain.path.as_deref())
        .map_err(|err| Failure::run(err.to_string()))?;
    if let Action::Path = action {
        return writeln!(anstream::stdout(), "{}", dir.display()).map_err(write_failure);
    }
    let storage = Storage::open_default()
        .map_err(|err| Failure::run(format!("could not open the database: {err}")))?;
    let mut out = anstream::stdout().lock();
    match action {
        Action::Path => unreachable!("answered before the database opened"),
        Action::Add { content, name } => {
            let slug = notes::write_note(&dir, name.as_deref(), &content)
                .map_err(|err| Failure::run(format!("could not add the note: {err}")))?;
            sync(&storage, &config)?;
            writeln!(out, "{}", line(&find(&storage, &slug)?)).map_err(write_failure)
        }
        Action::List => {
            sync(&storage, &config)?;
            let memories = storage
                .list_memories()
                .map_err(|err| Failure::run(format!("could not list memories: {err}")))?;
            for memory in memories {
                writeln!(out, "{}", line(&memory)).map_err(write_failure)?;
            }
            Ok(())
        }
        Action::Search { query } => {
            sync(&storage, &config)?;
            let count = storage
                .count_memories()
                .map_err(|err| Failure::run(format!("could not search memories: {err}")))?;
            if count == 0 {
                return Ok(());
            }
            let embedding = embed_one(&query)?;
            let neighbors = storage
                .knn(&embedding, SEARCH_LIMIT)
                .map_err(|err| Failure::run(format!("could not search memories: {err}")))?;
            for (memory, _) in neighbors {
                writeln!(out, "{}", line(&memory)).map_err(write_failure)?;
            }
            Ok(())
        }
        Action::Rm { name } => {
            notes::delete_note(&dir, &name)
                .map_err(|err| Failure::run(format!("could not delete the note: {err}")))?;
            sync(&storage, &config)?;
            writeln!(out, "deleted {name}").map_err(write_failure)
        }
        Action::Edit { name, content } => {
            notes::update_note(&dir, &name, &content)
                .map_err(|err| Failure::run(format!("could not edit the note: {err}")))?;
            sync(&storage, &config)?;
            writeln!(out, "{}", line(&find(&storage, &name)?)).map_err(write_failure)
        }
    }
}

/// Mirrors the folder into the index; the model loads only when a note is
/// new or edited.
fn sync(storage: &Storage, config: &Config) -> Result<(), Failure> {
    brain::sync(storage, &config.brain, load_default_embedder)
        .map_err(|err| Failure::run(format!("could not sync the brain folder: {err}")))?;
    Ok(())
}

fn find(storage: &Storage, slug: &str) -> Result<Memory, Failure> {
    storage
        .list_memories()
        .map_err(|err| Failure::run(format!("could not read the memory back: {err}")))?
        .into_iter()
        .find(|memory| memory.slug == slug)
        .ok_or_else(|| Failure::run(format!("note `{slug}` did not survive the sync")))
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

/// One row per note: the slug, the cost, the first line of the content.
fn line(memory: &Memory) -> String {
    let mut first = memory
        .content
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    if memory.content.lines().count() > 1 {
        first.push_str(" …");
    }
    format!("{}  {} tk  {}", memory.slug, memory.tokens, first)
}
