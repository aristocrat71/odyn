//! The tool loop: one user turn that may span several provider requests.
//!
//! Tools are offered only when the message asked for them (`/memory`), and the
//! only one is `save_memory`. The index is not touched here — the folder is the
//! truth, and the next recall or preview syncs it.

use std::path::Path;

use futures::StreamExt;

use crate::chat::{
    ChatError, ChatEvent, ChatProvider, ChatRequest, Message, ToolCall, ToolDef, Usage,
};
use crate::notes::{self, NotesError};

pub const SAVE_MEMORY: &str = "save_memory";

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
}

pub struct TurnReply {
    pub text: String,
    pub usage: Option<Usage>,
    /// Slugs of the notes saved this turn, in save order.
    pub saved: Vec<String>,
}

pub fn save_memory_tool() -> ToolDef {
    ToolDef {
        name: SAVE_MEMORY.to_string(),
        description: "Save one new memory to the user's personal notes.".to_string(),
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
                    "description": "Optional short kebab-case name for the note."
                }
            },
            "required": ["content"]
        }),
    }
}

/// Streams, runs any tool calls, follows up with the results, and repeats until
/// the model answers in text. An empty `tools` slice makes this exactly one
/// request. Every delta and save reaches `emit` as it happens.
pub async fn run_turn(
    provider: &dyn ChatProvider,
    model: &str,
    mut messages: Vec<Message>,
    tools: &[ToolDef],
    brain_dir: &Path,
    mut emit: impl FnMut(TurnEvent<'_>) -> std::io::Result<()>,
) -> Result<TurnReply, TurnError> {
    let mut text = String::new();
    let mut usage: Option<Usage> = None;
    let mut saved = Vec::new();
    for _ in 0..=MAX_TOOL_ROUNDS {
        let mut round_text = String::new();
        let mut calls = Vec::new();
        {
            let mut req = ChatRequest::new(&messages, model);
            req.tools = tools;
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
        for call in calls {
            let (result, slug) = run_tool(brain_dir, &call);
            if let Some(slug) = slug {
                emit(TurnEvent::Saved(&slug)).map_err(TurnError::Write)?;
                saved.push(slug);
            }
            messages.push(Message::tool_result(call, result));
        }
    }
    Ok(TurnReply { text, usage, saved })
}

/// Answers the call with a result the model can read; a bad call gets its error
/// the same way, never a failed turn.
fn run_tool(brain_dir: &Path, call: &ToolCall) -> (String, Option<String>) {
    if call.name != SAVE_MEMORY {
        return (format!("error: no tool named `{}`", call.name), None);
    }
    let Some(content) = call
        .arguments
        .get("content")
        .and_then(serde_json::Value::as_str)
        .filter(|content| !content.trim().is_empty())
    else {
        return (
            "error: save_memory needs a non-empty string `content`".to_string(),
            None,
        );
    };
    let slug = call
        .arguments
        .get("slug")
        .and_then(serde_json::Value::as_str)
        .filter(|slug| !slug.trim().is_empty());
    let written = match notes::write_note(brain_dir, slug, content) {
        // A taken name derives a new slug rather than overwriting a note.
        Err(NotesError::Exists(_)) => notes::write_note(brain_dir, None, content),
        written => written,
    };
    match written {
        Ok(slug) => (format!("saved as {slug}"), Some(slug)),
        Err(err) => (format!("error: {err}"), None),
    }
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

    /// Records every request's messages and tool count.
    struct Scripted {
        rounds: Mutex<Vec<Vec<Result<ChatEvent, ChatError>>>>,
        requests: Mutex<Vec<(Vec<Message>, usize)>>,
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
            self.requests
                .lock()
                .expect("record request")
                .push((req.messages.to_vec(), req.tools.len()));
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
        ToolCall {
            id: "call_1".to_string(),
            name: SAVE_MEMORY.to_string(),
            arguments,
        }
    }

    #[test]
    fn a_save_call_writes_the_note_and_the_model_hears_the_result() {
        let dir = TempDir::new("save");
        let asked = call(serde_json::json!({
            "content": "Mitul takes espresso, no sugar.",
            "slug": "espresso"
        }));
        let provider = Scripted::new(vec![
            vec![Ok(ChatEvent::ToolCall(asked.clone())), done()],
            vec![Ok(ChatEvent::TextDelta("Saved.".to_string())), done()],
        ]);
        let messages = vec![Message::new(Role::User, "remember my espresso order")];
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            messages.clone(),
            &[save_memory_tool()],
            &dir.0,
            |event| {
                seen.push(match event {
                    TurnEvent::Delta(delta) => format!("delta:{delta}"),
                    TurnEvent::Saved(slug) => format!("saved:{slug}"),
                });
                Ok(())
            },
        ))
        .expect("turn");

        assert_eq!(reply.text, "Saved.");
        assert_eq!(reply.saved, vec!["espresso".to_string()]);
        assert_eq!(
            reply.usage,
            Some(Usage {
                input_tokens: 20,
                output_tokens: 10,
            }),
            "usage sums across rounds"
        );
        assert_eq!(seen, vec!["saved:espresso", "delta:Saved."]);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("espresso.md")).expect("note"),
            "Mitul takes espresso, no sugar.\n"
        );

        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1, 1, "tools offered on the first request");
        let followup = &requests[1].0;
        assert_eq!(followup.len(), 3);
        assert_eq!(followup[1], Message::tool_request("", vec![asked.clone()]));
        assert_eq!(
            followup[2],
            Message::tool_result(asked, "saved as espresso")
        );
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
        let (result, slug) = run_tool(
            &dir.0,
            &call(serde_json::json!({"content": "Fresh note.", "slug": "espresso"})),
        );
        assert_eq!(slug.as_deref(), Some("fresh-note"));
        assert_eq!(result, "saved as fresh-note");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("espresso.md")).expect("kept"),
            "already here\n"
        );
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
            |_| Ok(()),
        ))
        .expect("turn");
        assert_eq!(reply.text, "hi");
        assert!(reply.saved.is_empty());
        assert_eq!(provider.requests.lock().expect("requests").len(), 1);
    }
}
