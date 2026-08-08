//! Chat types and the provider abstraction. Runtime-agnostic: no tokio here.

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool's answer to a call; lives inside one turn, never a transcript.
    Tool,
}

/// One tool the model may call; `parameters` is a JSON Schema.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A call the model made. `arguments` is whatever it produced: a bad shape is
/// answered through the tool result, not rejected here.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Empty on providers that do not id their calls (Ollama).
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// The calls an assistant message asked for, replayed on the follow-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a `Role::Tool` message answers; each dialect reads the half it
    /// understands, the id or the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<ToolCall>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            answers: None,
        }
    }

    pub fn tool_request(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: calls,
            answers: None,
        }
    }

    pub fn tool_result(call: ToolCall, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            answers: Some(call),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    TextDelta(String),
    /// Arrives before `Done`; the caller runs the tool and follows up.
    ToolCall(ToolCall),
    Done {
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ChatError {
    #[error("network error: {0}")]
    Network(String),
    #[error("api error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("parse error: {0}")]
    Parse(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    pub messages: &'a [Message],
    pub model: &'a str,
    /// Offered, not forced: an empty slice sends no tools field at all.
    pub tools: &'a [ToolDef],
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl<'a> ChatRequest<'a> {
    pub fn new(messages: &'a [Message], model: &'a str) -> Self {
        Self {
            messages,
            model,
            tools: &[],
            temperature: None,
            max_tokens: None,
        }
    }
}

pub trait ChatProvider: Send + Sync {
    fn chat_stream<'a>(
        &'a self,
        req: ChatRequest<'a>,
    ) -> BoxStream<'a, Result<ChatEvent, ChatError>>;

    /// Folds the stream into the full response text; the first error aborts it.
    fn chat_collect<'a>(
        &'a self,
        req: ChatRequest<'a>,
    ) -> BoxFuture<'a, Result<String, ChatError>> {
        let mut stream = self.chat_stream(req);
        Box::pin(async move {
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                match event? {
                    ChatEvent::TextDelta(delta) => out.push_str(&delta),
                    ChatEvent::ToolCall(_) => {}
                    ChatEvent::Done { .. } => break,
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::stream;

    struct MockProvider {
        events: Vec<Result<ChatEvent, ChatError>>,
    }

    impl ChatProvider for MockProvider {
        fn chat_stream<'a>(
            &'a self,
            _req: ChatRequest<'a>,
        ) -> BoxStream<'a, Result<ChatEvent, ChatError>> {
            stream::iter(self.events.clone()).boxed()
        }
    }

    fn req(messages: &[Message]) -> ChatRequest<'_> {
        ChatRequest::new(messages, "mock-model")
    }

    #[test]
    fn collect_folds_deltas_into_full_response() {
        let provider = MockProvider {
            events: vec![
                Ok(ChatEvent::TextDelta("Hel".into())),
                Ok(ChatEvent::TextDelta("lo, ".into())),
                Ok(ChatEvent::TextDelta("world".into())),
                Ok(ChatEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 3,
                        output_tokens: 5,
                    }),
                }),
            ],
        };
        let messages = [Message::new(Role::User, "hi")];
        let out = block_on(provider.chat_collect(req(&messages))).unwrap();
        assert_eq!(out, "Hello, world");
    }

    #[test]
    fn collect_surfaces_mid_stream_error() {
        let provider = MockProvider {
            events: vec![
                Ok(ChatEvent::TextDelta("partial".into())),
                Err(ChatError::Network("connection reset".into())),
                Ok(ChatEvent::TextDelta("never seen".into())),
            ],
        };
        let messages = [Message::new(Role::User, "hi")];
        let err = block_on(provider.chat_collect(req(&messages))).unwrap_err();
        match err {
            ChatError::Network(msg) => assert_eq!(msg, "connection reset"),
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    #[test]
    fn stream_can_be_driven_directly() {
        let provider = MockProvider {
            events: vec![
                Ok(ChatEvent::TextDelta("a".into())),
                Ok(ChatEvent::Done { usage: None }),
            ],
        };
        let messages = [Message::new(Role::User, "hi")];
        let events: Vec<_> = block_on(provider.chat_stream(req(&messages)).collect::<Vec<_>>());
        assert_eq!(events.len(), 2);
        assert!(matches!(events[1], Ok(ChatEvent::Done { usage: None })));
    }
}
