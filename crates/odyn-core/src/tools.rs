//! The tool loop: one user turn that may span several provider requests.
//!
//! Tools are offered only when the message asked for them: `/memory` earns
//! `save_memory`, `/update-memory` earns `update_memory`, `/delete-memory`
//! earns `delete_memory`, `/link-memory` earns `link_memory`. One tool per
//! trigger — small models misroute a choice between tools, so the user makes
//! it. The index is not touched here — the folder is the truth, and the next
//! recall or preview syncs it.

use std::path::Path;

use futures::StreamExt;

use crate::chat::{
    ChatError, ChatEvent, ChatProvider, ChatRequest, Message, ToolCall, ToolDef, Usage,
};
use crate::notes::{self, NotesError};

pub const SAVE_MEMORY: &str = "save_memory";
pub const UPDATE_MEMORY: &str = "update_memory";
pub const DELETE_MEMORY: &str = "delete_memory";
pub const LINK_MEMORY: &str = "link_memory";

/// A model that keeps asking for tools is looping, not working.
const MAX_TOOL_ROUNDS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Chat(#[from] ChatError),
    #[error("could not write the reply: {0}")]
    Write(std::io::Error),
}

/// What the surfaces render as it happens.
pub enum TurnEvent<'a> {
    Delta(&'a str),
    Saved(&'a str),
    Updated(&'a str),
    Deleted(&'a str),
    Linked { from: &'a str, to: &'a str },
}

pub struct TurnReply {
    pub text: String,
    pub usage: Option<Usage>,
    /// Slugs of the notes saved this turn, in save order.
    pub saved: Vec<String>,
    /// Slugs of the notes rewritten this turn, in call order.
    pub updated: Vec<String>,
    /// Slugs of the notes trashed this turn, in call order.
    pub deleted: Vec<String>,
    /// `(from, to)` pairs connected this turn, in call order.
    pub linked: Vec<(String, String)>,
}

pub fn save_memory_tool() -> ToolDef {
    ToolDef {
        name: SAVE_MEMORY.to_string(),
        description: "Save one new memory to the user's personal notes. Only \
                      for a fact no memory covers yet — never a second note \
                      about a topic that already has one."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The memory as a short markdown note in third person; \
                                    [[slug]] links a related existing memory."
                },
                "slug": {
                    "type": "string",
                    "description": "Optional short kebab-case name for the note's \
                                    subject, never its current value: car-keys, \
                                    not car-keys-on-desk."
                }
            },
            "required": ["content"]
        }),
    }
}

/// The tools a turn's mentions earn. Update leads when several are offered: a
/// misrouted update errors and self-corrects, a misrouted save duplicates.
pub fn offered(memorize: bool, update: bool, delete: bool, link: bool) -> Vec<ToolDef> {
    let mut tools = Vec::new();
    if update {
        tools.push(update_memory_tool());
    }
    if memorize {
        tools.push(save_memory_tool());
    }
    if delete {
        tools.push(delete_memory_tool());
    }
    if link {
        tools.push(link_memory_tool());
    }
    tools
}

pub fn link_memory_tool() -> ToolDef {
    ToolDef {
        name: LINK_MEMORY.to_string(),
        description: "Connect two existing memories so the brain relates them.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "string",
                    "description": "The memory the link is written into — its \
                                    exact slug from the ### heading above."
                },
                "to": {
                    "type": "string",
                    "description": "The memory it points at — its exact slug \
                                    from the ### heading above."
                }
            },
            "required": ["from", "to"]
        }),
    }
}

pub fn delete_memory_tool() -> ToolDef {
    ToolDef {
        name: DELETE_MEMORY.to_string(),
        description: "Delete one memory the user asked to forget.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The memory to delete — its exact slug from \
                                    the ### heading above."
                }
            },
            "required": ["slug"]
        }),
    }
}

pub fn update_memory_tool() -> ToolDef {
    ToolDef {
        name: UPDATE_MEMORY.to_string(),
        description: "Rewrite one existing memory whose fact changed.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "The memory to rewrite — its exact slug from \
                                    the ### heading above."
                },
                "content": {
                    "type": "string",
                    "description": "The whole replacement note; it overwrites \
                                    the old content."
                }
            },
            "required": ["slug", "content"]
        }),
    }
}

/// Streams, runs any tool calls, follows up with the results, and repeats
/// until the model answers in text — except after a round whose calls all
/// succeeded, which ends the turn on a synthesized confirmation instead of
/// another request. An empty `tools` slice makes this exactly one request at
/// the provider's default temperature; tool turns sample at `temperature`
/// instead (`brain.save_temperature`) — saving is transcription, not
/// creativity. Every delta and write reaches `emit` as it happens.
pub async fn run_turn(
    provider: &dyn ChatProvider,
    model: &str,
    mut messages: Vec<Message>,
    tools: &[ToolDef],
    brain_dir: &Path,
    temperature: f32,
    mut emit: impl FnMut(TurnEvent<'_>) -> std::io::Result<()>,
) -> Result<TurnReply, TurnError> {
    let mut text = String::new();
    let mut usage: Option<Usage> = None;
    let mut saved = Vec::new();
    let mut updated = Vec::new();
    let mut deleted = Vec::new();
    let mut linked = Vec::new();
    for _ in 0..=MAX_TOOL_ROUNDS {
        let mut round_text = String::new();
        let mut calls = Vec::new();
        {
            let mut req = ChatRequest::new(&messages, model);
            req.tools = tools;
            if !tools.is_empty() {
                req.temperature = Some(temperature);
            }
            let mut stream = provider.chat_stream(req);
            while let Some(event) = stream.next().await {
                match event? {
                    ChatEvent::TextDelta(delta) => {
                        emit(TurnEvent::Delta(&delta)).map_err(TurnError::Write)?;
                        round_text.push_str(&delta);
                    }
                    ChatEvent::ToolCall(call) => calls.push(call),
                    ChatEvent::Done { usage: reported } => {
                        usage = add_usage(usage, reported);
                        break;
                    }
                }
            }
        }
        text.push_str(&round_text);
        if calls.is_empty() {
            break;
        }
        messages.push(Message::tool_request(round_text, calls.clone()));
        let mut clean = Vec::new();
        let mut failed = false;
        for call in calls {
            let (result, written) = run_tool(brain_dir, &call);
            match written {
                Some(Written::Saved(slug)) => {
                    emit(TurnEvent::Saved(&slug)).map_err(TurnError::Write)?;
                    saved.push(slug);
                    clean.push(result.clone());
                }
                Some(Written::Updated(slug)) => {
                    emit(TurnEvent::Updated(&slug)).map_err(TurnError::Write)?;
                    updated.push(slug);
                    clean.push(result.clone());
                }
                Some(Written::Deleted(slug)) => {
                    emit(TurnEvent::Deleted(&slug)).map_err(TurnError::Write)?;
                    deleted.push(slug);
                    clean.push(result.clone());
                }
                Some(Written::Linked(from, to)) => {
                    emit(TurnEvent::Linked {
                        from: &from,
                        to: &to,
                    })
                    .map_err(TurnError::Write)?;
                    linked.push((from, to));
                    clean.push(result.clone());
                }
                None => failed = true,
            }
            messages.push(Message::tool_result(call, result));
        }
        // A failed call keeps the loop so the model can read the error and
        // retry. A clean round of writes ends the turn on Odyn's own
        // confirmation: handing a small model another round over the recalled
        // notes invites it to recite them instead of confirming.
        if failed || clean.is_empty() {
            continue;
        }
        let confirmation = if text.is_empty() {
            clean.join(" · ")
        } else {
            format!("\n{}", clean.join(" · "))
        };
        emit(TurnEvent::Delta(&confirmation)).map_err(TurnError::Write)?;
        text.push_str(&confirmation);
        break;
    }
    Ok(TurnReply {
        text,
        usage,
        saved,
        updated,
        deleted,
        linked,
    })
}

/// What a call did to the folder, for the trace events.
enum Written {
    Saved(String),
    Updated(String),
    Deleted(String),
    Linked(String, String),
}

/// Answers the call with a result the model can read; a bad call gets its error
/// the same way, never a failed turn.
fn run_tool(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    match call.name.as_str() {
        SAVE_MEMORY => save(brain_dir, call),
        UPDATE_MEMORY => update(brain_dir, call),
        DELETE_MEMORY => delete(brain_dir, call),
        LINK_MEMORY => link(brain_dir, call),
        other => (format!("error: no tool named `{other}`"), None),
    }
}

fn save(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    let Some(content) = text_arg(call, "content") else {
        return (
            "error: save_memory needs a non-empty string `content`".to_string(),
            None,
        );
    };
    let written = match notes::write_note(brain_dir, text_arg(call, "slug"), content) {
        // A taken name derives a new slug rather than overwriting a note.
        Err(NotesError::Exists(_)) => notes::write_note(brain_dir, None, content),
        written => written,
    };
    match written {
        Ok(slug) => (format!("saved as {slug}"), Some(Written::Saved(slug))),
        Err(err) => (format!("error: {err}"), None),
    }
}

fn update(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    let (Some(slug), Some(content)) = (text_arg(call, "slug"), text_arg(call, "content")) else {
        return (
            "error: update_memory needs non-empty strings `slug` and `content`".to_string(),
            None,
        );
    };
    match notes::update_note(brain_dir, slug, content) {
        Ok(()) => (
            format!("updated {slug}"),
            Some(Written::Updated(slug.to_string())),
        ),
        Err(err) => (format!("error: {err}"), None),
    }
}

/// Trashes rather than removes: `notes::trash_note` keeps the file under
/// `.trash/`, so a wrong slug from a small model is recoverable.
fn delete(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    let Some(slug) = text_arg(call, "slug") else {
        return (
            "error: delete_memory needs a non-empty string `slug`".to_string(),
            None,
        );
    };
    match notes::trash_note(brain_dir, slug) {
        Ok(()) => (
            format!("deleted {slug}"),
            Some(Written::Deleted(slug.to_string())),
        ),
        Err(err) => (format!("error: {err}"), None),
    }
}

/// An already-linked pair is a success, not an error: the turn ends on a
/// confirmation rather than sending the model back to try again.
fn link(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    let (Some(from), Some(to)) = (text_arg(call, "from"), text_arg(call, "to")) else {
        return (
            "error: link_memory needs non-empty strings `from` and `to`".to_string(),
            None,
        );
    };
    match notes::link_note(brain_dir, from, to) {
        Ok(added) => (
            if added {
                format!("linked {from} to {to}")
            } else {
                format!("{from} already links to {to}")
            },
            Some(Written::Linked(from.to_string(), to.to_string())),
        ),
        Err(err) => (format!("error: {err}"), None),
    }
}

fn text_arg<'a>(call: &'a ToolCall, name: &str) -> Option<&'a str> {
    call.arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn add_usage(total: Option<Usage>, reported: Option<Usage>) -> Option<Usage> {
    match (total, reported) {
        (Some(a), Some(b)) => Some(Usage {
            input_tokens: a.input_tokens + b.input_tokens,
            output_tokens: a.output_tokens + b.output_tokens,
        }),
        (total, reported) => total.or(reported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Role;
    use futures::executor::block_on;
    use futures::stream::{self, BoxStream};
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "odyn-tools-{}-{label}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Messages, tool count and temperature of one recorded request.
    type Recorded = (Vec<Message>, usize, Option<f32>);

    struct Scripted {
        rounds: Mutex<Vec<Vec<Result<ChatEvent, ChatError>>>>,
        requests: Mutex<Vec<Recorded>>,
    }

    impl Scripted {
        fn new(rounds: Vec<Vec<Result<ChatEvent, ChatError>>>) -> Self {
            Self {
                rounds: Mutex::new(rounds),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatProvider for Scripted {
        fn chat_stream<'a>(
            &'a self,
            req: ChatRequest<'a>,
        ) -> BoxStream<'a, Result<ChatEvent, ChatError>> {
            self.requests.lock().expect("record request").push((
                req.messages.to_vec(),
                req.tools.len(),
                req.temperature,
            ));
            let mut rounds = self.rounds.lock().expect("next round");
            let events = if rounds.is_empty() {
                Vec::new()
            } else {
                rounds.remove(0)
            };
            stream::iter(events).boxed()
        }
    }

    fn done() -> Result<ChatEvent, ChatError> {
        Ok(ChatEvent::Done {
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
            }),
        })
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        named_call(SAVE_MEMORY, arguments)
    }

    fn named_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    fn slug_of(written: Option<Written>) -> Option<String> {
        written.map(|written| match written {
            Written::Saved(slug) | Written::Updated(slug) | Written::Deleted(slug) => slug,
            Written::Linked(from, to) => format!("{from}->{to}"),
        })
    }

    fn label(event: TurnEvent<'_>) -> String {
        match event {
            TurnEvent::Delta(delta) => format!("delta:{delta}"),
            TurnEvent::Saved(slug) => format!("saved:{slug}"),
            TurnEvent::Updated(slug) => format!("updated:{slug}"),
            TurnEvent::Deleted(slug) => format!("deleted:{slug}"),
            TurnEvent::Linked { from, to } => format!("linked:{from}->{to}"),
        }
    }

    /// A clean write ends the turn on Odyn's own confirmation: one request,
    /// and the model never speaks over the recalled notes again.
    #[test]
    fn a_save_call_writes_the_note_and_ends_the_turn_with_its_own_confirmation() {
        let dir = TempDir::new("save");
        let asked = call(serde_json::json!({
            "content": "Mitul takes espresso, no sugar.",
            "slug": "espresso"
        }));
        let provider = Scripted::new(vec![vec![Ok(ChatEvent::ToolCall(asked.clone())), done()]]);
        let messages = vec![Message::new(Role::User, "remember my espresso order")];
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            messages.clone(),
            &[save_memory_tool()],
            &dir.0,
            0.3,
            |event| {
                seen.push(label(event));
                Ok(())
            },
        ))
        .expect("turn");

        assert_eq!(reply.text, "saved as espresso");
        assert_eq!(reply.saved, vec!["espresso".to_string()]);
        assert!(reply.updated.is_empty());
        assert_eq!(
            reply.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
            })
        );
        assert_eq!(seen, vec!["saved:espresso", "delta:saved as espresso"]);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("espresso.md")).expect("note"),
            "Mitul takes espresso, no sugar.\n"
        );

        let requests = provider.requests.lock().expect("requests");
        assert_eq!(
            requests.len(),
            1,
            "a clean write asks the model nothing more"
        );
        assert_eq!(requests[0].1, 1, "tools offered on the first request");
        assert_eq!(requests[0].2, Some(0.3));
    }

    /// One success and one failure in a round: the loop continues so the model
    /// can read the error, and the clean write is not double-confirmed.
    #[test]
    fn a_failed_call_in_a_round_keeps_the_model_in_the_loop() {
        let dir = TempDir::new("mixed");
        let good = call(serde_json::json!({
            "content": "Espresso, no sugar.",
            "slug": "espresso"
        }));
        let bad = named_call(
            UPDATE_MEMORY,
            serde_json::json!({"slug": "ghost", "content": "x"}),
        );
        let provider = Scripted::new(vec![
            vec![
                Ok(ChatEvent::ToolCall(good)),
                Ok(ChatEvent::ToolCall(bad)),
                done(),
            ],
            vec![
                Ok(ChatEvent::TextDelta(
                    "There is no note about that to update.".to_string(),
                )),
                done(),
            ],
        ]);
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "remember and update")],
            &[save_memory_tool(), update_memory_tool()],
            &dir.0,
            0.3,
            |_| Ok(()),
        ))
        .expect("turn");

        assert_eq!(reply.text, "There is no note about that to update.");
        assert_eq!(reply.saved, vec!["espresso".to_string()]);
        assert!(reply.updated.is_empty());
        assert_eq!(provider.requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn a_bad_call_answers_with_an_error_instead_of_failing_the_turn() {
        let dir = TempDir::new("bad");
        let provider = Scripted::new(vec![
            vec![
                Ok(ChatEvent::ToolCall(call(serde_json::json!({"slug": "x"})))),
                done(),
            ],
            vec![
                Ok(ChatEvent::TextDelta("I could not save that.".to_string())),
                done(),
            ],
        ]);
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "/memory")],
            &[save_memory_tool()],
            &dir.0,
            0.3,
            |_| Ok(()),
        ))
        .expect("turn");

        assert!(reply.saved.is_empty());
        assert_eq!(std::fs::read_dir(&dir.0).expect("dir").count(), 0);
        let requests = provider.requests.lock().expect("requests");
        let result = &requests[1].0[2];
        assert_eq!(result.role, Role::Tool);
        assert!(result.content.starts_with("error:"), "{}", result.content);
    }

    #[test]
    fn a_taken_slug_falls_back_to_a_derived_one() {
        let dir = TempDir::new("taken");
        notes::write_note(&dir.0, Some("espresso"), "already here").expect("seed");
        let (result, written) = run_tool(
            &dir.0,
            &call(serde_json::json!({"content": "Fresh note.", "slug": "espresso"})),
        );
        assert_eq!(slug_of(written).as_deref(), Some("fresh-note"));
        assert_eq!(result, "saved as fresh-note");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("espresso.md")).expect("kept"),
            "already here\n"
        );
    }

    /// Model text that preceded the call keeps its place; the synthesized
    /// confirmation joins it on its own line.
    #[test]
    fn an_update_call_rewrites_the_note_in_place() {
        let dir = TempDir::new("update");
        notes::write_note(&dir.0, Some("car-keys"), "Car keys are on the desk.").expect("seed");
        let asked = named_call(
            UPDATE_MEMORY,
            serde_json::json!({"slug": "car-keys", "content": "Car keys are above the fridge."}),
        );
        let provider = Scripted::new(vec![vec![
            Ok(ChatEvent::TextDelta("Rewriting.".to_string())),
            Ok(ChatEvent::ToolCall(asked.clone())),
            done(),
        ]]);
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "my keys moved")],
            &[save_memory_tool(), update_memory_tool()],
            &dir.0,
            0.3,
            |event| {
                seen.push(label(event));
                Ok(())
            },
        ))
        .expect("turn");

        assert!(reply.saved.is_empty());
        assert_eq!(reply.updated, vec!["car-keys".to_string()]);
        assert_eq!(reply.text, "Rewriting.\nupdated car-keys");
        assert_eq!(
            seen,
            vec![
                "delta:Rewriting.",
                "updated:car-keys",
                "delta:\nupdated car-keys"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("car-keys.md")).expect("note"),
            "Car keys are above the fridge.\n"
        );
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(
            requests.len(),
            1,
            "a clean write asks the model nothing more"
        );
    }

    #[test]
    fn a_delete_call_trashes_the_note_and_reports_it() {
        let dir = TempDir::new("delete");
        notes::write_note(&dir.0, Some("car-keys"), "on the desk").expect("seed");
        let (result, written) = run_tool(
            &dir.0,
            &named_call(DELETE_MEMORY, serde_json::json!({"slug": "car-keys"})),
        );
        assert_eq!(slug_of(written).as_deref(), Some("car-keys"));
        assert_eq!(result, "deleted car-keys");
        assert!(!dir.0.join("car-keys.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join(".trash").join("car-keys.md")).expect("kept"),
            "on the desk\n"
        );

        let (result, written) = run_tool(
            &dir.0,
            &named_call(DELETE_MEMORY, serde_json::json!({"slug": "ghost"})),
        );
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
    }

    /// A second call for a pair that is already connected still confirms: the
    /// folder is where it ends up that matters, not who wrote the link.
    #[test]
    fn a_link_call_connects_the_pair_and_repeats_are_not_errors() {
        let dir = TempDir::new("link");
        notes::write_note(&dir.0, Some("football"), "He plays on Sundays.").expect("seed");
        notes::write_note(&dir.0, Some("mitul"), "The user.").expect("seed");
        let asked = named_call(
            LINK_MEMORY,
            serde_json::json!({"from": "football", "to": "mitul"}),
        );
        let provider = Scripted::new(vec![vec![Ok(ChatEvent::ToolCall(asked)), done()]]);
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "connect those two")],
            &[link_memory_tool()],
            &dir.0,
            0.3,
            |event| {
                seen.push(label(event));
                Ok(())
            },
        ))
        .expect("turn");

        assert_eq!(
            reply.linked,
            vec![("football".to_string(), "mitul".to_string())]
        );
        assert_eq!(reply.text, "linked football to mitul");
        assert_eq!(
            seen,
            vec!["linked:football->mitul", "delta:linked football to mitul"]
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("football.md")).expect("note"),
            "He plays on Sundays.\n\nSee also [[mitul]].\n"
        );

        let (result, written) = run_tool(
            &dir.0,
            &named_call(
                LINK_MEMORY,
                serde_json::json!({"from": "football", "to": "mitul"}),
            ),
        );
        assert_eq!(slug_of(written).as_deref(), Some("football->mitul"));
        assert_eq!(result, "football already links to mitul");

        let (result, written) = run_tool(
            &dir.0,
            &named_call(
                LINK_MEMORY,
                serde_json::json!({"from": "ghost", "to": "mitul"}),
            ),
        );
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
    }

    #[test]
    fn an_update_of_a_missing_note_answers_with_an_error() {
        let dir = TempDir::new("update-missing");
        let (result, written) = run_tool(
            &dir.0,
            &named_call(
                UPDATE_MEMORY,
                serde_json::json!({"slug": "nowhere", "content": "anything"}),
            ),
        );
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
        assert_eq!(std::fs::read_dir(&dir.0).expect("dir").count(), 0);
    }

    #[test]
    fn no_tools_means_exactly_one_request() {
        let dir = TempDir::new("plain");
        let provider = Scripted::new(vec![vec![
            Ok(ChatEvent::TextDelta("hi".to_string())),
            done(),
        ]]);
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "hello")],
            &[],
            &dir.0,
            0.3,
            |_| Ok(()),
        ))
        .expect("turn");
        assert_eq!(reply.text, "hi");
        assert!(reply.saved.is_empty());
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].2, None, "plain turns keep the provider default");
    }
}
