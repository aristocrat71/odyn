//! OpenAI-compatible streaming chat provider: OpenCode Zen, DeepSeek, and any
//! other endpoint speaking the `/chat/completions` SSE dialect.
//!
//! Must be driven on a tokio runtime with the IO and time drivers enabled —
//! reqwest's requirement, not the `ChatProvider` trait's.

use std::collections::VecDeque;
use std::error::Error as _;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tokio::time::Instant;

use super::sse::{SseEvent, SseParser};
use crate::chat::{ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Usage};

/// Error bodies reach the user, so a stray HTML page must not flood the screen.
const MAX_ERROR_BODY_CHARS: usize = 512;

/// Short enough that a status line never waits on a dead endpoint.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Long enough for a cold gateway, short enough that a model menu still opens.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Reachability for a status line: any HTTP answer means the endpoint is
/// there, so an unauthorized `GET /models` still counts as up. Never an error
/// — "unknown" and "down" look the same to the user.
pub async fn ping(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(PING_TIMEOUT).build() else {
        return false;
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    client.get(url).send().await.is_ok()
}

/// Configuration mistakes, which is why these are separate from [`ChatError`]:
/// none of them can happen mid-conversation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderInitError {
    #[error("base_url must not be empty")]
    EmptyBaseUrl,
    #[error("invalid header name `{name}`")]
    HeaderName { name: String },
    #[error("invalid value for header `{name}`")]
    HeaderValue { name: String },
    #[error("api key contains characters that cannot be sent in an HTTP header")]
    ApiKey,
    #[error("could not build http client: {0}")]
    Client(String),
}

/// No overall request timeout on purpose: a long answer is not a stuck one.
/// A silent one is: `idle` bounds the wait for each read once data flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    /// From issuing the request until the first event arrives.
    pub first_token: Duration,
    /// Longest tolerated silence between reads mid-stream.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            first_token: Duration::from_secs(60),
            idle: Duration::from_secs(90),
        }
    }
}

/// `Debug` is safe to derive: the api key header is marked sensitive, so it
/// prints as `Sensitive` rather than leaking into logs.
#[derive(Debug)]
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    /// Trailing slashes stripped, so `{base_url}/chat/completions` is well formed.
    base_url: String,
    /// Kept because reqwest's connect timeout is a client-level setting, so
    /// `with_timeouts` has to rebuild the client from them.
    headers: HeaderMap,
    timeouts: Timeouts,
}

impl OpenAiCompatProvider {
    /// `api_key` is sent as `Authorization: Bearer …`; `None` sends no auth
    /// header at all, which is what local endpoints expect.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_headers: Vec<(String, String)>,
    ) -> Result<Self, ProviderInitError> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ProviderInitError::EmptyBaseUrl);
        }

        let mut headers = HeaderMap::new();
        for (name, value) in default_headers {
            let header_name = HeaderName::try_from(name.as_str())
                .map_err(|_| ProviderInitError::HeaderName { name: name.clone() })?;
            let mut header_value = HeaderValue::from_str(&value)
                .map_err(|_| ProviderInitError::HeaderValue { name })?;
            // Gateways take keys in arbitrary headers (`api-key`, `x-api-key`),
            // so every configured value is treated as a secret.
            header_value.set_sensitive(true);
            headers.insert(header_name, header_value);
        }
        if let Some(key) = api_key {
            let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| ProviderInitError::ApiKey)?;
            // Keeps the key out of reqwest's debug output.
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }

        let timeouts = Timeouts::default();
        let client = build_client(&headers, &timeouts)?;
        Ok(Self {
            client,
            base_url,
            headers,
            timeouts,
        })
    }

    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Result<Self, ProviderInitError> {
        self.client = build_client(&self.headers, &timeouts)?;
        self.timeouts = timeouts;
        Ok(self)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model names the endpoint serves, sorted and deduplicated. Bounded
    /// as a whole: a listing is small and instant, so a wedged endpoint must
    /// not hang a model menu the way the streaming timeouts allow a long
    /// answer to.
    pub async fn list_models(&self) -> Result<Vec<String>, ChatError> {
        let url = format!("{}/models", self.base_url);
        let fetch = async {
            let response = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(|err| transport_error(&err))?;
            let status = response.status();
            let body = response.text().await.map_err(|err| transport_error(&err))?;
            Ok::<_, ChatError>((status, body))
        };
        let (status, body) = match tokio::time::timeout(LIST_TIMEOUT, fetch).await {
            Err(_) => {
                return Err(ChatError::Network(format!(
                    "no model list from {url} within {LIST_TIMEOUT:?}"
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

        let listing: ModelListing = serde_json::from_str(&body)
            .map_err(|err| ChatError::Parse(format!("malformed model list: {err}")))?;
        let mut names: Vec<String> = listing
            .into_ids()
            .into_iter()
            // Some endpoints namespace what they list (`models/<name>`) but
            // take the bare name at `/chat/completions`. No model name starts
            // with that segment, so stripping it only ever makes a name usable.
            .map(|id| id.strip_prefix("models/").unwrap_or(&id).to_string())
            .filter(|id| !id.trim().is_empty())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// Both shapes seen in the wild: OpenAI's envelope, and the bare array some
/// gateways answer with.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ModelListing {
    Envelope { data: Vec<ModelId> },
    Bare(Vec<ModelId>),
}

impl ModelListing {
    fn into_ids(self) -> Vec<String> {
        let models = match self {
            Self::Envelope { data } => data,
            Self::Bare(models) => models,
        };
        models.into_iter().map(|model| model.id).collect()
    }
}

#[derive(serde::Deserialize)]
struct ModelId {
    id: String,
}

fn build_client(
    headers: &HeaderMap,
    timeouts: &Timeouts,
) -> Result<reqwest::Client, ProviderInitError> {
    reqwest::Client::builder()
        .default_headers(headers.clone())
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.idle)
        .build()
        .map_err(|err| ProviderInitError::Client(err.to_string()))
}

impl ChatProvider for OpenAiCompatProvider {
    fn chat_stream<'a>(
        &'a self,
        req: ChatRequest<'a>,
    ) -> BoxStream<'a, Result<ChatEvent, ChatError>> {
        // Serialized eagerly so the stream state borrows nothing from `req`.
        let body = match serde_json::to_vec(&ChatCompletionRequest {
            model: req.model,
            messages: req.messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
            temperature: req.temperature,
            max_tokens: req.max_tokens,
        }) {
            Ok(body) => body,
            Err(err) => {
                let err = ChatError::Parse(format!("could not encode request: {err}"));
                return futures::stream::once(std::future::ready(Err(err))).boxed();
            }
        };

        let builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        futures::stream::unfold(
            State::Start {
                builder: Box::new(builder),
                first_token: self.timeouts.first_token,
            },
            step,
        )
        .boxed()
    }
}

#[derive(serde::Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(serde::Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Unknown fields are ignored, so provider extras (`reasoning_content`,
/// `logprobs`, …) stay non-breaking.
#[derive(serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<StreamUsage>,
}

#[derive(serde::Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(serde::Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(serde::Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(serde::Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(serde::Deserialize)]
struct ApiErrorDetail {
    message: String,
}

enum State {
    Start {
        builder: Box<reqwest::RequestBuilder>,
        first_token: Duration,
    },
    Streaming(Box<Streaming>),
    Finished,
}

struct Streaming {
    body: BoxStream<'static, Result<Vec<u8>, reqwest::Error>>,
    parser: SseParser,
    /// One chunk can decode into several events.
    pending: VecDeque<ChatEvent>,
    usage: Option<Usage>,
    /// `None` once the stream has produced something: the first-token budget no
    /// longer applies.
    deadline: Option<Instant>,
    first_token: Duration,
    /// `[DONE]` seen or body exhausted: drain `pending`, then end the stream.
    terminated: bool,
    /// A decode error, held back until every event decoded before it has been
    /// delivered: how much text survives a bad chunk must not depend on how
    /// TCP framed it.
    failure: Option<ChatError>,
}

impl Streaming {
    fn absorb(&mut self, events: Vec<SseEvent>) {
        for event in events {
            if self.terminated || self.failure.is_some() {
                break;
            }
            match event {
                SseEvent::Done => {
                    self.pending
                        .push_back(ChatEvent::Done { usage: self.usage });
                    self.terminated = true;
                }
                SseEvent::Data(payload) => {
                    // A blank `data:` line is a keep-alive in the wild, not an
                    // event.
                    if payload.trim().is_empty() {
                        continue;
                    }
                    let chunk: StreamChunk = match serde_json::from_str(&payload) {
                        Ok(chunk) => chunk,
                        Err(err) => {
                            self.failure =
                                Some(ChatError::Parse(format!("malformed stream chunk: {err}")));
                            break;
                        }
                    };
                    if let Some(usage) = chunk.usage {
                        self.usage = Some(Usage {
                            input_tokens: usage.prompt_tokens,
                            output_tokens: usage.completion_tokens,
                        });
                    }
                    // Only the first choice matters: n > 1 is not part of v1.
                    if let Some(content) = chunk
                        .choices
                        .into_iter()
                        .next()
                        .and_then(|c| c.delta.content)
                    {
                        // The opening `{"role":"assistant","content":""}` chunk
                        // is not a delta.
                        if !content.is_empty() {
                            self.pending.push_back(ChatEvent::TextDelta(content));
                        }
                    }
                }
            }
        }
    }
}

async fn step(mut state: State) -> Option<(Result<ChatEvent, ChatError>, State)> {
    loop {
        state = match state {
            State::Finished => return None,

            State::Start {
                builder,
                first_token,
            } => {
                let deadline = Instant::now() + first_token;
                let response = match tokio::time::timeout_at(deadline, (*builder).send()).await {
                    Err(_) => {
                        return Some((Err(first_token_timeout(first_token)), State::Finished))
                    }
                    Ok(Err(err)) => return Some((Err(transport_error(&err)), State::Finished)),
                    Ok(Ok(response)) => response,
                };

                let status = response.status();
                if !status.is_success() {
                    let message = match read_error_body(response, first_token).await {
                        Ok(body) => error_message(&body),
                        Err(reason) => format!("could not read error body: {reason}"),
                    };
                    let err = ChatError::Api {
                        status: status.as_u16(),
                        message,
                    };
                    return Some((Err(err), State::Finished));
                }
                // A 2xx that declares a non-SSE content type is a failure
                // report, not an answer; parsing it as SSE would end as a
                // silent empty reply. A missing content type gets the benefit
                // of the doubt.
                let declared = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_ascii_lowercase);
                if let Some(declared) = declared {
                    if !declared.contains("text/event-stream") {
                        let message = match read_error_body(response, first_token).await {
                            Ok(body) => error_message(&body),
                            Err(reason) => format!("could not read error body: {reason}"),
                        };
                        let err = ChatError::Api {
                            status: status.as_u16(),
                            message: format!("response was not an event stream: {message}"),
                        };
                        return Some((Err(err), State::Finished));
                    }
                }

                // `to_vec` copies, but it avoids taking a `bytes` dependency
                // just to name the item type, and SSE chunks are tiny.
                let body = response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                    .boxed();
                State::Streaming(Box::new(Streaming {
                    body,
                    parser: SseParser::default(),
                    pending: VecDeque::new(),
                    usage: None,
                    deadline: Some(deadline),
                    first_token,
                    terminated: false,
                    failure: None,
                }))
            }

            State::Streaming(mut streaming) => {
                if let Some(event) = streaming.pending.pop_front() {
                    streaming.deadline = None;
                    return Some((Ok(event), State::Streaming(streaming)));
                }
                if let Some(err) = streaming.failure.take() {
                    return Some((Err(err), State::Finished));
                }
                if streaming.terminated {
                    return None;
                }

                let next = match streaming.deadline {
                    Some(deadline) => {
                        match tokio::time::timeout_at(deadline, streaming.body.next()).await {
                            Ok(next) => next,
                            Err(_) => {
                                let err = first_token_timeout(streaming.first_token);
                                return Some((Err(err), State::Finished));
                            }
                        }
                    }
                    None => streaming.body.next().await,
                };

                let ended = next.is_none();
                let (events, parse_failure) = match next {
                    Some(Ok(bytes)) => streaming.parser.feed(&bytes),
                    Some(Err(err)) => return Some((Err(transport_error(&err)), State::Finished)),
                    None => streaming.parser.finish(),
                };
                streaming.absorb(events);
                // An absorb failure sits earlier in the byte stream, so it wins.
                if streaming.failure.is_none() {
                    streaming.failure = parse_failure;
                }
                // Servers that just close the connection never send `[DONE]`.
                if ended && !streaming.terminated && streaming.failure.is_none() {
                    streaming.pending.push_back(ChatEvent::Done {
                        usage: streaming.usage,
                    });
                    streaming.terminated = true;
                }
                State::Streaming(streaming)
            }
        };
    }
}

fn first_token_timeout(budget: Duration) -> ChatError {
    ChatError::Network(format!(
        "no response from the provider within the first-token timeout ({budget:?})"
    ))
}

/// reqwest's own message is generic; the actionable cause (connection refused,
/// dns failure, tls handshake) sits at the end of the source chain.
fn transport_error(err: &reqwest::Error) -> ChatError {
    let mut message = if err.is_connect() {
        match err.url() {
            Some(url) => format!("could not connect to {url}"),
            None => "could not connect to the provider".to_string(),
        }
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

/// Bounded in bytes and time: an error body must never hang or bloat the
/// client the way `Response::text` on a stalled or endless body would.
async fn read_error_body(response: reqwest::Response, budget: Duration) -> Result<String, String> {
    const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
    let read = async {
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| err.to_string())?;
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_ERROR_BODY_BYTES {
                break;
            }
        }
        Ok::<_, String>(body)
    };
    match tokio::time::timeout(budget, read).await {
        Err(_) => Err("timed out".to_string()),
        Ok(Err(err)) => Err(err),
        Ok(Ok(body)) => Ok(String::from_utf8_lossy(&body).into_owned()),
    }
}

fn error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        return truncate(parsed.error.message.trim());
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no response body".to_string()
    } else {
        truncate(trimmed)
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_ERROR_BODY_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_ERROR_BODY_CHARS).collect();
    format!("{head}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Role;
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;

    /// A recorded DeepSeek-shaped stream, split the way a server flushes it.
    /// Frames 3 and 4 split one event in half on purpose.
    fn openai_frames() -> Vec<String> {
        vec![
            ": keep-alive\n\n".to_string(),
            concat!(
                r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"#,
                r#""model":"deepseek-chat","system_fingerprint":"fp_1","#,
                r#""choices":[{"index":0,"delta":{"role":"assistant","content":""},"logprobs":null,"finish_reason":null}]}"#,
                "\n\n"
            )
            .to_string(),
            r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hel"#
                .to_string(),
            "lo\"},\"finish_reason\":null}]}\n\n".to_string(),
            concat!(
                r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":", "},"finish_reason":null}]}"#,
                "\n\n",
                r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":"wörld"},"finish_reason":null}]}"#,
                "\n\n"
            )
            .to_string(),
            concat!(
                r#"data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "\n\n",
                r#"data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15}}"#,
                "\n\n",
                "data: [DONE]\n\n"
            )
            .to_string(),
        ]
    }

    /// One HTTP chunk per frame, so the client really sees those boundaries.
    fn chunked_sse_response(frames: &[String]) -> Vec<Vec<u8>> {
        let mut pieces = vec![b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()];
        for frame in frames {
            pieces.push(format!("{:x}\r\n{frame}\r\n", frame.len()).into_bytes());
        }
        pieces.push(b"0\r\n\r\n".to_vec());
        pieces
    }

    fn error_response(status_line: &str, content_type: &str, body: &str) -> Vec<Vec<u8>> {
        vec![format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()]
    }

    /// Serves one connection with a canned response, returning the raw request
    /// so tests can assert on headers and body.
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

    /// Accepts, then says nothing, so the first-token deadline is what fires.
    fn spawn_silent_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = read_request(&mut stream);
            std::thread::sleep(Duration::from_secs(5));
        });
        addr
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

    fn provider_for(addr: SocketAddr, path: &str, api_key: Option<String>) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(format!("http://{addr}{path}"), api_key, Vec::new())
            .expect("build provider")
    }

    async fn drive(
        provider: &OpenAiCompatProvider,
        messages: &[Message],
    ) -> Vec<Result<ChatEvent, ChatError>> {
        provider
            .chat_stream(ChatRequest::new(messages, "deepseek-chat"))
            .collect()
            .await
    }

    fn user(text: &str) -> Vec<Message> {
        vec![Message::new(Role::User, text)]
    }

    #[test]
    fn base_url_trailing_slashes_are_stripped() {
        let provider = OpenAiCompatProvider::new("https://example.test/v1///", None, Vec::new())
            .expect("build provider");
        assert_eq!(provider.base_url(), "https://example.test/v1");
    }

    #[test]
    fn empty_base_url_is_rejected() {
        let err = OpenAiCompatProvider::new("///", None, Vec::new()).expect_err("empty base url");
        assert!(
            matches!(err, ProviderInitError::EmptyBaseUrl),
            "got {err:?}"
        );
    }

    #[test]
    fn invalid_default_header_name_is_rejected() {
        let err = OpenAiCompatProvider::new(
            "https://example.test",
            None,
            vec![("bad header".to_string(), "value".to_string())],
        )
        .expect_err("invalid header name");
        assert!(
            matches!(err, ProviderInitError::HeaderName { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn timeouts_default_to_ten_and_sixty_seconds() {
        let timeouts = Timeouts::default();
        assert_eq!(timeouts.connect, Duration::from_secs(10));
        assert_eq!(timeouts.first_token, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn streams_deltas_and_usage_from_a_recorded_sse_response() {
        let (addr, _rx) = spawn_server(chunked_sse_response(&openai_frames()));
        let provider = provider_for(addr, "/v1", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        let events: Vec<ChatEvent> = events
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("stream must not error");

        assert_eq!(
            events,
            vec![
                ChatEvent::TextDelta("Hello".to_string()),
                ChatEvent::TextDelta(", ".to_string()),
                ChatEvent::TextDelta("wörld".to_string()),
                ChatEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 11,
                        output_tokens: 4,
                    }),
                },
            ]
        );
    }

    #[tokio::test]
    async fn chat_collect_joins_the_deltas() {
        let (addr, _rx) = spawn_server(chunked_sse_response(&openai_frames()));
        let provider = provider_for(addr, "/v1", None);
        let messages = user("hi");

        let text = provider
            .chat_collect(ChatRequest::new(&messages, "deepseek-chat"))
            .await
            .expect("collect");
        assert_eq!(text, "Hello, wörld");
    }

    #[tokio::test]
    async fn request_targets_chat_completions_with_the_expected_body() {
        let (addr, rx) = spawn_server(chunked_sse_response(&openai_frames()));
        let provider = OpenAiCompatProvider::new(
            format!("http://{addr}/v1/"),
            Some("secret-key".to_string()),
            vec![("x-title".to_string(), "odyn".to_string())],
        )
        .expect("build provider");
        let messages = user("hi");
        let mut req = ChatRequest::new(&messages, "deepseek-chat");
        req.temperature = Some(0.2);
        req.max_tokens = Some(64);

        let _ = provider.chat_collect(req).await.expect("collect");
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        let lower = request.to_ascii_lowercase();

        assert!(
            request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            lower.contains("authorization: bearer secret-key"),
            "{request}"
        );
        assert!(lower.contains("x-title: odyn"), "{request}");
        assert!(
            lower.contains("content-type: application/json"),
            "{request}"
        );

        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn optional_fields_are_omitted_and_no_auth_header_is_sent_without_a_key() {
        let (addr, rx) = spawn_server(chunked_sse_response(&openai_frames()));
        let provider = provider_for(addr, "", None);
        let messages = user("hi");

        let _ = provider
            .chat_collect(ChatRequest::new(&messages, "local-model"))
            .await
            .expect("collect");
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");

        assert!(
            request.starts_with("POST /chat/completions HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            !request.to_ascii_lowercase().contains("authorization"),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("max_tokens").is_none(), "{body}");
    }

    #[tokio::test]
    async fn stream_without_done_sentinel_still_ends_with_done() {
        // Server closes after the usage chunk; no `[DONE]`, no trailing blank line.
        let frames = vec![concat!(
            r#"data: {"choices":[{"delta":{"content":"only"}}]}"#,
            "\n\n",
            r#"data: {"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1}}"#,
            "\n"
        )
        .to_string()];
        let (addr, _rx) = spawn_server(chunked_sse_response(&frames));
        let provider = provider_for(addr, "/v1", None);
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
                ChatEvent::Done {
                    usage: Some(Usage {
                        input_tokens: 7,
                        output_tokens: 1,
                    }),
                },
            ]
        );
    }

    #[tokio::test]
    async fn malformed_chunk_json_is_a_parse_error() {
        let frames = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_string(),
            "data: {not json}\n\n".to_string(),
        ];
        let (addr, _rx) = spawn_server(chunked_sse_response(&frames));
        let provider = provider_for(addr, "/v1", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert!(
            matches!(events[0], Ok(ChatEvent::TextDelta(_))),
            "{:?}",
            events[0]
        );
        match events.get(1) {
            Some(Err(ChatError::Parse(message))) => {
                assert!(message.contains("malformed stream chunk"), "{message}")
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        assert_eq!(events.len(), 2, "the stream must stop after a parse error");
    }

    #[tokio::test]
    async fn http_401_with_a_json_error_body_maps_to_api_error() {
        let body = r#"{"error":{"message":"Authentication Fails, Your api key: ****ey is invalid","type":"authentication_error","code":"invalid_request_error"}}"#;
        let (addr, _rx) =
            spawn_server(error_response("401 Unauthorized", "application/json", body));
        let provider = provider_for(addr, "/v1", Some("bad-key".to_string()));
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ChatError::Api { status, message }) => {
                assert_eq!(*status, 401);
                assert_eq!(
                    message,
                    "Authentication Fails, Your api key: ****ey is invalid"
                );
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_error_body_falls_back_to_raw_text() {
        let (addr, _rx) = spawn_server(error_response(
            "502 Bad Gateway",
            "text/html",
            "<html><body>upstream is down</body></html>",
        ));
        let provider = provider_for(addr, "/v1", None);
        let messages = user("hi");

        match &drive(&provider, &messages).await[0] {
            Err(ChatError::Api { status, message }) => {
                assert_eq!(*status, 502);
                assert!(message.contains("upstream is down"), "{message}");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_refused_maps_to_a_network_error() {
        // Bind then drop, so nothing is listening on a port we know is free.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        drop(listener);

        let provider = provider_for(addr, "/v1", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ChatError::Network(message)) => {
                assert!(message.contains("could not connect"), "{message}")
            }
            other => panic!("expected a network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_token_timeout_maps_to_a_network_error() {
        let addr = spawn_silent_server();
        let provider = provider_for(addr, "/v1", None)
            .with_timeouts(Timeouts {
                first_token: Duration::from_millis(150),
                ..Timeouts::default()
            })
            .expect("set timeouts");
        let messages = user("hi");

        match &drive(&provider, &messages).await[0] {
            Err(ChatError::Network(message)) => {
                assert!(message.contains("first-token timeout"), "{message}")
            }
            other => panic!("expected a network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_reports_reachable_even_when_the_key_is_rejected() {
        let (addr, rx) = spawn_server(error_response(
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"no key"}}"#,
        ));

        assert!(ping(&format!("http://{addr}/v1/")).await);
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(
            request.starts_with("GET /v1/models HTTP/1.1\r\n"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn list_models_reads_the_envelope_and_normalises_the_names() {
        let body = r#"{"object":"list","data":[
            {"id":"models/qwen3-32b"},
            {"id":"gpt-oss-120b"},
            {"id":"gpt-oss-120b"},
            {"id":"  "}
        ]}"#;
        let (addr, rx) = spawn_server(error_response("200 OK", "application/json", body));
        let provider = provider_for(addr, "/v1", Some("sk-test".to_string()));

        let models = provider.list_models().await.expect("list models");

        // Sorted and deduplicated, with the namespace some endpoints put on
        // their ids stripped back to the name the chat endpoint takes.
        assert_eq!(models, vec!["gpt-oss-120b", "qwen3-32b"]);
        let request = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(
            request.starts_with("GET /v1/models HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{request}"
        );
    }

    /// Gateways that answer with the bare array are common enough that the
    /// envelope cannot be assumed.
    #[tokio::test]
    async fn list_models_reads_a_bare_array_too() {
        let (addr, _rx) = spawn_server(error_response(
            "200 OK",
            "application/json",
            r#"[{"id":"kimi-k3"},{"id":"deepseek-chat"}]"#,
        ));
        let provider = provider_for(addr, "/v1", None);

        let models = provider.list_models().await.expect("list models");
        assert_eq!(models, vec!["deepseek-chat", "kimi-k3"]);
    }

    /// A rejected key is the connect flow's one refusal, so it has to arrive as
    /// a status and not as a network failure.
    #[tokio::test]
    async fn list_models_surfaces_a_rejected_key_as_its_status() {
        let (addr, _rx) = spawn_server(error_response(
            "401 Unauthorized",
            "application/json",
            r#"{"error":{"message":"invalid api key"}}"#,
        ));
        let provider = provider_for(addr, "/v1", Some("sk-wrong".to_string()));

        match provider.list_models().await {
            Err(ChatError::Api { status, message }) => {
                assert_eq!(status, 401);
                assert_eq!(message, "invalid api key");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_reports_unreachable_when_nothing_listens() {
        // Bind then drop, so nothing is listening on a port we know is free.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        drop(listener);

        assert!(!ping(&format!("http://{addr}/v1")).await);
    }

    #[test]
    fn error_bodies_are_truncated() {
        let long = "x".repeat(MAX_ERROR_BODY_CHARS + 50);
        let message = error_message(&long);
        assert_eq!(message.chars().count(), MAX_ERROR_BODY_CHARS + 3);
        assert!(message.ends_with("..."));
    }

    #[test]
    fn empty_error_body_is_described() {
        assert_eq!(error_message("   "), "no response body");
    }

    /// Live smoke test. Never run in CI or by an agent: run it by hand with
    /// `DEEPSEEK_API_KEY=… cargo test -p odyn-core -- --ignored live_deepseek`.
    #[tokio::test]
    #[ignore = "hits the live DeepSeek API; run manually with DEEPSEEK_API_KEY set"]
    async fn live_deepseek_smoke() {
        let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY must be set");
        let provider = OpenAiCompatProvider::new("https://api.deepseek.com", Some(key), Vec::new())
            .expect("build provider");
        let messages = user("Reply with exactly one word: pong");
        let mut req = ChatRequest::new(&messages, "deepseek-chat");
        req.max_tokens = Some(16);

        let text = provider.chat_collect(req).await.expect("live chat");
        assert!(
            text.to_lowercase().contains("pong"),
            "unexpected reply: {text}"
        );
    }

    #[tokio::test]
    async fn a_delta_survives_a_malformed_event_in_the_same_chunk() {
        // One HTTP chunk holds a good event and then a bad one: the good delta
        // must be delivered before the error, whatever the framing.
        let frames = vec![concat!(
            r#"data: {"id":"x","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
            "\n\n",
            "data: {not json}\n\n"
        )
        .to_string()];
        let (addr, _rx) = spawn_server(chunked_sse_response(&frames));
        let provider = provider_for(addr, "", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "ok"),
            "{events:?}"
        );
        assert!(matches!(&events[1], Err(ChatError::Parse(_))), "{events:?}");
    }

    #[tokio::test]
    async fn a_success_that_is_not_an_event_stream_is_an_api_error() {
        // Gateways report failures as 200 + JSON; treating that as an empty
        // answer hid the whole class.
        let (addr, _rx) = spawn_server(error_response(
            "200 OK",
            "application/json",
            r#"{"error":{"message":"rate limit exceeded","type":"rate_limit"}}"#,
        ));
        let provider = provider_for(addr, "", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            Err(ChatError::Api {
                status: 200,
                message,
            }) => {
                assert!(message.contains("not an event stream"), "{message}");
                assert!(message.contains("rate limit exceeded"), "{message}");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_data_heartbeat_is_ignored() {
        let frames = vec![concat!(
            r#"data: {"id":"x","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            "\n\n",
            "data:\n\n",
            "data: [DONE]\n\n"
        )
        .to_string()];
        let (addr, _rx) = spawn_server(chunked_sse_response(&frames));
        let provider = provider_for(addr, "", None);
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "hi"));
        assert!(matches!(&events[1], Ok(ChatEvent::Done { .. })));
    }

    #[tokio::test]
    async fn a_stream_that_goes_silent_mid_answer_times_out() {
        // Headers and one delta arrive, then the server holds the socket open
        // silently; the idle timeout must end the stream with an error.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let frame = "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{frame}\r\n",
                frame.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(5));
        });
        let provider = provider_for(addr, "", None)
            .with_timeouts(Timeouts {
                first_token: Duration::from_secs(5),
                idle: Duration::from_millis(300),
                ..Timeouts::default()
            })
            .expect("set timeouts");
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Ok(ChatEvent::TextDelta(text)) if text == "hi"));
        assert!(
            matches!(&events[1], Err(ChatError::Network(_))),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_stalled_error_body_times_out_instead_of_hanging() {
        // Error headers arrive, the body never does; reading it must give up
        // within the first-token budget rather than hang forever.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nstart\r\n",
            );
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(5));
        });
        let provider = provider_for(addr, "", None)
            .with_timeouts(Timeouts {
                first_token: Duration::from_millis(300),
                ..Timeouts::default()
            })
            .expect("set timeouts");
        let messages = user("hi");

        let events = drive(&provider, &messages).await;
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            Err(ChatError::Api {
                status: 500,
                message,
            }) => {
                assert!(message.contains("could not read error body"), "{message}");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
    }
}
