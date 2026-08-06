//! The one gate every injected memory token passes through.
//!
//! `build_context` is the only producer of memory context in Odyn. The CLI,
//! the GUI, `--show-context` and the composer ledger all render its output —
//! never their own — which is what keeps the ledger equal to reality.

use crate::brevity::Brevity;
use crate::chat::{Message, Role};
use crate::config::MemoryConfig;
use crate::embed::{EmbedError, Embedder};
use crate::storage::{Memory, MemoryTier, Storage, StorageError};

/// Two turns of history join the retrieval query.
const QUERY_MESSAGES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedContext {
    pub core: Vec<Memory>,
    /// Retrieval order: closest first, truncated at the episodic token cap.
    pub episodic: Vec<Memory>,
    /// The exact system message the model sees; empty when nothing is injected.
    pub system_message: String,
    pub core_tokens: i64,
    pub episodic_tokens: i64,
    /// Core is never truncated: over budget it is injected whole and flagged,
    /// because silently dropping a core memory would falsify the ledger.
    pub core_over_budget: bool,
}

impl InjectedContext {
    pub fn is_empty(&self) -> bool {
        self.core.is_empty() && self.episodic.is_empty()
    }

    /// Injection order — core first, then episodic — for recording.
    pub fn memory_ids(&self) -> Vec<i64> {
        self.core
            .iter()
            .chain(&self.episodic)
            .map(|memory| memory.id)
            .collect()
    }
}

/// The context when there is no database to read: no memories, but the style
/// directive still applies — brevity is not memory.
pub fn empty_context(brevity: Brevity) -> InjectedContext {
    InjectedContext {
        core: Vec::new(),
        episodic: Vec::new(),
        system_message: render(&[], &[], brevity),
        core_tokens: 0,
        episodic_tokens: 0,
        core_over_budget: false,
    }
}

/// Assembles the context for the next prompt. `history` is the conversation
/// so far, without `user_msg`. The embedder is behind a loader so it is only
/// ever loaded — and on first use, downloaded — when an episodic memory
/// actually exists to retrieve.
pub fn build_context<F>(
    storage: &Storage,
    config: &MemoryConfig,
    history: &[Message],
    user_msg: &str,
    brevity: Brevity,
    load_embedder: F,
) -> Result<InjectedContext, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    let core = storage.list_memories(Some(MemoryTier::Core))?;
    let episodic = retrieve_episodic(storage, config, history, user_msg, load_embedder)?;
    let core_tokens: i64 = core.iter().map(|memory| memory.tokens).sum();
    let episodic_tokens: i64 = episodic.iter().map(|memory| memory.tokens).sum();
    Ok(InjectedContext {
        system_message: render(&core, &episodic, brevity),
        core_over_budget: core_tokens > i64::from(config.core_budget_tokens),
        core,
        episodic,
        core_tokens,
        episodic_tokens,
    })
}

fn retrieve_episodic<F>(
    storage: &Storage,
    config: &MemoryConfig,
    history: &[Message],
    user_msg: &str,
    load_embedder: F,
) -> Result<Vec<Memory>, BrainError>
where
    F: FnOnce() -> Result<Box<dyn Embedder>, EmbedError>,
{
    if storage.count_memories(Some(MemoryTier::Episodic))? == 0 {
        return Ok(Vec::new());
    }
    let query = query_text(history, user_msg);
    let mut embedder = load_embedder()?;
    let embedding = embedder
        .embed(&[query.as_str()])?
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError::Embed("the embedder returned no vector".to_string()))?;
    let neighbors = storage.knn_episodic(&embedding, config.episodic_top_k as usize)?;

    let cap = i64::from(config.episodic_cap_tokens);
    let mut kept = Vec::new();
    let mut total = 0;
    for (memory, _) in neighbors {
        if total + memory.tokens > cap {
            break;
        }
        total += memory.tokens;
        kept.push(memory);
    }
    Ok(kept)
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
    parts.push(user_msg);
    parts.join("\n")
}

/// The exact template from the plan: golden-tested byte for byte. A blank
/// line between sections, one `- [id] content` line per memory in retrieval
/// order, empty sections omitted entirely. The brevity directive is the final
/// section; `Off` adds nothing, not even the heading.
fn render(core: &[Memory], episodic: &[Memory], brevity: Brevity) -> String {
    let mut sections = Vec::new();
    if !core.is_empty() {
        sections.push(section("## Core profile", core));
    }
    if !episodic.is_empty() {
        sections.push(section("## Relevant memories", episodic));
    }
    if let Some(directive) = brevity.directive() {
        sections.push(format!("## Style\n{directive}"));
    }
    sections.join("\n\n")
}

fn section(header: &str, memories: &[Memory]) -> String {
    let mut lines = vec![header.to_string()];
    lines.extend(
        memories
            .iter()
            .map(|memory| format!("- [{}] {}", memory.display_id(), memory.content)),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{FakeEmbedder, EMBEDDING_DIM};
    use crate::storage::tests::TempDir;

    /// Always embeds to a unit vector on `axis`, leaning `lean` towards the
    /// next axis, so tests choose exactly what the query lands near.
    struct FixedEmbedder {
        axis: usize,
        lean: f32,
    }

    impl Embedder for FixedEmbedder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vector(self.axis, self.lean)).collect())
        }
    }

    /// Records what it was asked to embed.
    struct Recorder(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

    impl Embedder for Recorder {
        fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0
                .borrow_mut()
                .extend(texts.iter().map(|text| text.to_string()));
            FakeEmbedder.embed(texts)
        }
    }

    fn vector(axis: usize, lean: f32) -> Vec<f32> {
        let mut values = vec![0.0f32; EMBEDDING_DIM];
        values[axis] = 1.0 - lean;
        values[axis + 1] = lean;
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut values {
            *value /= norm;
        }
        values
    }

    fn config(top_k: u32, cap: u32, budget: u32) -> MemoryConfig {
        MemoryConfig {
            core_budget_tokens: budget,
            episodic_top_k: top_k,
            episodic_cap_tokens: cap,
            ..MemoryConfig::default()
        }
    }

    fn open(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        (dir, storage)
    }

    /// Two cores + three episodic clusters around the query axis.
    fn seeded(label: &str) -> (TempDir, Storage) {
        let (dir, storage) = open(label);
        storage
            .add_memory(MemoryTier::Core, "name is Mitul", None)
            .expect("c1");
        storage
            .add_memory(MemoryTier::Core, "prefers terse replies", None)
            .expect("c2");
        storage
            .add_memory(MemoryTier::Episodic, "went to CERN", Some(&vector(0, 0.0)))
            .expect("e3");
        storage
            .add_memory(
                MemoryTier::Episodic,
                "likes espresso",
                Some(&vector(0, 0.1)),
            )
            .expect("e4");
        storage
            .add_memory(MemoryTier::Episodic, "hates yaml", Some(&vector(0, 0.2)))
            .expect("e5");
        (dir, storage)
    }

    fn at_axis_zero() -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(FixedEmbedder { axis: 0, lean: 0.0 }))
    }

    fn never() -> Result<Box<dyn Embedder>, EmbedError> {
        Err(EmbedError::Load(
            "the embedder must not be loaded here".to_string(),
        ))
    }

    #[test]
    fn the_template_is_byte_identical_to_the_plan() {
        let (_dir, storage) = seeded("golden");
        let context = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");

        assert_eq!(
            context.system_message,
            "## Core profile\n\
             - [c-01] name is Mitul\n\
             - [c-02] prefers terse replies\n\
             \n\
             ## Relevant memories\n\
             - [e-0003] went to CERN\n\
             - [e-0004] likes espresso\n\
             - [e-0005] hates yaml"
        );
        assert_eq!(context.core_tokens, 4 + 6);
        assert_eq!(context.episodic_tokens, 3 + 4 + 3);
        assert!(!context.core_over_budget);
        assert_eq!(context.memory_ids(), vec![1, 2, 3, 4, 5]);

        let again = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("rebuild");
        assert_eq!(again, context, "same inputs must rebuild identically");
    }

    #[test]
    fn top_k_bounds_retrieval() {
        let (_dir, storage) = seeded("topk");
        let context = build_context(
            &storage,
            &config(2, 900, 500),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.memory_ids(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn the_episodic_cap_truncates_exactly_where_the_budget_runs_out() {
        let (_dir, storage) = seeded("cap");
        // e-0003 (3) + e-0004 (4) fit a cap of 7 exactly; e-0005 must not.
        let context = build_context(
            &storage,
            &config(6, 7, 500),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.episodic.len(), 2);
        assert_eq!(context.episodic_tokens, 7);
        // A cap of 6 cuts at the first memory that no longer fits.
        let context = build_context(
            &storage,
            &config(6, 6, 500),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(context.episodic.len(), 1);
        assert_eq!(context.episodic_tokens, 3);
    }

    #[test]
    fn core_over_budget_is_flagged_but_never_truncated() {
        let (_dir, storage) = seeded("budget");
        let context = build_context(
            &storage,
            &config(6, 900, 5),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert!(context.core_over_budget);
        assert_eq!(context.core.len(), 2, "core is injected whole regardless");
    }

    #[test]
    fn empty_sections_are_omitted_and_an_empty_brain_never_loads_the_embedder() {
        let (_dir, storage) = open("empty");
        let context = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "q",
            Brevity::Off,
            never,
        )
        .expect("build");
        assert!(context.is_empty());
        assert_eq!(context.system_message, "");

        storage
            .add_memory(MemoryTier::Core, "only core", None)
            .expect("core");
        let context = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "q",
            Brevity::Off,
            never,
        )
        .expect("build");
        assert_eq!(
            context.system_message,
            "## Core profile\n- [c-01] only core"
        );

        let (_dir, storage) = open("episodic-only");
        storage
            .add_memory(MemoryTier::Episodic, "only episodic", Some(&vector(0, 0.0)))
            .expect("episodic");
        let context = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "q",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("build");
        assert_eq!(
            context.system_message,
            "## Relevant memories\n- [e-0001] only episodic"
        );
    }

    #[test]
    fn the_query_is_the_last_two_turns_and_the_prompt_without_system_messages() {
        let (_dir, storage) = open("query");
        storage
            .add_memory(MemoryTier::Episodic, "anything", Some(&vector(0, 0.0)))
            .expect("episodic");
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
            &config(6, 900, 500),
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
            &config(6, 900, 500),
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
    /// `Off` output is byte-identical to the pre-addendum template.
    #[test]
    fn each_brevity_level_appends_exactly_its_style_section() {
        let (_dir, storage) = seeded("brevity");
        let base = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "cern?",
            Brevity::Off,
            at_axis_zero,
        )
        .expect("off")
        .system_message;
        assert_eq!(
            base,
            "## Core profile\n\
             - [c-01] name is Mitul\n\
             - [c-02] prefers terse replies\n\
             \n\
             ## Relevant memories\n\
             - [e-0003] went to CERN\n\
             - [e-0004] likes espresso\n\
             - [e-0005] hates yaml"
        );
        for level in [Brevity::Lite, Brevity::Full, Brevity::Ultra] {
            let message = build_context(
                &storage,
                &config(6, 900, 500),
                &[],
                "cern?",
                level,
                at_axis_zero,
            )
            .expect("build")
            .system_message;
            let directive = level.directive().expect("directive");
            assert_eq!(message, format!("{base}\n\n## Style\n{directive}"));
        }

        // An empty brain still gets its style section, with nothing above it.
        let (_dir, storage) = open("brevity-empty");
        let message = build_context(
            &storage,
            &config(6, 900, 500),
            &[],
            "q",
            Brevity::Ultra,
            never,
        )
        .expect("build")
        .system_message;
        assert_eq!(
            message,
            format!("## Style\n{}", Brevity::Ultra.directive().expect("ultra"))
        );
    }
}
