//! What both commands share: which provider/model to talk to, how a reply is
//! streamed, and how a failure reaches the user.

use std::io::Write;

use futures::StreamExt;
use odyn_core::brain::{self, Ask, InjectedContext};
use odyn_core::brevity::Brevity;
use odyn_core::chat::{ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, Usage};
use odyn_core::config::{BrainConfig, Config, ConfigError, ProviderConfig, ProviderRegistry};
use odyn_core::embed::load_default_embedder;
use odyn_core::storage::{Storage, StorageError};

const TITLE_CHARS: usize = 40;

/// Exit codes are part of the CLI's contract: 1 for a provider or runtime
/// failure, 2 for anything the user has to fix in `odyn.toml` or the flags.
pub struct Failure {
    pub code: u8,
    pub message: String,
}

impl Failure {
    pub fn run(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }
}

/// Red only on the prefix; anstream drops it when stderr is not a terminal.
pub fn warn(message: &str) {
    let _ = writeln!(anstream::stderr(), "\u{1b}[31modyn:\u{1b}[0m {message}");
}

pub fn write_failure(err: std::io::Error) -> Failure {
    Failure::run(format!("could not write to stdout: {err}"))
}

pub struct Session {
    registry: ProviderRegistry,
    pub provider: String,
    pub model: String,
    pub handle: Box<dyn ChatProvider>,
    pub brain: BrainConfig,
    /// The `[style]` default; a `--brevity` flag overrides it per invocation.
    pub brevity: Brevity,
}

impl Session {
    /// Flag first, then the provider's `default_model`; anything unresolved is
    /// an error that says what to pass.
    pub fn start(provider: Option<String>, model: Option<String>) -> Result<Self, Failure> {
        let config = Config::load().map_err(config_failure)?;
        let registry = ProviderRegistry::from_config(&config).map_err(config_failure)?;
        let provider = provider.unwrap_or_else(|| registry.default_provider_name().to_string());
        known(&registry, &provider)?;
        let model = match model {
            Some(model) => model,
            None => default_model(&config, &provider)?,
        };
        let handle = registry.provider(&provider).map_err(config_failure)?;
        Ok(Self {
            registry,
            provider,
            model,
            handle,
            brain: config.brain.clone(),
            brevity: config.style.brevity,
        })
    }

    /// `provider` is `None` when only the model changed.
    pub fn switch(&mut self, provider: Option<String>, model: String) -> Result<(), Failure> {
        if let Some(provider) = provider {
            known(&self.registry, &provider)?;
            self.handle = self.registry.provider(&provider).map_err(config_failure)?;
            self.provider = provider;
        }
        self.model = model;
        Ok(())
    }

    pub fn knows(&self, provider: &str) -> bool {
        self.registry.kind(provider).is_some()
    }
}

pub struct Reply {
    pub text: String,
    pub usage: Option<Usage>,
}

/// Drives one reply, handing every delta to `emit` as it arrives so the caller
/// decides what reaches the terminal.
pub async fn stream_reply(
    provider: &dyn ChatProvider,
    model: &str,
    messages: &[Message],
    mut emit: impl FnMut(&str) -> std::io::Result<()>,
) -> Result<Reply, Failure> {
    let mut stream = provider.chat_stream(ChatRequest::new(messages, model));
    let mut text = String::new();
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event.map_err(|err| Failure::run(format!("stream failed: {}", describe(&err))))? {
            ChatEvent::TextDelta(delta) => {
                emit(&delta).map_err(write_failure)?;
                text.push_str(&delta);
            }
            ChatEvent::Done { usage: reported } => {
                usage = reported;
                break;
            }
        }
    }
    Ok(Reply { text, usage })
}

pub fn save_turn(
    storage: &Storage,
    conversation: i64,
    prompt: &str,
    reply: &Reply,
    injected: &[i64],
) -> Result<(), StorageError> {
    storage.append_turn(conversation, prompt, &reply.text, reply.usage, injected)
}

/// Memory is opt-in and additive: a turn that never mentioned `/brain` gets
/// the style directive alone, and when the brain cannot run, the turn still
/// goes out — uninjected, loudly, with nothing recorded. `None` means exactly
/// that. No database is not a failure either: style still applies.
pub fn memory_context(
    storage: Option<&Storage>,
    brain_config: &BrainConfig,
    history: &[Message],
    ask: &Ask,
    brevity: Brevity,
) -> Option<InjectedContext> {
    if !ask.recall {
        return Some(brain::empty_context(brevity));
    }
    let Some(storage) = storage else {
        return Some(brain::empty_context(brevity));
    };
    // The folder is the truth: recall reads the files as they are now.
    if let Err(err) = brain::sync(storage, brain_config, load_default_embedder) {
        warn(&format!("brain folder not synced: {err}"));
    }
    let context = brain::build_context(
        storage,
        brain_config,
        history,
        &ask.query,
        brevity,
        load_default_embedder,
    );
    match context {
        Ok(context) => Some(context),
        Err(err) => {
            warn(&format!("memory skipped this turn: {err}"));
            None
        }
    }
}

/// The messages a turn actually sends: the injected context, when there is
/// any, ahead of the conversation. The system message can be non-empty with
/// zero memories — a brevity directive alone still has to reach the model.
pub fn with_context(context: Option<&InjectedContext>, history: &[Message]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() + 1);
    if let Some(context) = context.filter(|context| !context.system_message.is_empty()) {
        messages.push(Message::new(Role::System, context.system_message.as_str()));
    }
    messages.extend(history.iter().cloned());
    messages
}

/// `--show-context`: the system message verbatim, then per-item token counts.
/// Text mode goes to stderr so piped answers stay clean; in `--json` mode it
/// is one more event on the stream.
pub fn print_context(
    context: Option<&InjectedContext>,
    brain_config: &BrainConfig,
    json: bool,
) -> Result<(), Failure> {
    if json {
        let items: Vec<serde_json::Value> = context
            .map(|context| {
                context
                    .memories
                    .iter()
                    .map(|memory| serde_json::json!({"id": memory.slug, "tokens": memory.tokens}))
                    .collect()
            })
            .unwrap_or_default();
        let event = serde_json::json!({
            "type": "context",
            "system": context.map(|context| context.system_message.as_str()).unwrap_or(""),
            "items": items,
            "note": "token counts are a chars/4 approximation",
        });
        return writeln!(anstream::stdout(), "{event}").map_err(write_failure);
    }
    let mut err = anstream::stderr().lock();
    let done = match context.filter(|context| !context.system_message.is_empty()) {
        None => writeln!(err, "----- context: empty -----"),
        Some(context) => writeln!(err, "----- context -----")
            .and_then(|()| writeln!(err, "{}", context.system_message))
            .and_then(|()| writeln!(err, "----- tokens (chars/4 approximation) -----"))
            .and_then(|()| {
                for item in &context.memories {
                    writeln!(err, "{} {}", item.slug, item.tokens)?;
                }
                writeln!(
                    err,
                    "memories {}/{} tk",
                    context.tokens, brain_config.cap_tokens
                )
            })
            .and_then(|()| writeln!(err, "-------------------")),
    };
    done.map_err(|err| Failure::run(format!("could not write to stderr: {err}")))
}

/// A conversation title has to fit one sidebar line, so: one line, 40 chars.
pub fn title_from(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_CHARS)
        .collect()
}

/// `ChatError`'s own prefixes would read as "stream failed: network error: …".
fn describe(err: &ChatError) -> String {
    match err {
        ChatError::Network(message) | ChatError::Parse(message) => message.clone(),
        ChatError::Api { status, message } => format!("provider returned {status}: {message}"),
        ChatError::Cancelled => "cancelled".to_string(),
    }
}

fn default_model(config: &Config, provider: &str) -> Result<String, Failure> {
    if let Some(ProviderConfig::OpenAiCompat {
        default_model: Some(model),
        ..
    }) = config.providers.get(provider)
    {
        return Ok(model.clone());
    }
    Err(Failure::config(format!(
        "no model for `{provider}`: pass --model, or set default_model under [providers.{provider}]"
    )))
}

fn known(registry: &ProviderRegistry, name: &str) -> Result<(), Failure> {
    if registry.kind(name).is_some() {
        return Ok(());
    }
    Err(Failure::config(format!(
        "no provider named `{name}` in odyn.toml; configured: {}",
        registry.names().join(", ")
    )))
}

pub fn config_failure(err: ConfigError) -> Failure {
    Failure::config(err.to_string())
}
