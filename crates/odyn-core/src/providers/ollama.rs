//! Ollama's native `/api/chat` streaming provider: NDJSON lines rather than
//! SSE, plus `/api/tags` for the models installed locally. Must be driven on a
//! tokio runtime with the IO driver enabled.
//!
//! No read/idle timeout on purpose: a cold model load is legitimately silent
//! for minutes before the first token.

use std::collections::VecDeque;
use std::error::Error as _;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt};
use reqwest::header::CONTENT_TYPE;

use super::openai_compat::ProviderInitError;
use crate::chat::{
    ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, ToolCall, ToolDef, Usage,
};

const DEFAULT_KEEP_ALIVE: &str = "5m";

/// Short enough that a status line never waits on a dead endpoint.
const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Reachability for a status line: any answer from `/api/tags` counts as up.
/// Never errors.
pub async fn ping(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(PING_TIMEOUT).build() else {
        return false;
    };
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    client.get(url).send().await.is_ok()
}

#[derive(Debug)]
pub struct OllamaProvider {
    client: reqwest::Client,
    /// Trailing slashes stripped, so `{base_url}/api/chat` is well formed.
    base_url: String,
    keep_alive: String,
}

/// One entry of `/api/tags`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub name: String,
    pub size_bytes: u64,
    /// `embedding`, `completion`, … Empty on daemons too old to report it.
    pub capabilities: Vec<String>,
}

impl ModelInfo {
    pub fn embeds(&self) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability == "embedding")
    }

    /// `Some(false)` only when the daemon reported capabilities and `tools`
    /// was not among them; a daemon too old to report stays unknown.
    pub fn calls_tools(&self) -> Option<bool> {
        if self.capabilities.is_empty() {
            return None;
        }
        Some(
            self.capabilities
                .iter()
                .any(|capability| capability == "tools"),
        )
    }
}

/// The installed models that can embed. An unreachable daemon is an empty
/// list, not an error: the picker still has its bundled models.
pub async fn embedding_models(base_url: &str) -> Vec<ModelInfo> {
    let Ok(provider) = OllamaProvider::new(base_url, None) else {
        return Vec::new();
    };
    let Ok(models) = provider.list_models().await else {
        return Vec::new();
    };
    models.into_iter().filter(ModelInfo::embeds).collect()
}

impl OllamaProvider {
    /// `keep_alive` is how long Ollama holds the model in RAM after a reply:
    /// `None` means `5m`, `"0"` unloads immediately (RAM-frugal mode).
    pub fn new(
        base_url: impl Into<String>,
        keep_alive: Option<String>,
    ) -> Result<Self, ProviderInitError> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ProviderInitError::EmptyBaseUrl);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| ProviderInitError::Client(err.to_string()))?;
        Ok(Self {
            client,
            base_url,
            keep_alive: keep_alive.unwrap_or_else(|| DEFAULT_KEEP_ALIVE.to_string()),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ChatError> {
        // Bounded overall: unlike a chat stream, a tags listing is never slow.
        let fetch = async {
            let response = self
                .client
                .get(format!("{}/api/tags", self.base_url))
                .send()
                .await
                .map_err(|err| transport_error(&err, &self.base_url))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| transport_error(&err, &self.base_url))?;
            Ok::<_, ChatError>((status, body))
        };
        let (status, body) = match tokio::time::timeout(Duration::from_secs(10), fetch).await {
            Err(_) => {
                return Err(ChatError::Network(format!(
                    "ollama did not answer /api/tags at {} within 10s",
                    self.base_url
                )))
            }
            Ok(result) => result?,
        };
        if !status.is_success() {
            return Err(ChatError::Api {
                status: status.as_u16(),
                message: error_message(&body),
            });
        }

        let tags: TagsResponse = serde_json::from_str(&body)
            .map_err(|err| ChatError::Parse(format!("malformed model list: {err}")))?;
        Ok(tags
            .models
            .into_iter()
            .map(|model| ModelInfo {
                name: model.name,
                size_bytes: model.size,
                capabilities: model.capabilities,
            })
            .collect())
    }
}

impl ChatProvider for OllamaProvider {
    fn chat_stream<'a>(
        &'a self,
        req: ChatRequest<'a>,
    ) -> BoxStream<'a, Result<ChatEvent, ChatError>> {
        // Serialized eagerly so the stream state borrows nothing from `req`.
        let body = match serde_json::to_vec(&ChatBody {
            model: req.model,
            messages: wire_messages(req.messages),
            stream: true,
            keep_alive: &self.keep_alive,
            tools: req.tools.iter().map(WireTool::from).collect(),
            options: Options {
                temperature: req.temperature,
                num_predict: req.max_tokens,
            },
        }) {
            Ok(body) => body,
            Err(err) => {
                let err = ChatError::Parse(format!("could not encode request: {err}"));
                return futures::stream::once(std::future::ready(Err(err))).boxed();
            }
        };

        let builder = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        // Local reasoning models put `<think>` blocks in `content` too.
        crate::reasoning::strip_reasoning(
            futures::stream::unfold(
                State::Start {
                    builder: Box::new(builder),
                    base_url: self.base_url.clone(),
                },
                step,
            )
            .boxed(),
        )
    }
}

#[derive(serde::Serialize)]
struct ChatBody<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    stream: bool,
    keep_alive: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    options: Options,
}

/// Ollama's message dialect: object arguments, tool results named by tool.
#[derive(serde::Serialize)]
struct WireMessage<'a> {
    role: Role,
    content: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct WireCall<'a> {
    function: WireFunction<'a>,
}

#[derive(serde::Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: &'a serde_json::Value,
}

#[derive(serde::Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: &'a ToolDef,
}

impl<'a> From<&'a ToolDef> for WireTool<'a> {
    fn from(tool: &'a ToolDef) -> Self {
        Self {
            kind: "function",
            function: tool,
        }
    }
}

fn wire_messages(messages: &[Message]) -> Vec<WireMessage<'_>> {
    messages
        .iter()
        .map(|message| WireMessage {
            role: message.role,
            content: &message.content,
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| WireCall {
                    function: WireFunction {
                        name: &call.name,
                        arguments: &call.arguments,
                    },
                })
                .collect(),
            tool_name: message.answers.as_ref().map(|call| call.name.as_str()),
        })
        .collect()
}

#[derive(serde::Serialize)]
struct Options {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(serde::Deserialize)]
struct StreamLine {
    #[serde(default)]
    message: Option<LineMessage>,
    #[serde(default)]
    done: bool,
    /// Ollama reports mid-stream failures in the body, at HTTP 200.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(serde::Deserialize)]
struct LineMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<LineCall>,
}

#[derive(serde::Deserialize)]
struct LineCall {
    function: LineFunction,
}

#[derive(serde::Deserialize)]
struct LineFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(serde::Deserialize)]
struct TagsModel {
    name: String,
    #[serde(default)]
    size: u64,
    /// Absent on older daemons, so nothing may key off it being present.
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

enum State {
    Start {
        builder: Box<reqwest::RequestBuilder>,
        base_url: String,
    },
    Streaming(Box<Streaming>),
    Finished,
}

struct Streaming {
    body: BoxStream<'static, Result<Vec<u8>, reqwest::Error>>,
    base_url: String,
    /// An unterminated line, kept as bytes because a chunk boundary can fall
    /// inside a multi-byte character.
    line: Vec<u8>,
    pending: VecDeque<ChatEvent>,
    /// `done: true` seen or body exhausted: drain `pending`, then end.
    terminated: bool,
    /// Held back until every delta decoded before it has been delivered: how
    /// much text survives a bad line must not depend on TCP framing.
    failure: Option<ChatError>,
}

/// Growth past this means the endpoint is not Ollama, and buffering it whole
/// would betray the RAM rules.
const MAX_LINE_BYTES: usize = 1 << 20;

impl Streaming {
    fn absorb(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(idx) = rest.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&rest[..idx]);
            rest = &rest[idx + 1..];
            let raw = std::mem::take(&mut self.line);
            self.handle_line(&raw);
            if self.terminated || self.failure.is_some() {
                return;
            }
        }
        self.line.extend_from_slice(rest);
        if self.line.len() > MAX_LINE_BYTES {
            self.failure = Some(ChatError::Parse(
                "stream line exceeded 1MiB; endpoint is not ollama".to_string(),
            ));
        }
    }

    /// Flushes a line left unterminated by a closed connection, then guarantees
    /// the stream ends with `Done`.
    fn finish(&mut self) {
        if !self.line.is_empty() {
            let raw = std::mem::take(&mut self.line);
            self.handle_line(&raw);
        }
        if !self.terminated && self.failure.is_none() {
            self.pending.push_back(ChatEvent::Done { usage: None });
            self.terminated = true;
        }
    }

    fn handle_line(&mut self, raw: &[u8]) {
        let Ok(line) = std::str::from_utf8(raw) else {
            self.failure = Some(ChatError::Parse(
                "stream contained invalid UTF-8".to_string(),
            ));
            return;
        };
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        let parsed: StreamLine = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.failure = Some(ChatError::Parse(format!("malformed stream line: {err}")));
                return;
            }
        };
        if let Some(message) = parsed.error {
            self.failure = Some(ChatError::Api {
                status: 200,
                message,
            });
            return;
        }
        if let Some(message) = parsed.message {
            // The final line carries an empty content field, not a delta.
            if let Some(content) = message.content.filter(|content| !content.is_empty()) {
                self.pending.push_back(ChatEvent::TextDelta(content));
            }
            for call in message.tool_calls {
                self.pending.push_back(ChatEvent::ToolCall(ToolCall {
                    id: String::new(),
                    name: call.function.name,
                    arguments: call.function.arguments,
                }));
            }
        }
        if parsed.done {
            let usage =
                (parsed.prompt_eval_count.is_some() || parsed.eval_count.is_some()).then(|| {
                    Usage {
                        input_tokens: parsed.prompt_eval_count.unwrap_or(0),
                        output_tokens: parsed.eval_count.unwrap_or(0),
                    }
                });
            self.pending.push_back(ChatEvent::Done { usage });
            self.terminated = true;
        }
    }
}

async fn step(mut state: State) -> Option<(Result<ChatEvent, ChatError>, State)> {
    loop {
        state = match state {
            State::Finished => return None,

            State::Start { builder, base_url } => {
                let response = match (*builder).send().await {
                    Err(err) => {
                        return Some((Err(transport_error(&err, &base_url)), State::Finished))
                    }
                    Ok(response) => response,
                };

                let status = response.status();
                if !status.is_success() {
                    let message = match response.text().await {
                        Ok(body) => error_message(&body),
                        Err(err) => format!("could not read error body: {err}"),
                    };
                    let err = ChatError::Api {
                        status: status.as_u16(),
                        message,
                    };
                    return Some((Err(err), State::Finished));
                }

                // `to_vec` copies, but avoids a `bytes` dependency just to name
                // the item type, and the lines are tiny.
                let body = response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                    .boxed();
                State::Streaming(Box::new(Streaming {
                    body,
                    base_url,
                    line: Vec::new(),
                    pending: VecDeque::new(),
                    terminated: false,
                    failure: None,
                }))
            }

            State::Streaming(mut streaming) => {
                if let Some(event) = streaming.pending.pop_front() {
                    return Some((Ok(event), State::Streaming(streaming)));
                }
                if let Some(err) = streaming.failure.take() {
                    return Some((Err(err), State::Finished));
                }
                if streaming.terminated {
                    return None;
                }

                match streaming.body.next().await {
                    Some(Ok(bytes)) => streaming.absorb(&bytes),
                    Some(Err(err)) => {
                        let err = transport_error(&err, &streaming.base_url);
                        return Some((Err(err), State::Finished));
                    }
                    None => streaming.finish(),
                }
                State::Streaming(streaming)
            }
        };
    }
}

/// A refused connection is the everyday case here (Ollama isn't running), so it
/// gets a message that says exactly that.
fn transport_error(err: &reqwest::Error, base_url: &str) -> ChatError {
    let mut message = if err.is_connect() {
        format!("ollama not reachable at {base_url}")
    } else if err.is_timeout() {
        "the request timed out".to_string()
    } else if err.is_body() || err.is_decode() {
        "the response stream ended unexpectedly".to_string()
    } else {
        "the request failed".to_string()
    };

    let mut cause: Option<&(dyn std::error::Error + 'static)> = err.source();
    let mut deepest = None;
    while let Some(current) = cause {
        deepest = Some(current);
        cause = current.source();
    }
    if let Some(deepest) = deepest {
        message.push_str(&format!(": {deepest}"));
    }
    ChatError::Network(message)
}

fn error_message(body: &str) -> String {
    const MAX_CHARS: usize = 512;
    let text = match serde_json::from_str::<ErrorBody>(body) {
        Ok(parsed) => parsed.error.trim().to_string(),
        Err(_) => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                return "no response body".to_string();
            }
            trimmed.to_string()
        }
    };
    // A wrong base_url can answer with an arbitrary page; cap what is relayed.
    if text.chars().count() <= MAX_CHARS {
        text
    } else {
        let head: String = text.chars().take(MAX_CHARS).collect();
        format!("{head}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Role;
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Three content deltas, then the final line with its token counts.
    fn ndjson_body() -> String {
        concat!(
            r#"{"model":"llama3.2:3b","created_at":"2024-10-01T12:00:00.1Z","message":{"role":"assistant","content":"Hel"},"done":false}"#,
            "\n",
            r#"{"model":"llama3.2:3b","created_at":"2024-10-01T12:00:00.2Z","message":{"role":"assistant","content":"lo, "},"done":false}"#,
            "\n",
            r#"{"model":"llama3.2:3b","created_at":"2024-10-01T12:00:00.3Z","message":{"role":"assistant","content":"wörld"},"done":false}"#,
            "\n",
            r#"{"model":"llama3.2:3b","created_at":"2024-10-01T12:00:00.4Z","message":{"role":"assistant","content":""},"#,
            r#""done_reason":"stop","done":true,"total_duration":1926000000,"load_duration":31000000,"#,
            r#""prompt_eval_count":26,"prompt_eval_duration":91000000,"eval_count":7,"eval_duration":1800000000}"#,
            "\n",
        )
        .to_string()
    }

    fn expected_events() -> Vec<ChatEvent> {
        vec![
            ChatEvent::TextDelta("Hel".to_string()),
            ChatEvent::TextDelta("lo, ".to_string()),
            ChatEvent::TextDelta("wörld".to_string()),
            ChatEvent::Done {
                usage: Some(Usage {
                    input_tokens: 26,
                    output_tokens: 7,
                }),
            },
        ]
    }

    /// One HTTP chunk per frame, so the client really sees those boundaries.
    fn chunked_response(content_type: &str, frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut pieces = vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .into_bytes()];
        for frame in frames {
            let mut piece = format!("{:x}\r\n", frame.len()).into_bytes();
            piece.extend_from_slice(frame);
            piece.extend_from_slice(b"\r\n");
            pieces.push(piece);
        }
        pieces.push(b"0\r\n\r\n".to_vec());
        pieces
    }

    fn ndjson_response(body: &str) -> Vec<Vec<u8>> {
        chunked_response("application/x-ndjson", &[body.as_bytes().to_vec()])
    }

    fn plain_response(status_line: &str, content_type: &str, body: &str) -> Vec<Vec<u8>> {
        vec![format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()]
    }

    /// Serves one connection with a canned response, returning the raw request.
    fn spawn_server(pieces: Vec<Vec<u8>>) -> (SocketAddr, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = tx.send(read_request(&mut stream));
            for piece in pieces {
                if stream.write_all(&piece).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
            let _ = stream.shutdown(Shutdown::Write);
            // Drain before dropping so the client sees a clean FIN, not a reset.
            let _ = stream.read_to_end(&mut Vec::new());
        });
        (addr, rx)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        // Byte at a time so we stop exactly at the header terminator.
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => return String::from_utf8_lossy(&head).into_owned(),
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut len = 0usize;
        for line in head.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                if let Ok(parsed) = value.trim().parse::<usize>() {
                    len = parsed;
                }
            }
        }
        let mut body = vec![0u8; len];
        if len > 0 && stream.read_exact(&mut body).is_err() {
            return head;
        }
        format!("{head}{}", String::from_utf8_lossy(&body))
    }

    fn provider_for(addr: SocketAddr, keep_alive: Option<String>) -> OllamaProvider {
        OllamaProvider::new(format!("http://{addr}"), keep_alive).expect("build provider")
    }

    fn user(text: &str) -> Vec<Message> {
        vec![Message::new(Role::User, text)]
    }

    async fn drive(
        provider: &OllamaProvider,
        messages: &[Message],
    ) -> Vec<Result<ChatEvent, ChatError>> {
        provider
            .chat_stream(ChatRequest::new(messages, "llama3.2:3b"))
            .collect()
            .await
    }

    #[test]
    fn base_url_trailing_slashes_are_stripped() {
        let provider =
            OllamaProvider::new("http://localhost:11434///", None).expect("build provider");
        assert_eq!(provider.base_url(), "http://localhost:11434");
    }

    #[test]
    fn empty_base_url_is_rejected() {
        let err = OllamaProvider::new("///", None).expect_err("empty base url");
        assert!(
            matches!(err, ProviderInitError::EmptyBaseUrl),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn streams_deltas_and_usage_from_a_recorded_ndjson_response() {
        let (addr, _rx) = spawn_server(ndjson_response(&ndjson_body()));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events: Vec<ChatEvent> = drive(&provider, &messages)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream must not error");
        assert_eq!(events, expected_events());
    }

    #[tokio::test]
    async fn lines_split_across_chunk_boundaries_yield_the_same_events() {
        // Seven-byte frames cut lines mid-JSON and `wörld` mid-character.
        let body = ndjson_body();
        let frames: Vec<Vec<u8>> = body.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
        let (addr, _rx) = spawn_server(chunked_response("application/x-ndjson", &frames));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events: Vec<ChatEvent> = drive(&provider, &messages)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream must not error");
        assert_eq!(events, expected_events());
    }

    #[tokio::test]
    async fn chat_collect_joins_the_deltas() {
        let (addr, _rx) = spawn_server(ndjson_response(&ndjson_body()));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let text = provider
            .chat_collect(ChatRequest::new(&messages, "llama3.2:3b"))
            .await
            .expect("collect");
        assert_eq!(text, "Hello, wörld");
    }

    #[tokio::test]
    async fn request_targets_api_chat_with_the_expected_body() {
        let (addr, rx) = spawn_server(ndjson_response(&ndjson_body()));
        let provider =
            OllamaProvider::new(format!("http://{addr}/"), None).expect("build provider");
        let messages = user("hi");
        let mut req = ChatRequest::new(&messages, "llama3.2:3b");
        req.temperature = Some(0.2);
        req.max_tokens = Some(64);

        let _ = provider.chat_collect(req).await.expect("collect");
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");

        assert!(
            request.starts_with("POST /api/chat HTTP/1.1\r\n"),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(body["model"], "llama3.2:3b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["keep_alive"], "5m");
        assert_eq!(body["options"]["temperature"], 0.2);
        assert_eq!(body["options"]["num_predict"], 64);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn keep_alive_zero_is_sent_and_unset_options_are_omitted() {
        let (addr, rx) = spawn_server(ndjson_response(&ndjson_body()));
        let provider = provider_for(addr, Some("0".to_string()));
        let messages = user("hi");

        let _ = provider
            .chat_collect(ChatRequest::new(&messages, "llama3.2:3b"))
            .await
            .expect("collect");
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");

        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(body["keep_alive"], "0");
        assert!(body["options"].get("temperature").is_none(), "{body}");
        assert!(body["options"].get("num_predict").is_none(), "{body}");
    }

    #[tokio::test]
    async fn stream_without_a_done_line_still_ends_with_done() {
        let body = concat!(
            r#"{"message":{"role":"assistant","content":"only"},"done":false}"#,
            "\n"
        );
        let (addr, _rx) = spawn_server(ndjson_response(body));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events: Vec<ChatEvent> = drive(&provider, &messages)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream must not error");
        assert_eq!(
            events,
            vec![
                ChatEvent::TextDelta("only".to_string()),
                ChatEvent::Done { usage: None },
            ]
        );
    }

    #[tokio::test]
    async fn error_line_maps_to_an_api_error() {
        let frames = vec![
            format!(
                "{}\n",
                r#"{"message":{"role":"assistant","content":"partial"},"done":false}"#
            )
            .into_bytes(),
            format!(
                "{}\n",
                r#"{"error":"model \"llama3.2:3b\" not found, try pulling it first"}"#
            )
            .into_bytes(),
        ];
        let (addr, _rx) = spawn_server(chunked_response("application/x-ndjson", &frames));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert!(
            matches!(&events[0], Ok(ChatEvent::TextDelta(delta)) if delta == "partial"),
            "{:?}",
            events[0]
        );
        match events.get(1) {
            Some(Err(ChatError::Api { status, message })) => {
                assert_eq!(*status, 200);
                assert_eq!(
                    message,
                    "model \"llama3.2:3b\" not found, try pulling it first"
                );
            }
            other => panic!("expected an api error, got {other:?}"),
        }
        assert_eq!(events.len(), 2, "the stream must stop after an error line");
    }

    #[tokio::test]
    async fn malformed_json_line_is_a_parse_error() {
        let frames = vec![
            format!(
                "{}\n",
                r#"{"message":{"role":"assistant","content":"ok"},"done":false}"#
            )
            .into_bytes(),
            b"{not json}\n".to_vec(),
        ];
        let (addr, _rx) = spawn_server(chunked_response("application/x-ndjson", &frames));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert!(
            matches!(&events[0], Ok(ChatEvent::TextDelta(delta)) if delta == "ok"),
            "{:?}",
            events[0]
        );
        match events.get(1) {
            Some(Err(ChatError::Parse(message))) => {
                assert!(message.contains("malformed stream line"), "{message}")
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        assert_eq!(events.len(), 2, "the stream must stop after a parse error");
    }

    #[tokio::test]
    async fn http_error_status_maps_to_an_api_error() {
        let (addr, _rx) = spawn_server(plain_response(
            "404 Not Found",
            "application/json",
            r#"{"error":"model 'nope' not found"}"#,
        ));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ChatError::Api { status, message }) => {
                assert_eq!(*status, 404);
                assert_eq!(message, "model 'nope' not found");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_models_parses_a_recorded_tags_payload() {
        let body = concat!(
            r#"{"models":[{"name":"llama3.2:3b","model":"llama3.2:3b","#,
            r#""modified_at":"2024-09-30T14:12:03.123456789+02:00","size":2019393189,"#,
            r#""digest":"a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72","#,
            r#""details":{"parent_model":"","format":"gguf","family":"llama","families":["llama"],"#,
            r#""parameter_size":"3.2B","quantization_level":"Q4_K_M"}},"#,
            r#"{"name":"qwen2.5-coder:7b","model":"qwen2.5-coder:7b","#,
            r#""modified_at":"2024-11-02T09:01:44.987654321+01:00","size":4683087519,"#,
            r#""digest":"2b0496514337b4a4b0f8c1cbd1e1cd6a1f7ba7b8f5f4a1c3d2e1b0a9f8e7d6c5","#,
            r#""details":{"parent_model":"","format":"gguf","family":"qwen2","families":["qwen2"],"#,
            r#""parameter_size":"7.6B","quantization_level":"Q4_K_M"},"#,
            r#""capabilities":["completion","tools"]}]}"#,
        );
        let (addr, rx) = spawn_server(plain_response("200 OK", "application/json", body));
        let provider = provider_for(addr, None);

        let models = provider.list_models().await.expect("list models");
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    name: "llama3.2:3b".to_string(),
                    size_bytes: 2_019_393_189,
                    capabilities: Vec::new(),
                },
                ModelInfo {
                    name: "qwen2.5-coder:7b".to_string(),
                    size_bytes: 4_683_087_519,
                    capabilities: vec!["completion".to_string(), "tools".to_string()],
                },
            ]
        );
        assert!(!models[0].embeds());
        // No capability list is unknown, never a verdict.
        assert_eq!(models[0].calls_tools(), None);
        assert_eq!(models[1].calls_tools(), Some(true));
        let chat_only = ModelInfo {
            name: "gemma:2b".to_string(),
            size_bytes: 1,
            capabilities: vec!["completion".to_string()],
        };
        assert_eq!(chat_only.calls_tools(), Some(false));
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(
            request.starts_with("GET /api/tags HTTP/1.1\r\n"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn malformed_tags_payload_is_a_parse_error() {
        let (addr, _rx) = spawn_server(plain_response("200 OK", "application/json", "{not json}"));
        let provider = provider_for(addr, None);

        match provider.list_models().await {
            Err(ChatError::Parse(message)) => {
                assert!(message.contains("malformed model list"), "{message}")
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_says_ollama_is_not_reachable() {
        // Bind then drop, so nothing is listening on a port we know is free.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        drop(listener);

        let provider = provider_for(addr, None);
        let messages = user("hi");
        let expected = format!("ollama not reachable at http://{addr}");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ChatError::Network(message)) => assert!(message.contains(&expected), "{message}"),
            other => panic!("expected a network error, got {other:?}"),
        }

        match provider.list_models().await {
            Err(ChatError::Network(message)) => assert!(message.contains(&expected), "{message}"),
            other => panic!("expected a network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_hits_api_tags_and_reports_reachability() {
        let (addr, rx) = spawn_server(plain_response(
            "200 OK",
            "application/json",
            r#"{"models":[]}"#,
        ));

        assert!(ping(&format!("http://{addr}/")).await);
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(
            request.starts_with("GET /api/tags HTTP/1.1\r\n"),
            "{request}"
        );

        // Bind then drop, so nothing is listening on a port we know is free.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let free = listener.local_addr().expect("address");
        drop(listener);
        assert!(!ping(&format!("http://{free}")).await);
    }

    /// Never run in CI or by an agent: run by hand against a local Ollama.
    #[tokio::test]
    #[ignore = "hits a local Ollama; run manually with `ollama serve` running"]
    async fn live_ollama_smoke() {
        let provider = OllamaProvider::new("http://localhost:11434", None).expect("build provider");
        let models = provider.list_models().await.expect("list models");
        assert!(!models.is_empty(), "no models installed");

        let messages = user("Reply with exactly one word: pong");
        let mut req = ChatRequest::new(&messages, &models[0].name);
        req.max_tokens = Some(16);
        let text = provider.chat_collect(req).await.expect("live chat");
        assert!(!text.trim().is_empty(), "empty reply");
    }

    #[tokio::test]
    async fn a_delta_survives_an_error_line_in_the_same_chunk() {
        // Content line and error line in ONE http chunk.
        let body = concat!(
            r#"{"model":"m","message":{"role":"assistant","content":"partial"},"done":false}"#,
            "\n",
            r#"{"error":"model ran out of memory"}"#,
            "\n"
        );
        let frames = vec![body.as_bytes().to_vec()];
        let (addr, _rx) = spawn_server(chunked_response("application/x-ndjson", &frames));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "partial"),
            "{events:?}"
        );
        assert!(
            matches!(&events[1], Err(ChatError::Api { status: 200, .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_delta_survives_a_malformed_line_in_the_same_chunk() {
        let body = concat!(
            r#"{"model":"m","message":{"role":"assistant","content":"kept"},"done":false}"#,
            "\n",
            "{not json}\n"
        );
        let frames = vec![body.as_bytes().to_vec()];
        let (addr, _rx) = spawn_server(chunked_response("application/x-ndjson", &frames));
        let provider = provider_for(addr, None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "kept"));
        assert!(matches!(&events[1], Err(ChatError::Parse(_))), "{events:?}");
    }

    #[tokio::test]
    async fn tool_calls_stream_as_events_and_the_wire_speaks_ollama() {
        let body = concat!(
            r#"{"message":{"role":"assistant","content":"on it"},"done":false}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"save_memory","arguments":{"content":"likes espresso"}}}]},"done":false}"#,
            "\n",
            r#"{"message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":9,"eval_count":3}"#,
            "\n"
        );
        let (addr, rx) = spawn_server(ndjson_response(body));
        let provider = provider_for(addr, None);
        let call = ToolCall {
            id: String::new(),
            name: "save_memory".to_string(),
            arguments: serde_json::json!({"content": "likes espresso"}),
        };
        let messages = vec![
            Message::new(Role::User, "remember I like espresso"),
            Message::tool_request("", vec![call.clone()]),
            Message::tool_result(call, "saved as espresso"),
        ];
        let tools = [ToolDef {
            name: "save_memory".to_string(),
            description: "Save one memory".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let mut req = ChatRequest::new(&messages, "llama3.2:3b");
        req.tools = &tools;

        let events: Vec<ChatEvent> = provider
            .chat_stream(req)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream must not error");
        assert_eq!(
            events,
            vec![
                ChatEvent::TextDelta("on it".to_string()),
                ChatEvent::ToolCall(ToolCall {
                    id: String::new(),
                    name: "save_memory".to_string(),
                    arguments: serde_json::json!({"content": "likes espresso"}),
                }),
                ChatEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 9,
                        output_tokens: 3,
                    }),
                },
            ]
        );

        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "save_memory");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"]["content"],
            "likes espresso"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_name"], "save_memory");
        assert!(body["messages"][0].get("tool_calls").is_none(), "{body}");
        assert!(body["messages"][2].get("answers").is_none(), "{body}");
    }
}
