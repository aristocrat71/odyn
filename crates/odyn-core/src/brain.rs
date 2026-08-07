//! The one gate every injected memory token passes through.
//!
//! Nothing is injected unless the user asks: a message mentioning `/brain`
//! recalls, every other message reaches the model bare (style directives
//! aside — brevity is not memory). Recall is a walk, not a lookup: the
//! question's embedding seeds a personalized PageRank over the brain graph,
//! so a note earns its way into context either by matching the question or
//! by sitting close — by wikilink, similarity or shared use — to notes that
//! match. `build_context` is the only producer of memory context in Odyn;
//! the CLI, the GUI, `--show-context` and the composer ledger all render its
//! output, which is what keeps the ledger equal to reality.

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

/// Two turns of history join the retrieval query.
const QUERY_MESSAGES: usize = 4;
/// Random-walk-with-restart: how much mass follows edges each step.
const DAMPING: f64 = 0.85;
const WALK_ITERATIONS: usize = 30;
/// The final rank: this share answers the question directly, the rest is
/// standing in the walked neighborhood of what does.
const SIMILARITY_SHARE: f64 = 0.6;

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

/// A user message with its `/brain` mention, if any, understood and removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// What the model sees and the transcript stores: the message without
    /// the trigger token. A message that was only the trigger keeps it —
    /// an empty user message would be worse.
    pub message: String,
    /// What retrieval embeds: the cleaned text, possibly empty — a bare
    /// `/brain` recalls on the conversation history alone.
    pub query: String,
    /// Whether recall runs for this turn.
    pub recall: bool,
}

/// Finds a whitespace-delimited `/brain` anywhere in the message, case
/// insensitively, tolerating trailing punctuation. The token and the
/// whitespace after it are removed; everything else stays byte-for-byte.
pub fn parse_ask(text: &str) -> Ask {
    let mut cleaned = String::with_capacity(text.len());
    let mut token = String::new();
    let mut recall = false;
    let mut swallow_gap = false;
    let flush = |token: &mut String, cleaned: &mut String, recall: &mut bool| -> bool {
        if token.is_empty() {
            return false;
        }
        let trailer = token.trim_end_matches([',', '.', ';', ':', '!', '?']);
        let dropped = trailer.eq_ignore_ascii_case(TRIGGER);
        if dropped {
            *recall = true;
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
            if flush(&mut token, &mut cleaned, &mut recall) {
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
    flush(&mut token, &mut cleaned, &mut recall);
    let cleaned = cleaned.trim().to_string();
    if !recall {
        return Ask {
            query: text.to_string(),
            message: text.to_string(),
            recall,
        };
    }
    Ask {
        message: if cleaned.is_empty() {
            text.trim().to_string()
        } else {
            cleaned.clone()
        },
        query: cleaned,
        recall,
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
}

impl InjectedContext {
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty()
    }

    /// Injection order, for recording.
    pub fn memory_ids(&self) -> Vec<i64> {
        self.memories.iter().map(|memory| memory.id).collect()
    }
}

/// The context of a turn that does not recall — no memories, but the style
/// directive still applies. Also the context when there is no database.
pub fn empty_context(brevity: Brevity) -> InjectedContext {
    InjectedContext {
        memories: Vec::new(),
        system_message: render(&[], brevity),
        tokens: 0,
    }
}

/// Mirrors the brain folder into the index: new and edited notes are
/// re-embedded, rows whose file is gone are dropped. The embedder is behind
/// a loader so it is only ever loaded — and on first use, downloaded — when
/// a note actually changed. Answers whether the index moved.
///
/// Pointing the index at the configured model comes first, so a `brain.model`
/// change is noticed here: the vector table is rebuilt at the new width and
/// every note comes back stale, which is what re-embeds the folder.
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
    // A swap invalidates every vector, so nothing the old index says about
    // staleness is worth asking; everything is stale by definition.
    let stale: Vec<String> = if swapping {
        notes.iter().map(|note| note.slug.clone()).collect()
    } else {
        let plan = storage.note_sync_plan(&notes)?;
        if !plan.changed {
            return Ok(false);
        }
        plan.stale
    };

    // One load serves both the width probe and the batch: the expensive part
    // of an embedder is getting hold of it, not using it.
    let mut embedder = if swapping || !stale.is_empty() {
        Some(load_embedder()?)
    } else {
        None
    };
    if swapping {
        let dim = match config.model.known_dim() {
            Some(dim) => dim,
            // Only the model itself can say how wide an HTTP backend's
            // vectors are, so ask it before sizing the table.
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

/// Embeds exactly the named notes with an embedder the caller already holds —
/// what a surface uses when it must control when the model is loaded.
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

/// Assembles the context for a recalling turn. `history` is the conversation
/// so far, without `user_msg`; `user_msg` is the cleaned question. The
/// embedder is behind a loader so an empty brain never loads it.
pub fn build_context<F>(
    storage: &Storage,
    config: &BrainConfig,
    history: &[Message],
    user_msg: &str,
    brevity: Brevity,
    load_embedder: F,
) -> Result<InjectedContext, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    let count = storage.count_memories()?;
    if count == 0 {
        return Ok(empty_context(brevity));
    }
    let query = query_text(history, user_msg);
    let mut embedder = load_embedder()?;
    let embedding = embedder
        .embed(&[query.as_str()])?
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError::Embed("the embedder returned no vector".to_string()))?;
    let ranked = recall(storage, config, &embedding, count as usize)?;

    let cap = i64::from(config.cap_tokens);
    let mut kept = Vec::new();
    let mut tokens = 0;
    for memory in ranked {
        if tokens + memory.tokens > cap {
            break;
        }
        tokens += memory.tokens;
        kept.push(memory);
    }
    Ok(InjectedContext {
        system_message: render(&kept, brevity),
        memories: kept,
        tokens,
    })
}

/// The walk: every memory's distance to the question from one KNN sweep, a
/// personalized PageRank seeded on the closest `top_k`, and a rank blending
/// the two. Deterministic — same brain, same question, same order.
fn recall(
    storage: &Storage,
    config: &BrainConfig,
    embedding: &[f32],
    count: usize,
) -> Result<Vec<Memory>, BrainError> {
    // One sweep answers every node's distance: closest first.
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
    // Parallel edges (a link that is also similar) add up: the pair is that
    // much harder to leave.
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

    // Restart mass sits on the seeds, proportional to how well each answers
    // the question. A question near nothing restarts evenly across the seeds.
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
    let mut order: Vec<usize> = (0..swept.len()).collect();
    let score = |index: usize| -> f64 {
        let walked = if walked_peak > 0.0 {
            mass[index] / walked_peak
        } else {
            0.0
        };
        SIMILARITY_SHARE * similarity[index] + (1.0 - SIMILARITY_SHARE) * walked
    };
    order.sort_by(|&a, &b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(swept[a].0.id.cmp(&swept[b].0.id))
    });
    let mut swept: Vec<Option<Memory>> =
        swept.into_iter().map(|(memory, _)| Some(memory)).collect();
    Ok(order
        .into_iter()
        .filter_map(|index| swept[index].take())
        .collect())
}

/// The last two turns and the new prompt, newline-joined. Injected system
/// messages are not conversation content and never feed retrieval.
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

/// The template, golden-tested byte for byte: one `### slug` section per
/// recalled note under `## Memories`, walk order, empty section omitted
/// entirely. The brevity directive is the final section; `Off` adds nothing,
/// not even the heading.
fn render(memories: &[Memory], brevity: Brevity) -> String {
    let mut sections = Vec::new();
    if !memories.is_empty() {
        let mut lines = vec!["## Memories".to_string()];
        lines.extend(
            memories
                .iter()
                .map(|memory| format!("\n### {}\n{}", memory.slug, memory.content)),
        );
        sections.push(lines.join("\n"));
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

    /// Always embeds to a unit vector on `axis`, so tests choose exactly what
    /// the query lands near.
    struct FixedEmbedder {
        axis: usize,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vector(self.axis, 0.0)).collect())
        }
    }

    /// Records what it was asked to embed.
    struct Recorder(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

    impl Embedder for Recorder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0
                .borrow_mut()
                .extend(texts.iter().map(|text| text.to_string()));
            Ok(texts.iter().map(|_| vector(0, 0.0)).collect())
        }
    }

    fn config(top_k: u32, cap: u32) -> BrainConfig {
        BrainConfig {
            top_k,
            cap_tokens: cap,
            ..BrainConfig::default()
        }
    }

    fn open(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        (dir, storage)
    }

    /// Three notes: `cern-trip` at the query's axis, `espresso-order` leaning
    /// off it, `yaml-grudge` further still.
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
        // No trigger: the message passes through untouched.
        assert_eq!(
            parse_ask("my brainstorm about /brains"),
            Ask {
                message: "my brainstorm about /brains".to_string(),
                query: "my brainstorm about /brains".to_string(),
                recall: false,
            }
        );
        // Only the trigger: recall runs on history alone, and the message
        // survives non-empty.
        assert_eq!(
            parse_ask("  /brain  "),
            Ask {
                message: "/brain".to_string(),
                query: String::new(),
                recall: true,
            }
        );
    }

    #[test]
    fn the_template_is_byte_identical_to_the_plan() {
        let (_dir, storage) = seeded("golden");
        let context = build_context(
            &storage,
            &config(6, 900),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");

        assert_eq!(
            context.system_message,
            "## Memories\n\
             \n\
             ### cern-trip\n\
             went to CERN in june\n\
             \n\
             ### espresso-order\n\
             likes espresso\n\
             \n\
             ### yaml-grudge\n\
             hates yaml"
        );
        assert_eq!(context.tokens, 5 + 4 + 3);
        assert_eq!(context.memory_ids(), vec![1, 2, 3]);

        let again = build_context(
            &storage,
            &config(6, 900),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("rebuild");
        assert_eq!(again, context, "same inputs must rebuild identically");
    }

    /// The walk's whole point: a note far from the question but wikilinked to
    /// the best answer outranks an equally far stranger.
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
            &storage,
            &config(3, 900),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        let slugs: Vec<&str> = context
            .memories
            .iter()
            .map(|memory| memory.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["anchor", "sidekick", "stranger"]);
    }

    #[test]
    fn the_cap_truncates_exactly_where_the_budget_runs_out() {
        let (_dir, storage) = seeded("cap");
        // cern-trip (5) + espresso-order (4) fit a cap of 9 exactly.
        let context = build_context(
            &storage,
            &config(6, 9),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memories.len(), 2);
        assert_eq!(context.tokens, 9);
        // A cap of 8 cuts at the first memory that no longer fits.
        let context = build_context(
            &storage,
            &config(6, 8),
            &[],
            "q",
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
        let context =
            build_context(&storage, &config(6, 900), &[], "q", Brevity::Off, never).expect("build");
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
            &storage,
            &config(6, 900),
            &history,
            "now",
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
            &storage,
            &config(6, 900),
            &[],
            "cern?",
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

    /// C.1 golden: the directive is the final section under `## Style`, and
    /// `Off` output carries no style section at all.
    #[test]
    fn each_brevity_level_appends_exactly_its_style_section() {
        let (_dir, storage) = seeded("brevity");
        let base = build_context(
            &storage,
            &config(6, 900),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("off")
        .system_message;
        for level in [Brevity::Lite, Brevity::Full, Brevity::Ultra] {
            let message =
                build_context(&storage, &config(6, 900), &[], "cern?", level, at_axis_zero)
                    .expect("build")
                    .system_message;
            let directive = level.directive().expect("directive");
            assert_eq!(message, format!("{base}\n\n## Style\n{directive}"));
        }

        // No recall still gets its style section, with nothing above it.
        let message = empty_context(Brevity::Ultra).system_message;
        assert_eq!(
            message,
            format!("## Style\n{}", Brevity::Ultra.directive().expect("ultra"))
        );
        assert_eq!(empty_context(Brevity::Off).system_message, "");
    }

    /// The folder-to-index pipeline end to end: files in, sync embeds only
    /// what changed, and an unchanged folder never touches the model.
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

        // A deleted file prunes without the model: nothing needs embedding.
        crate::notes::delete_note(&brain.0, "second-note").expect("delete");
        assert!(sync(&storage, &config, never).expect("prune"));
        assert_eq!(storage.count_memories().expect("count"), 1);
    }
}
