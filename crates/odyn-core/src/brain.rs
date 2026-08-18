//! The one gate every injected memory token passes through.
//!
//! Nothing is injected unless the user asks for it with `/brain`. Recall is a
//! personalized PageRank over the brain graph seeded by the question's
//! embedding. `build_context` is the only producer of memory context in Odyn,
//! which is what keeps the ledger equal to reality.

use std::collections::HashMap;

use crate::brevity::Brevity;
use crate::chat::{Message, Role};
use crate::config::BrainConfig;
use crate::embed::{self, EmbedError, Embedder};
use crate::graph::{self, GraphError};
use crate::notes::{self, NotesError};
use crate::storage::{Memory, Storage, StorageError};

/// The mention that turns recall on for one message.
pub const TRIGGER: &str = "/brain";
/// The mention that asks the model to save a memory this turn.
pub const MEMORIZE: &str = "/memory";
/// The mention that asks the model to rewrite a memory this turn.
pub const UPDATE: &str = "/update-memory";
/// The mention that asks the model to trash a memory this turn.
pub const DELETE: &str = "/delete-memory";
/// The mention that asks the model to connect two memories this turn.
pub const LINK: &str = "/link-memory";
/// The mention that asks the model to disconnect two memories this turn.
pub const UNLINK: &str = "/unlink-memory";
/// The mention that asks the model to set a reminder this turn. Alone among
/// the triggers it reads nothing from the brain.
pub const REMIND: &str = "/reminder";
/// The mention that asks the model to schedule a recurring ask. Like
/// `/reminder`, it reads nothing from the brain.
pub const SCHEDULE: &str = "/schedule";

/// Two turns of history join the retrieval query.
const QUERY_MESSAGES: usize = 4;
/// Random-walk-with-restart: how much mass follows edges each step.
const DAMPING: f64 = 0.85;
const WALK_ITERATIONS: usize = 30;
/// Share of the final rank from question similarity; the rest is walk mass.
const SIMILARITY_SHARE: f64 = 0.6;
/// Names in the write-turn index before it is summarized; a folder larger than
/// this would spend the whole recall cap on slugs.
const NAMES_SHOWN: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Notes(#[from] NotesError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

/// A user message with its trigger mentions, if any, understood and removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// The message without the trigger tokens. A message that was only
    /// triggers keeps them — an empty user message would be worse.
    pub message: String,
    /// What retrieval embeds; a bare `/brain` leaves it empty and recalls on
    /// the conversation history alone.
    pub query: String,
    pub recall: bool,
    /// Whether the model is handed `save_memory` this turn.
    pub memorize: bool,
    /// Whether the model is handed `update_memory` this turn.
    pub update: bool,
    /// Whether the model is handed `delete_memory` this turn.
    pub delete: bool,
    /// Whether the model is handed `link_memory` this turn.
    pub link: bool,
    /// Whether the model is handed `unlink_memory` this turn.
    pub unlink: bool,
    /// Whether the model is handed `set_reminder` this turn.
    pub remind: bool,
    /// Whether the model is handed `schedule_ask` this turn.
    pub schedule: bool,
}

impl Ask {
    /// Whether the turn reads the brain. `/reminder` is deliberately absent:
    /// a reminder is not written from memory, so it must not load the embedder.
    pub fn any(&self) -> bool {
        self.recall || self.writes()
    }

    /// Whether the turn is handed a tool that writes to the folder. Those
    /// turns recall wider and see every memory's name, because their task is
    /// picking the right note rather than answering from the best few.
    pub fn writes(&self) -> bool {
        self.memorize || self.update || self.delete || self.link || self.unlink
    }
}

#[derive(Default)]
struct Mentions {
    recall: bool,
    memorize: bool,
    update: bool,
    delete: bool,
    link: bool,
    unlink: bool,
    remind: bool,
    schedule: bool,
}

impl Mentions {
    fn any(&self) -> bool {
        self.recall
            || self.memorize
            || self.update
            || self.delete
            || self.link
            || self.unlink
            || self.remind
            || self.schedule
    }
}

/// Finds a whitespace-delimited `/brain`, `/memory`, `/update-memory`,
/// `/delete-memory`, `/link-memory`, `/unlink-memory`, `/reminder` or
/// `/schedule` anywhere in the message, case insensitively, tolerating trailing
/// punctuation. Each token and the whitespace after it are removed; everything
/// else stays byte-for-byte.
pub fn parse_ask(text: &str) -> Ask {
    let mut cleaned = String::with_capacity(text.len());
    let mut token = String::new();
    let mut found = Mentions::default();
    let mut swallow_gap = false;
    let flush = |token: &mut String, cleaned: &mut String, found: &mut Mentions| {
        if token.is_empty() {
            return false;
        }
        let trailer = token.trim_end_matches([',', '.', ';', ':', '!', '?']);
        let flag = if trailer.eq_ignore_ascii_case(TRIGGER) {
            Some(&mut found.recall)
        } else if trailer.eq_ignore_ascii_case(MEMORIZE) {
            Some(&mut found.memorize)
        } else if trailer.eq_ignore_ascii_case(UPDATE) {
            Some(&mut found.update)
        } else if trailer.eq_ignore_ascii_case(DELETE) {
            Some(&mut found.delete)
        } else if trailer.eq_ignore_ascii_case(LINK) {
            Some(&mut found.link)
        } else if trailer.eq_ignore_ascii_case(UNLINK) {
            Some(&mut found.unlink)
        } else if trailer.eq_ignore_ascii_case(REMIND) {
            Some(&mut found.remind)
        } else if trailer.eq_ignore_ascii_case(SCHEDULE) {
            Some(&mut found.schedule)
        } else {
            None
        };
        let dropped = flag.is_some();
        if let Some(flag) = flag {
            *flag = true;
            // The sentence keeps its punctuation, not the token's letters.
            cleaned.push_str(&token[trailer.len()..]);
        } else {
            cleaned.push_str(token);
        }
        token.clear();
        dropped
    };
    for ch in text.chars() {
        if ch.is_whitespace() {
            if flush(&mut token, &mut cleaned, &mut found) {
                swallow_gap = true;
            }
            if swallow_gap {
                continue;
            }
            cleaned.push(ch);
        } else {
            swallow_gap = false;
            token.push(ch);
        }
    }
    flush(&mut token, &mut cleaned, &mut found);
    let cleaned = cleaned.trim().to_string();
    if !found.any() {
        return Ask {
            query: text.to_string(),
            message: text.to_string(),
            recall: found.recall,
            memorize: found.memorize,
            update: found.update,
            delete: found.delete,
            link: found.link,
            unlink: found.unlink,
            remind: found.remind,
            schedule: found.schedule,
        };
    }
    Ask {
        message: if cleaned.is_empty() {
            text.trim().to_string()
        } else {
            cleaned.clone()
        },
        query: cleaned,
        recall: found.recall,
        memorize: found.memorize,
        update: found.update,
        delete: found.delete,
        link: found.link,
        unlink: found.unlink,
        remind: found.remind,
        schedule: found.schedule,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InjectedContext {
    /// Walk order: best first, truncated at the recall cap.
    pub memories: Vec<Memory>,
    /// The exact system message the model sees; empty when nothing is
    /// injected and no style directive applies.
    pub system_message: String,
    pub tokens: i64,
    /// `soul.md`, injected on every turn; 0 when there is none.
    pub soul_tokens: i64,
}

impl InjectedContext {
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }

    pub fn memory_ids(&self) -> Vec<i64> {
        self.memories.iter().map(|memory| memory.id).collect()
    }
}

/// The fallback when the brain cannot run at all: no soul, no recall, but the
/// task sections and the style directive still reach the model.
pub fn empty_context(brevity: Brevity, ask: &Ask) -> InjectedContext {
    let tasks = Tasks::from(ask);
    InjectedContext {
        memories: Vec::new(),
        system_message: render(None, &[], &[], brevity, tasks, &clock(tasks)),
        tokens: 0,
        soul_tokens: 0,
    }
}

/// The wall clock, read only by a turn whose sections quote it.
fn clock(tasks: Tasks) -> String {
    if tasks.reminding {
        crate::reminder::local_now()
    } else {
        String::new()
    }
}

/// Mirrors the brain folder into the index and answers whether it moved. The
/// embedder loader only runs when a note actually changed. A `brain.model`
/// change rebuilds the vector table at the new width, staling every note.
pub fn sync<F>(
    storage: &Storage,
    config: &BrainConfig,
    load_embedder: F,
) -> Result<bool, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    let wanted = config.model.canonical();
    let swapping = !storage.index_matches(&wanted)?;
    let dir = notes::brain_dir(config.path.as_deref())?;
    let notes = notes::read_notes(&dir)?;
    // A swap invalidates every vector, so everything is stale by definition.
    let stale: Vec<String> = if swapping {
        notes.iter().map(|note| note.slug.clone()).collect()
    } else {
        let plan = storage.note_sync_plan(&notes)?;
        if !plan.changed {
            return Ok(false);
        }
        plan.stale
    };

    // One load serves the width probe and the batch: the expensive part of an
    // embedder is getting hold of it.
    let mut embedder = if swapping || !stale.is_empty() {
        Some(load_embedder()?)
    } else {
        None
    };
    if swapping {
        let dim = match config.model.known_dim() {
            Some(dim) => dim,
            // Only the model can say how wide an HTTP backend's vectors are.
            None => embed::probe_dim(
                embedder
                    .as_deref_mut()
                    .expect("an embedder is loaded whenever a swap is in flight"),
            )?,
        };
        storage.rebuild_index(&wanted, dim)?;
    }

    let embeddings = match embedder.as_deref_mut() {
        Some(embedder) => embed_notes(&notes, &stale, embedder)?,
        None => Vec::new(),
    };
    storage.sync_notes(&notes, &embeddings)?;
    Ok(true)
}

/// One batch through the model for exactly the notes the plan named.
pub fn embed_stale<F>(
    notes: &[notes::NoteFile],
    stale: &[String],
    load_embedder: F,
) -> Result<Vec<(String, Vec<f32>)>, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    if stale.is_empty() {
        return Ok(Vec::new());
    }
    embed_notes(notes, stale, load_embedder()?.as_mut())
}

/// Embeds exactly the named notes with an embedder the caller already holds.
pub fn embed_notes(
    notes: &[notes::NoteFile],
    stale: &[String],
    embedder: &mut dyn Embedder,
) -> Result<Vec<(String, Vec<f32>)>, BrainError> {
    if stale.is_empty() {
        return Ok(Vec::new());
    }
    let by_slug: HashMap<&str, &str> = notes
        .iter()
        .map(|note| (note.slug.as_str(), note.content.as_str()))
        .collect();
    let texts: Vec<&str> = stale
        .iter()
        .filter_map(|slug| by_slug.get(slug.as_str()).copied())
        .collect();
    let vectors = embedder.embed(&texts)?;
    Ok(stale.iter().cloned().zip(vectors).collect())
}

/// Assembles the context for one turn: recalled notes for `/brain` — and for
/// the memory triggers, which recall too so the model can see what it should
/// link, rewrite or trash — the matching task section, and the style
/// directive always. `history` excludes the ask. A turn with no triggers
/// never loads the embedder. `None` storage means no database: saving still
/// works, recall has nothing to read.
pub fn build_context<F>(
    storage: Option<&Storage>,
    config: &BrainConfig,
    history: &[Message],
    ask: &Ask,
    brevity: Brevity,
    load_embedder: F,
) -> Result<InjectedContext, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    let dir = notes::brain_dir(config.path.as_deref())?;
    let soul = notes::read_soul(&dir)?;
    let mut kept = Vec::new();
    let mut tokens = 0;
    if let Some(storage) = storage.filter(|_| ask.any()) {
        let count = storage.count_memories()?;
        if count > 0 {
            let query = query_text(history, &ask.query);
            let mut embedder = load_embedder()?;
            let embedding = embedder
                .embed(&[query.as_str()])?
                .into_iter()
                .next()
                .ok_or_else(|| EmbedError::Embed("the embedder returned no vector".to_string()))?;
            let cap = i64::from(config.cap_tokens);
            // A write turn keeps the whole ranked sweep and lets the token cap
            // be the only limit: the note it must rewrite, trash or link is
            // often not among the best few answers to what was said.
            let width = if ask.writes() {
                Width {
                    keep: count as usize,
                    floor: 0.0,
                }
            } else {
                Width {
                    keep: config.top_k as usize,
                    floor: f64::from(config.min_relevance.clamp(0.0, 1.0)),
                }
            };
            for memory in recall(storage, config, &embedding, count as usize, width)? {
                if tokens + memory.tokens > cap {
                    break;
                }
                tokens += memory.tokens;
                kept.push(memory);
            }
        }
    }
    // Content is what recall chose; names are what exists. Only a write turn
    // gets the index — an answer must not read a folder listing back to you.
    let names = if ask.writes() {
        notes::list_slugs(&dir)?
    } else {
        Vec::new()
    };
    let tasks = Tasks::from(ask);
    Ok(InjectedContext {
        system_message: render(soul.as_ref(), &kept, &names, brevity, tasks, &clock(tasks)),
        soul_tokens: soul.map_or(0, |soul| soul.tokens),
        memories: kept,
        tokens,
    })
}

/// Which task sections the turn earns. One field per trigger, so the next one
/// is a field rather than another parameter.
#[derive(Default, Clone, Copy)]
struct Tasks {
    saving: bool,
    updating: bool,
    deleting: bool,
    linking: bool,
    unlinking: bool,
    reminding: bool,
    scheduling: bool,
}

impl From<&Ask> for Tasks {
    fn from(ask: &Ask) -> Self {
        Self {
            saving: ask.memorize,
            updating: ask.update,
            deleting: ask.delete,
            linking: ask.link,
            unlinking: ask.unlink,
            reminding: ask.remind,
            scheduling: ask.schedule,
        }
    }
}

/// How much of the ranked sweep survives into the prompt.
struct Width {
    keep: usize,
    floor: f64,
}

/// One KNN sweep, a personalized PageRank seeded on the closest `top_k`, and a
/// rank blending the two. `width` then decides how much of that ranking
/// survives; the seeding and so the order never change with it. Deterministic:
/// same brain, same question, same order.
fn recall(
    storage: &Storage,
    config: &BrainConfig,
    embedding: &[f32],
    count: usize,
    width: Width,
) -> Result<Vec<Memory>, BrainError> {
    let swept = storage.knn(embedding, count)?;
    let similarity: Vec<f64> = swept
        .iter()
        // Unit vectors: cos = 1 - d²/2, clamped to walkable mass.
        .map(|(_, distance)| (1.0 - distance * distance / 2.0).clamp(0.0, 1.0))
        .collect();

    let graph = graph::brain_graph(storage, config.similarity_edge_threshold)?;
    let index_of: HashMap<i64, usize> = swept
        .iter()
        .enumerate()
        .map(|(index, (memory, _))| (memory.id, index))
        .collect();
    // Parallel edges (a link that is also similar) add up.
    let mut weighted: HashMap<(usize, usize), f64> = HashMap::new();
    for edge in &graph.edges {
        let (Some(&a), Some(&b)) = (index_of.get(&edge.a), index_of.get(&edge.b)) else {
            continue;
        };
        *weighted.entry((a.min(b), a.max(b))).or_default() += f64::from(edge.weight);
    }
    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![Vec::new(); swept.len()];
    let mut degree = vec![0.0f64; swept.len()];
    for (&(a, b), &weight) in &weighted {
        neighbors[a].push((b, weight));
        neighbors[b].push((a, weight));
        degree[a] += weight;
        degree[b] += weight;
    }

    // Restart mass sits on the seeds, proportional to how well each answers the
    // question; a question near nothing restarts evenly across them.
    let seeds = (config.top_k as usize).min(swept.len());
    let mut restart = vec![0.0f64; swept.len()];
    let seed_total: f64 = similarity[..seeds].iter().sum();
    for index in 0..seeds {
        restart[index] = if seed_total > 0.0 {
            similarity[index] / seed_total
        } else {
            1.0 / seeds as f64
        };
    }

    let mut mass = restart.clone();
    let mut next = vec![0.0f64; swept.len()];
    for _ in 0..WALK_ITERATIONS {
        // Mass with nowhere to go restarts, so the total stays 1.
        let dangling: f64 = mass
            .iter()
            .zip(&degree)
            .filter(|(_, &degree)| degree == 0.0)
            .map(|(mass, _)| mass)
            .sum();
        for (index, slot) in next.iter_mut().enumerate() {
            let arriving: f64 = neighbors[index]
                .iter()
                .map(|&(from, weight)| mass[from] * weight / degree[from])
                .sum();
            *slot =
                (1.0 - DAMPING) * restart[index] + DAMPING * (arriving + dangling * restart[index]);
        }
        std::mem::swap(&mut mass, &mut next);
    }

    let walked_peak = mass.iter().copied().fold(0.0f64, f64::max);
    // Scored on the query's own scale: raw cosine baselines differ per model.
    let sim_min = similarity.iter().copied().fold(f64::INFINITY, f64::min);
    let sim_max = similarity.iter().copied().fold(0.0f64, f64::max);
    let spread = sim_max - sim_min;
    let mut order: Vec<usize> = (0..swept.len()).collect();
    let score = |index: usize| -> f64 {
        let walked = if walked_peak > 0.0 {
            mass[index] / walked_peak
        } else {
            0.0
        };
        let close = if spread > 0.0 {
            (similarity[index] - sim_min) / spread
        } else {
            1.0
        };
        SIMILARITY_SHARE * close + (1.0 - SIMILARITY_SHARE) * walked
    };
    order.sort_by(|&a, &b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(swept[a].0.id.cmp(&swept[b].0.id))
    });
    let floor = score(order[0]) * width.floor;
    order.retain(|&index| score(index) >= floor);
    order.truncate(width.keep);
    let mut swept: Vec<Option<Memory>> =
        swept.into_iter().map(|(memory, _)| Some(memory)).collect();
    Ok(order
        .into_iter()
        .filter_map(|index| swept[index].take())
        .collect())
}

/// The last two turns and the new prompt, newline-joined. Injected system
/// messages never feed retrieval.
fn query_text(history: &[Message], user_msg: &str) -> String {
    let spoken: Vec<&str> = history
        .iter()
        .filter(|message| message.role != Role::System)
        .map(|message| message.content.as_str())
        .collect();
    let start = spoken.len().saturating_sub(QUERY_MESSAGES);
    let mut parts = spoken[start..].to_vec();
    if !user_msg.is_empty() {
        parts.push(user_msg);
    }
    parts.join("\n")
}

const SOUL_PREAMBLE: &str =
    "The user's standing instructions, from their soul.md note. Follow them in every reply.";

/// Without this framing a small model reads the notes as a pasted document
/// rather than background about the user.
const PREAMBLE: &str = "The user's saved memories, recalled because they may \
relate to this message. Treat them as facts about the user and their world \
that you were told earlier. [[name]] links one memory to another. Use what \
is relevant and ignore the rest.";

const SAVING: &str = "The user asked you to save a memory. Call save_memory \
with the fact distilled to a short third-person note. Never save a second \
note about a topic a memory above already covers — if that fact changed, \
answer that /update-memory rewrites it instead. Name the user if a memory \
gives their name, and write every mention of a person or topic that already \
has a memory as a [[slug]] link — for example: \"[[anna]] left her car keys \
on the desk.\" Then confirm in one line without repeating the other memories.";

const UPDATING: &str = "The user asked you to update a memory. Pick the \
memory above that is about this fact, rewrite that whole note with the \
change applied, and call update_memory with its exact slug and the rewritten \
note. Keep [[slug]] links that still hold. If none of the memories above is \
about this fact, say so in one line and call nothing. Otherwise confirm in \
one line without repeating the other memories.";

const DELETING: &str = "The user asked you to delete a memory. Pick the \
memory above that is about this fact and call delete_memory with its exact \
slug. If none of the memories above is about this fact, say so in one line \
and call nothing. Otherwise confirm in one line without repeating the other \
memories.";

const NAMING: &str = "Every memory that exists, by name. The ones written out \
above were recalled for this message; the others exist but their content was \
not. A name here is a slug you may pass to a tool.";

const UNLINKING: &str = "The user asked you to disconnect two memories. \
Call unlink_memory with their exact slugs: `from` is the memory the link is \
written in, `to` is the memory it points at. If the two memories the user \
means are not both above, say so in one line and call nothing. Otherwise \
confirm in one line without repeating the other memories.";

const LINKING: &str = "The user asked you to connect two memories. Call \
link_memory with their exact slugs: `from` is the memory the link is written \
into, `to` is the memory it points at — for a memory `football` about the \
user playing and a memory `mitul` about the user, from is football and to is \
mitul. If the two memories the user means are not both above, say so in one \
line and call nothing. Otherwise confirm in one line without repeating the \
other memories.";

const REMINDING: &str = "The user asked you to set a reminder. Call \
set_reminder with `text` — what they should be told when it goes off, in a few \
words — and exactly one time: `in_minutes` for anything measured from now, \
`due_at` as YYYY-MM-DD HH:MM for a named day or clock time. Prefer \
`in_minutes` whenever the user said how long from now — for \"remind me in \
half an hour to call mum\", text is \"call mum\" and in_minutes is 30. Only \
when the user asked for a recurring reminder, pass `every` instead — \
\"every day 09:00\", \"every monday 9:30\", or \"every 45m\" — and omit the \
one-off times. Then confirm in one line.";

const SCHEDULING: &str = "The user asked odyn to run a prompt on a schedule. \
Call schedule_ask with `prompt` — the question to ask each time, in the \
user's own words — and `every`: \"every day 09:00\", \"every monday 9:30\", \
or \"every 45m\". For \"brief me every morning at 9 on my calendar\", prompt \
is \"brief me on my calendar\" and every is \"every day 09:00\". If the user \
asked it to use their memory, begin the prompt with /brain. Then confirm in \
one line.";

/// The injected system message, golden-tested byte for byte: `## Instructions`
/// (soul.md, when present), `## Memories` (omitted when empty), `## Memory
/// names` on a write turn, a task section per mentioned trigger, then
/// `## Style`. `now` is the local wall clock the reminder section quotes.
fn render(
    soul: Option<&notes::NoteFile>,
    memories: &[Memory],
    names: &[String],
    brevity: Brevity,
    tasks: Tasks,
    now: &str,
) -> String {
    let mut sections = Vec::new();
    if let Some(soul) = soul {
        sections.push(format!(
            "## Instructions\n{SOUL_PREAMBLE}\n{}",
            soul.content
        ));
    }
    if !memories.is_empty() {
        let mut lines = vec![format!("## Memories\n{PREAMBLE}")];
        lines.extend(
            memories
                .iter()
                .map(|memory| format!("\n### {}\n{}", memory.slug, memory.content)),
        );
        sections.push(lines.join("\n"));
    }
    if !names.is_empty() {
        let shown = names.len().min(NAMES_SHOWN);
        let mut line = names[..shown].join(", ");
        if names.len() > shown {
            line.push_str(&format!(", and {} more", names.len() - shown));
        }
        sections.push(format!("## Memory names\n{NAMING}\n{line}"));
    }
    if tasks.saving {
        sections.push(format!("## Saving\n{SAVING}"));
    }
    if tasks.updating {
        sections.push(format!("## Updating\n{UPDATING}"));
    }
    if tasks.deleting {
        sections.push(format!("## Deleting\n{DELETING}"));
    }
    if tasks.linking {
        sections.push(format!("## Linking\n{LINKING}"));
    }
    if tasks.unlinking {
        sections.push(format!("## Unlinking\n{UNLINKING}"));
    }
    if tasks.reminding {
        let mut section = format!("## Reminders\n{REMINDING}");
        if !now.is_empty() {
            section.push_str(&format!("\nThe current local time is {now}."));
        }
        sections.push(section);
    }
    if tasks.scheduling {
        sections.push(format!("## Scheduling\n{SCHEDULING}"));
    }
    if let Some(directive) = brevity.directive() {
        sections.push(format!("## Style\n{directive}"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_tests::{note, note_with_links, vector};
    use crate::storage::tests::TempDir;

    /// Always embeds to a unit vector on `axis`, so a test picks what the query
    /// lands near.
    struct FixedEmbedder {
        axis: usize,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vector(self.axis, 0.0)).collect())
        }
    }

    struct Recorder(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

    impl Embedder for Recorder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0
                .borrow_mut()
                .extend(texts.iter().map(|text| text.to_string()));
            Ok(texts.iter().map(|_| vector(0, 0.0)).collect())
        }
    }

    /// The folder is pinned into the test temp area and never created: no test
    /// may read the real data directory, not even for the name index.
    fn config(top_k: u32, cap: u32) -> BrainConfig {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        BrainConfig {
            top_k,
            cap_tokens: cap,
            path: Some(
                std::env::temp_dir().join(format!("odyn-nobrain-{}-{unique}", std::process::id())),
            ),
            ..BrainConfig::default()
        }
    }

    fn open(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        (dir, storage)
    }

    /// `cern-trip` sits at the query's axis, `espresso-order` leans off it,
    /// `yaml-grudge` is further still.
    fn seeded(label: &str) -> (TempDir, Storage) {
        let (dir, storage) = open(label);
        let notes = vec![
            note("cern-trip", "went to CERN in june"),
            note("espresso-order", "likes espresso"),
            note("yaml-grudge", "hates yaml"),
        ];
        let embeddings = vec![
            ("cern-trip".to_string(), vector(0, 0.0)),
            ("espresso-order".to_string(), vector(0, 0.35)),
            ("yaml-grudge".to_string(), vector(10, 0.0)),
        ];
        storage.sync_notes(&notes, &embeddings).expect("sync");
        (dir, storage)
    }

    fn at_axis_zero() -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FixedEmbedder { axis: 0 }))
    }

    fn never() -> Result<Box<dyn Embedder>, EmbedError> {
        Err(EmbedError::Load(
            "the embedder must not be loaded here".to_string(),
        ))
    }

    fn recalled(message: &str) -> Ask {
        Ask {
            message: message.to_string(),
            query: message.to_string(),
            recall: true,
            memorize: false,
            update: false,
            delete: false,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        }
    }

    #[test]
    fn the_trigger_is_found_anywhere_and_stripped_cleanly() {
        assert_eq!(
            parse_ask("/brain what did we decide about tokio?"),
            recalled("what did we decide about tokio?")
        );
        assert_eq!(parse_ask("check /BRAIN please"), recalled("check please"));
        assert_eq!(
            parse_ask("multi\nline /brain\nquestion"),
            recalled("multi\nline question")
        );
        assert_eq!(parse_ask("remind me, /brain."), recalled("remind me, ."));
        assert_eq!(
            parse_ask("my brainstorm about /brains"),
            Ask {
                message: "my brainstorm about /brains".to_string(),
                query: "my brainstorm about /brains".to_string(),
                recall: false,
                memorize: false,
                update: false,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
        // Only the trigger: recall runs on history alone, message stays non-empty.
        assert_eq!(
            parse_ask("  /brain  "),
            Ask {
                message: "/brain".to_string(),
                query: String::new(),
                recall: true,
                memorize: false,
                update: false,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
    }

    #[test]
    fn the_memorize_trigger_parses_alone_and_with_recall() {
        assert_eq!(
            parse_ask("/memory I take espresso, no sugar"),
            Ask {
                message: "I take espresso, no sugar".to_string(),
                query: "I take espresso, no sugar".to_string(),
                recall: false,
                memorize: true,
                update: false,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
        assert_eq!(
            parse_ask("/brain /memory update what you know about coffee"),
            Ask {
                message: "update what you know about coffee".to_string(),
                query: "update what you know about coffee".to_string(),
                recall: true,
                memorize: true,
                update: false,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
        assert_eq!(
            parse_ask("  /memory  "),
            Ask {
                message: "/memory".to_string(),
                query: String::new(),
                recall: false,
                memorize: true,
                update: false,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
    }

    #[test]
    fn the_delete_trigger_parses_alone() {
        assert_eq!(
            parse_ask("/delete-memory forget where my car keys are"),
            Ask {
                message: "forget where my car keys are".to_string(),
                query: "forget where my car keys are".to_string(),
                recall: false,
                memorize: false,
                update: false,
                delete: true,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
    }

    /// `/link-memory` is its own token, and every trigger recalls, so a link
    /// turn sees the notes it is being asked to connect.
    #[test]
    fn the_link_trigger_parses_alone_and_asks_for_recall() {
        let ask = parse_ask("/link-memory connect football to me");
        assert_eq!(
            ask,
            Ask {
                message: "connect football to me".to_string(),
                query: "connect football to me".to_string(),
                recall: false,
                memorize: false,
                update: false,
                delete: false,
                link: true,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
        assert!(ask.any(), "a link turn touches the brain");
        assert!(!parse_ask("/memory a fact").link);
    }

    /// `/unlink-memory` is its own token: it never trips the `/link-memory` flag.
    #[test]
    fn the_unlink_trigger_parses_and_never_reads_as_link() {
        let ask = parse_ask("/unlink-memory football has nothing to do with anna");
        assert!(ask.unlink);
        assert!(!ask.link);
        assert!(
            ask.writes(),
            "an unlink turn recalls wide and sees the names"
        );
        assert_eq!(ask.message, "football has nothing to do with anna");
        assert!(!parse_ask("/link-memory a and b").unlink);
    }

    #[test]
    fn a_link_turn_gets_the_linking_section_under_the_memories() {
        let (_dir, storage) = seeded("linking");
        let mut ask = recalled("cern?");
        ask.recall = false;
        ask.link = true;
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(!context.is_empty(), "a link must see what it can connect");
        assert!(context.system_message.contains("### cern-trip"));
        assert!(context
            .system_message
            .ends_with(&format!("\n\n## Linking\n{LINKING}")));
    }

    /// `/update-memory` is its own token: it never trips the `/memory` flag.
    #[test]
    fn the_update_trigger_parses_and_never_reads_as_memorize() {
        assert_eq!(
            parse_ask("/update-memory my car keys are on the fridge"),
            Ask {
                message: "my car keys are on the fridge".to_string(),
                query: "my car keys are on the fridge".to_string(),
                recall: false,
                memorize: false,
                update: true,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
        assert_eq!(
            parse_ask("the keys moved, /UPDATE-MEMORY."),
            Ask {
                message: "the keys moved, .".to_string(),
                query: "the keys moved, .".to_string(),
                recall: false,
                memorize: false,
                update: true,
                delete: false,
                link: false,
                unlink: false,
                remind: false,
                schedule: false,
            }
        );
    }

    /// An empty index is what keeps the embedder cold here: `/memory` recalls
    /// like `/brain` does, so the model can see what it should link against.
    #[test]
    fn a_memorize_turn_gets_the_saving_section_without_the_embedder() {
        let brain = TempDir::new("saving-brain");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        crate::notes::write_note(&brain.0, Some("espresso"), "espresso notes").expect("write");
        crate::notes::write_note(&brain.0, Some("birthday"), "a date").expect("write");
        let (_db, storage) = open("saving-db");
        let config = BrainConfig {
            path: Some(brain.0.clone()),
            ..BrainConfig::default()
        };
        let ask = Ask {
            message: "remember this".to_string(),
            query: "remember this".to_string(),
            recall: false,
            memorize: true,
            update: false,
            delete: false,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        };
        let context =
            build_context(Some(&storage), &config, &[], &ask, Brevity::Off, never).expect("build");
        // Names come from the folder, so an unindexed note is still nameable —
        // and reaching them cost no embedder and no injected content.
        assert!(context.is_empty());
        assert_eq!(context.tokens, 0);
        assert_eq!(
            context.system_message,
            format!("## Memory names\n{NAMING}\nbirthday, espresso\n\n## Saving\n{SAVING}")
        );

        // An empty folder still explains the task.
        let empty = BrainConfig {
            path: Some(brain.0.join("missing")),
            ..BrainConfig::default()
        };
        let context = build_context(None, &empty, &[], &ask, Brevity::Off, never).expect("build");
        assert_eq!(context.system_message, format!("## Saving\n{SAVING}"));
    }

    /// The relevance floor and `top_k` decide what answers a question. A write
    /// turn is not answering: it has to reach the note it must change, so it
    /// keeps the whole ranking and lets the token cap be the only limit.
    #[test]
    fn a_write_turn_keeps_what_the_relevance_floor_would_prune() {
        let (_dir, storage) = seeded("wide");
        let narrow = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(
            narrow.memory_ids(),
            vec![1, 2],
            "yaml-grudge is not about cern"
        );

        let mut ask = recalled("cern?");
        ask.recall = false;
        ask.update = true;
        let wide = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(wide.memory_ids(), vec![1, 2, 3]);
        assert!(wide.system_message.contains("### yaml-grudge"));

        // `top_k` stops truncating too, so a tight answering budget does not
        // hide notes from a rewrite.
        let wide = build_context(
            Some(&storage),
            &config(1, 900),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(wide.memories.len(), 3);

        // The cap still binds: it is the one limit a write turn respects.
        let capped = build_context(
            Some(&storage),
            &config(6, 9),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(
            capped.memory_ids(),
            vec![1, 2],
            "5 + 4 tokens fills a cap of 9"
        );
    }

    /// A `/brain` answer never sees the name index — reciting the folder back
    /// is exactly what it would do with one.
    #[test]
    fn a_recall_turn_gets_no_name_index() {
        let (dir, storage) = seeded("names-recall");
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(!context.system_message.contains("## Memory names"));
        drop(dir);
    }

    /// Past 50 the index is summarized rather than spending the recall cap on
    /// slugs.
    #[test]
    fn a_long_folder_summarizes_the_tail_of_the_name_index() {
        let brain = TempDir::new("many-names");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        for index in 0..53 {
            crate::notes::write_note(&brain.0, Some(&format!("note-{index:03}")), "a fact")
                .expect("write");
        }
        let config = BrainConfig {
            path: Some(brain.0.clone()),
            ..BrainConfig::default()
        };
        let ask = Ask {
            message: "forget one".to_string(),
            query: "forget one".to_string(),
            recall: false,
            memorize: false,
            update: false,
            delete: true,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        };
        let context = build_context(None, &config, &[], &ask, Brevity::Off, never).expect("build");
        assert!(context.system_message.contains("note-000, note-001"));
        assert!(context.system_message.contains("note-049, and 3 more"));
        assert!(!context.system_message.contains("note-050"));
    }

    /// An update turn recalls like a save turn and gets its own section — the
    /// recalled notes are the only slugs it can rewrite.
    #[test]
    fn an_update_turn_recalls_and_gets_the_updating_section() {
        let (_dir, storage) = seeded("updating");
        let ask = Ask {
            message: "the keys moved to the fridge".to_string(),
            query: "the keys moved to the fridge".to_string(),
            recall: false,
            memorize: false,
            update: true,
            delete: false,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        };
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(
            !context.is_empty(),
            "an update must see what it can rewrite"
        );
        assert!(context
            .system_message
            .contains("## Updating\nThe user asked you to update a memory."));
        assert!(!context.system_message.contains("## Saving"));
    }

    /// A delete turn recalls too — the recalled notes are the only slugs it
    /// can trash.
    #[test]
    fn a_delete_turn_recalls_and_gets_the_deleting_section() {
        let (_dir, storage) = seeded("deleting");
        let ask = Ask {
            message: "forget about the cern trip".to_string(),
            query: "forget about the cern trip".to_string(),
            recall: false,
            memorize: false,
            update: false,
            delete: true,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        };
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(!context.is_empty(), "a delete must see what it can trash");
        assert!(context
            .system_message
            .contains("## Deleting\nThe user asked you to delete a memory."));
        assert!(!context.system_message.contains("## Saving"));
        assert!(!context.system_message.contains("## Updating"));
    }

    /// The linking fix: a save is informed. `/memory` alone recalls the notes
    /// nearest the message, so `[[slug]]` links have something to land on.
    #[test]
    fn a_memorize_turn_recalls_related_notes_to_link_against() {
        let (_dir, storage) = seeded("informed");
        let brain = TempDir::new("informed-brain");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        crate::notes::write_note(&brain.0, Some("cern-trip"), "went to CERN in june")
            .expect("write");
        let config = BrainConfig {
            path: Some(brain.0.clone()),
            ..config(6, 900)
        };
        let ask = Ask {
            message: "remember the cern talk".to_string(),
            query: "remember the cern talk".to_string(),
            recall: false,
            memorize: true,
            update: false,
            delete: false,
            link: false,
            unlink: false,
            remind: false,
            schedule: false,
        };
        let context = build_context(
            Some(&storage),
            &config,
            &[],
            &ask,
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(!context.is_empty(), "a save must see what it can link");
        assert!(context.system_message.contains("### cern-trip"));
        assert!(context
            .system_message
            .contains("## Saving\nThe user asked you to save a memory."));
    }

    #[test]
    fn the_reminder_trigger_parses_and_asks_nothing_of_the_brain() {
        let ask = parse_ask("/reminder call mum in 20 minutes");
        assert_eq!(ask.message, "call mum in 20 minutes");
        assert!(ask.remind);
        assert!(!ask.recall);
        // Setting a reminder is not reading or writing memory.
        assert!(!ask.any(), "a reminder turn must not touch the brain");
        assert!(!ask.writes());
        assert!(!parse_ask("/reminders are useful").remind);
    }

    #[test]
    fn a_reminder_turn_is_told_how_and_never_loads_the_embedder() {
        // Seeded storage: an embedder load would be possible here, and the
        // erroring loader is what proves none is attempted.
        let (_dir, storage) = seeded("reminder-turn");
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &parse_ask("/reminder stand up in 20 minutes"),
            Brevity::Off,
            never,
        )
        .expect("a reminder turn builds without the embedder");
        assert!(context.is_empty(), "a reminder recalls nothing");
        assert!(context.system_message.contains("## Reminders"));
        assert!(
            !context.system_message.contains("## Memory names"),
            "a reminder is not a write turn and gets no folder listing"
        );
    }

    #[test]
    fn the_schedule_trigger_parses_and_earns_its_section() {
        let ask = parse_ask("/schedule brief me every morning at 9");
        assert!(ask.schedule);
        assert_eq!(ask.message, "brief me every morning at 9");
        assert!(!ask.any(), "a schedule turn must not touch the brain");
        assert!(!parse_ask("/scheduled for later").schedule);

        let context = build_context(
            None,
            &config(6, 900),
            &[],
            &parse_ask("/schedule brief me every morning at 9"),
            Brevity::Off,
            never,
        )
        .expect("a schedule turn builds without the embedder");
        assert!(context.system_message.contains("## Scheduling"));
    }

    #[test]
    fn the_reminder_section_states_the_clock() {
        let tasks = Tasks {
            reminding: true,
            ..Tasks::default()
        };
        assert_eq!(
            render(
                None,
                &[],
                &[],
                Brevity::Off,
                tasks,
                "2026-08-10 14:32 (Monday)"
            ),
            format!(
                "## Reminders\n{REMINDING}\nThe current local time is 2026-08-10 14:32 (Monday)."
            )
        );
        // An unreadable clock drops the line rather than inventing a time.
        assert_eq!(
            render(None, &[], &[], Brevity::Off, tasks, ""),
            format!("## Reminders\n{REMINDING}")
        );
    }

    #[test]
    fn the_template_is_byte_identical_to_the_plan() {
        let (_dir, storage) = seeded("golden");
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");

        assert_eq!(
            context.system_message,
            "## Memories\n\
             The user's saved memories, recalled because they may relate to \
             this message. Treat them as facts about the user and their world \
             that you were told earlier. [[name]] links one memory to another. \
             Use what is relevant and ignore the rest.\n\
             \n\
             ### cern-trip\n\
             went to CERN in june\n\
             \n\
             ### espresso-order\n\
             likes espresso"
        );
        assert_eq!(context.tokens, 5 + 4, "yaml-grudge is not about cern");
        assert_eq!(context.memory_ids(), vec![1, 2]);

        let again = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("rebuild");
        assert_eq!(again, context, "same inputs must rebuild identically");
    }

    #[test]
    fn a_linked_neighbor_of_the_answer_beats_an_unlinked_stranger() {
        let (_dir, storage) = open("walk");
        let notes = vec![
            note_with_links(
                "anchor",
                "answers the question, links [[sidekick]]",
                &["sidekick"],
            ),
            note("sidekick", "far from the question, near the anchor"),
            note("stranger", "equally far, connected to nothing"),
        ];
        let embeddings = vec![
            ("anchor".to_string(), vector(0, 0.0)),
            ("sidekick".to_string(), vector(100, 0.0)),
            ("stranger".to_string(), vector(200, 0.0)),
        ];
        storage.sync_notes(&notes, &embeddings).expect("sync");

        let context = build_context(
            Some(&storage),
            &config(3, 900),
            &[],
            &recalled("q"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        let slugs: Vec<&str> = context
            .memories
            .iter()
            .map(|memory| memory.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["anchor", "sidekick"]);
    }

    #[test]
    fn the_floor_and_top_k_bound_what_recall_returns() {
        let (_dir, storage) = seeded("bounds");
        let everything = BrainConfig {
            min_relevance: 0.0,
            ..config(6, 900)
        };
        let context = build_context(
            Some(&storage),
            &everything,
            &[],
            &recalled("q"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memory_ids(), vec![1, 2, 3]);

        let two = BrainConfig {
            min_relevance: 0.0,
            ..config(2, 900)
        };
        let context = build_context(
            Some(&storage),
            &two,
            &[],
            &recalled("q"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memory_ids(), vec![1, 2]);
    }

    #[test]
    fn the_cap_truncates_exactly_where_the_budget_runs_out() {
        let (_dir, storage) = seeded("cap");
        // cern-trip (5) + espresso-order (4) fit a cap of 9 exactly.
        let context = build_context(
            Some(&storage),
            &config(6, 9),
            &[],
            &recalled("q"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memories.len(), 2);
        assert_eq!(context.tokens, 9);
        let context = build_context(
            Some(&storage),
            &config(6, 8),
            &[],
            &recalled("q"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memories.len(), 1);
        assert_eq!(context.tokens, 5);
    }

    #[test]
    fn an_empty_brain_never_loads_the_embedder() {
        let (_dir, storage) = open("empty");
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("q"),
            Brevity::Off,
            never,
        )
        .expect("build");
        assert!(context.is_empty());
        assert_eq!(context.system_message, "");
    }

    #[test]
    fn the_query_is_the_last_two_turns_and_the_prompt_without_system_messages() {
        let (_dir, storage) = open("query");
        storage
            .sync_notes(
                &[note("anything", "anything")],
                &[("anything".to_string(), vector(0, 0.0))],
            )
            .expect("sync");
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let history = [
            Message::new(Role::User, "old and beyond the window"),
            Message::new(Role::User, "u1"),
            Message::new(Role::Assistant, "a1"),
            Message::new(Role::System, "injected earlier, never retrieved on"),
            Message::new(Role::User, "u2"),
            Message::new(Role::Assistant, "a2"),
        ];
        let recorder = Recorder(asked.clone());
        build_context(
            Some(&storage),
            &config(6, 900),
            &history,
            &recalled("now"),
            Brevity::Off,
            move || Ok(Box::new(recorder)),
        )
        .expect("build");
        assert_eq!(asked.borrow().as_slice(), ["u1\na1\nu2\na2\nnow"]);
    }

    #[test]
    fn a_saved_turn_records_exactly_the_injections_that_built_its_context() {
        let (_dir, storage) = seeded("record");
        let context = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        let conversation = storage
            .create_conversation("cern", "ollama", "llama3.2:3b")
            .expect("create");
        storage
            .append_turn(
                conversation.id,
                "cern?",
                "you visited in june",
                None,
                &context.memory_ids(),
            )
            .expect("save");

        let user_message = storage.messages(conversation.id).expect("messages")[0].id;
        let recorded: Vec<(Option<i64>, i64)> = storage
            .injections(conversation.id)
            .expect("injections")
            .into_iter()
            .map(|injection| (injection.message_id, injection.memory_id))
            .collect();
        assert_eq!(
            recorded,
            context
                .memory_ids()
                .iter()
                .map(|memory| (Some(user_message), *memory))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn each_brevity_level_appends_exactly_its_style_section() {
        let (_dir, storage) = seeded("brevity");
        let base = build_context(
            Some(&storage),
            &config(6, 900),
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("off")
        .system_message;
        for level in [Brevity::Lite, Brevity::Full, Brevity::Ultra] {
            let message = build_context(
                Some(&storage),
                &config(6, 900),
                &[],
                &recalled("cern?"),
                level,
                at_axis_zero,
            )
            .expect("build")
            .system_message;
            let directive = level.directive().expect("directive");
            assert_eq!(message, format!("{base}\n\n## Style\n{directive}"));
        }

        let message = empty_context(Brevity::Ultra, &parse_ask("hello")).system_message;
        assert_eq!(
            message,
            format!("## Style\n{}", Brevity::Ultra.directive().expect("ultra"))
        );
        assert_eq!(
            empty_context(Brevity::Off, &parse_ask("hello")).system_message,
            ""
        );
    }

    /// soul.md rides every turn — triggers or none, database or none — and its
    /// cost is counted separately from recall.
    #[test]
    fn the_soul_note_is_injected_on_every_turn_and_counted() {
        let brain = TempDir::new("soul-brain");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        std::fs::write(
            brain.0.join("soul.md"),
            "---\nkind: soul\n---\nAlways answer in metric.\n",
        )
        .expect("write soul");
        let config = BrainConfig {
            path: Some(brain.0.clone()),
            ..config(6, 900)
        };

        let bare = build_context(None, &config, &[], &parse_ask("hello"), Brevity::Off, never)
            .expect("build");
        assert_eq!(
            bare.system_message,
            format!("## Instructions\n{SOUL_PREAMBLE}\nAlways answer in metric.")
        );
        assert_eq!(bare.soul_tokens, 6, "24 chars make 6 tokens");
        assert_eq!(bare.tokens, 0);
        assert!(bare.is_empty());

        // With recall, the soul leads and the memories follow.
        let (_dir, storage) = seeded("soul-recall");
        let recalled = build_context(
            Some(&storage),
            &config,
            &[],
            &recalled("cern?"),
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(recalled
            .system_message
            .starts_with(&format!("## Instructions\n{SOUL_PREAMBLE}\n")));
        assert!(recalled.system_message.contains("\n\n## Memories\n"));
        assert_eq!(recalled.soul_tokens, 6);
        assert_eq!(
            recalled.tokens, 9,
            "recall's budget never pays for the soul"
        );
    }

    /// The soul is not a memory: never indexed, recalled, named or overwritten
    /// by a model-derived slug.
    #[test]
    fn the_soul_note_is_invisible_to_the_memory_pipeline() {
        let brain = TempDir::new("soul-hidden");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        std::fs::write(brain.0.join("soul.md"), "Standing orders.\n").expect("write soul");
        crate::notes::write_note(&brain.0, Some("espresso"), "espresso notes").expect("write");

        let notes = crate::notes::read_notes(&brain.0).expect("read");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].slug, "espresso");
        assert_eq!(
            crate::notes::list_slugs(&brain.0).expect("slugs"),
            vec!["espresso".to_string()]
        );
        let soul = crate::notes::read_soul(&brain.0)
            .expect("soul")
            .expect("some");
        assert_eq!(soul.content, "Standing orders.");
        assert_eq!(
            crate::notes::read_soul(&brain.0.join("missing")).expect("missing dir"),
            None
        );

        // A save can never claim the name: explicit errors, derived dodges.
        assert!(matches!(
            crate::notes::write_note(&brain.0, Some("soul"), "not instructions"),
            Err(crate::notes::NotesError::Exists(_))
        ));
        assert_eq!(
            crate::notes::write_note(&brain.0, None, "soul").expect("derived"),
            "soul-2"
        );
    }

    #[test]
    fn sync_mirrors_the_folder_and_reloads_nothing_when_unchanged() {
        let brain = TempDir::new("sync-brain");
        std::fs::create_dir_all(&brain.0).expect("create brain dir");
        let (_db, storage) = open("sync-db");
        let config = BrainConfig {
            path: Some(brain.0.clone()),
            ..BrainConfig::default()
        };

        crate::notes::write_note(&brain.0, None, "first note, see [[second-note]]")
            .expect("write first");
        crate::notes::write_note(&brain.0, Some("second-note"), "the second")
            .expect("write second");
        let embeds = std::cell::Cell::new(0usize);
        let changed = sync(&storage, &config, || {
            embeds.set(embeds.get() + 1);
            Ok(Box::new(crate::embed::FakeEmbedder) as Box<dyn Embedder>)
        })
        .expect("sync");
        assert!(changed);
        assert_eq!(embeds.get(), 1);
        let listed = storage.list_memories().expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(storage.links().expect("links").len(), 1);

        // Unchanged: the loader must not run.
        assert!(!sync(&storage, &config, never).expect("resync"));

        // A deleted file prunes without loading the model.
        crate::notes::delete_note(&brain.0, "second-note").expect("delete");
        assert!(sync(&storage, &config, never).expect("prune"));
        assert_eq!(storage.count_memories().expect("count"), 1);
    }
}
