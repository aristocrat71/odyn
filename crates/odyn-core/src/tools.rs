//! The tool loop: one user turn that may span several provider requests.
//!
//! Tools are offered only when the message asked for them: `/memory` earns
//! `save_memory`, `/update-memory` earns `update_memory`, `/delete-memory`
//! earns `delete_memory`, `/link-memory` earns `link_memory`,
//! `/unlink-memory` earns `unlink_memory`, `/reminder` earns `set_reminder`,
//! and `/schedule` earns `schedule_ask`. One tool per trigger — small models
//! misroute a choice between tools, so the user makes it. The index is not
//! touched here — the folder is the truth, and the next recall or preview
//! syncs it.
//!
//! A workspace in `Effects` makes the turn an agent turn: the `agent` tools
//! join, results always feed back, the budget grows, and nothing is
//! synthesized — the model speaks for itself at the end.

use std::path::Path;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::StreamExt;

use crate::agent;
use crate::chat::{
    ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, ToolCall, ToolDef, Usage,
};
use crate::notes::{self, NotesError};
use crate::reminder;
use crate::storage::now_secs;

pub const SAVE_MEMORY: &str = "save_memory";
pub const UPDATE_MEMORY: &str = "update_memory";
pub const DELETE_MEMORY: &str = "delete_memory";
pub const LINK_MEMORY: &str = "link_memory";
pub const UNLINK_MEMORY: &str = "unlink_memory";
pub const SET_REMINDER: &str = "set_reminder";
pub const SCHEDULE_ASK: &str = "schedule_ask";

/// `(text, due_at, every-phrase)` → the stored row's id.
pub type ReminderSink<'a> = dyn FnMut(&str, i64, Option<&str>) -> Result<i64, String> + Send + 'a;
/// `(prompt, every-phrase, next_at)` → the stored row's id.
pub type ScheduleSink<'a> = dyn FnMut(&str, &str, i64) -> Result<i64, String> + Send + 'a;
/// Shows the user one bash command; resolves when they answer. The allowlist
/// and "always" live on the surface — core asks about every command.
pub type Approver<'a> = dyn FnMut(String) -> BoxFuture<'static, Verdict> + Send + 'a;

/// The user's answer to a bash approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Run,
    Deny,
}

/// Where a call's effects land. The caller lends a writer instead of its
/// storage handle: that mutex must never be held across an await.
pub struct Effects<'a> {
    pub brain_dir: &'a Path,
    /// `None` offers no agent tools: exactly the memory-turn behavior.
    pub workspace: Option<&'a Path>,
    pub set_reminder: &'a mut ReminderSink<'a>,
    pub set_schedule: &'a mut ScheduleSink<'a>,
    pub approve: &'a mut Approver<'a>,
}

/// A memory model that keeps asking for tools is looping, not working.
const MAX_TOOL_ROUNDS: usize = 4;
/// The same call with the same arguments this many times is a stuck model;
/// the turn moves to the wrap-up instead of burning the budget.
const RUNAWAY_CALLS: usize = 15;
const BASH_TIMEOUT: Duration = Duration::from_secs(120);

/// No mid-task pressure — models abandon tasks under warnings — just one
/// clean request for the summary once the work has to stop.
const WRAP_UP: &str = "The tool round budget for this message is spent. Do not \
request more tools — reply now with a short summary of what you did, what \
worked, and what remains.";

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
    Linked {
        from: &'a str,
        to: &'a str,
    },
    Unlinked {
        from: &'a str,
        to: &'a str,
    },
    Reminded {
        text: &'a str,
        due_at: i64,
    },
    Scheduled {
        prompt: &'a str,
        next_at: i64,
    },
    /// An agent tool is about to run; `detail` is its one-line argument.
    AgentCall {
        tool: &'a str,
        detail: &'a str,
    },
    /// What it answered; `truncated` marks a bash tail cut.
    AgentOut {
        text: &'a str,
        truncated: bool,
    },
    /// The agent round counter, emitted as each round's calls arrive.
    Round {
        used: usize,
        budget: usize,
    },
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
    /// `(from, to)` pairs disconnected this turn, in call order.
    pub unlinked: Vec<(String, String)>,
    /// `(text, due_at)` reminders set this turn, in call order.
    pub reminders: Vec<(String, i64)>,
    /// `(prompt, next_at)` asks scheduled this turn, in call order.
    pub schedules: Vec<(String, i64)>,
    /// Provider requests the turn took, wrap-up excluded.
    pub rounds: usize,
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
/// `agent` — a workspace on the conversation — adds the whole agent family.
#[allow(clippy::too_many_arguments)]
pub fn offered(
    memorize: bool,
    update: bool,
    delete: bool,
    link: bool,
    unlink: bool,
    remind: bool,
    schedule: bool,
    agent: bool,
) -> Vec<ToolDef> {
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
    if unlink {
        tools.push(unlink_memory_tool());
    }
    if remind {
        tools.push(set_reminder_tool());
    }
    if schedule {
        tools.push(schedule_ask_tool());
    }
    if agent {
        tools.extend(agent::tool_defs());
    }
    tools
}

/// Both times are offered because users phrase it both ways; `in_minutes` is
/// preferred on resolution, needing no reference clock.
pub fn set_reminder_tool() -> ToolDef {
    ToolDef {
        name: SET_REMINDER.to_string(),
        description: "Set a reminder that will be shown back to the user at a \
                      given time."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "What to tell the user when it goes off, in a few \
                                    words: call mum, leave for the airport."
                },
                "in_minutes": {
                    "type": "integer",
                    "description": "Whole minutes from now. Use this whenever the user \
                                    said how long from now: in 20 minutes is 20, in an \
                                    hour and a half is 90."
                },
                "due_at": {
                    "type": "string",
                    "description": "Local date and time as YYYY-MM-DD HH:MM on a 24-hour \
                                    clock. Only for a named day or clock time, and only \
                                    when in_minutes does not fit."
                },
                "every": {
                    "type": "string",
                    "description": "Only when the user asked for a recurring reminder: \
                                    `every day 09:00`, `every monday 9:30`, or \
                                    `every 45m`. With this set, in_minutes and due_at \
                                    may be omitted."
                }
            },
            "required": ["text"]
        }),
    }
}

pub fn schedule_ask_tool() -> ToolDef {
    ToolDef {
        name: SCHEDULE_ASK.to_string(),
        description: "Schedule a prompt that odyn will run on a recurring \
                      schedule, each run landing as a normal conversation."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The question to ask each time, in the user's own \
                                    words."
                },
                "every": {
                    "type": "string",
                    "description": "When to run it: `every day 09:00`, \
                                    `every monday 9:30`, or `every 45m`."
                }
            },
            "required": ["prompt", "every"]
        }),
    }
}

pub fn unlink_memory_tool() -> ToolDef {
    ToolDef {
        name: UNLINK_MEMORY.to_string(),
        description: "Disconnect two memories that should not be related.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "string",
                    "description": "The memory the link is written in — its \
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
/// until the model answers in text. A memory turn whose round of writes all
/// succeeded ends on a synthesized confirmation instead of another request;
/// an agent turn (a workspace in `effects`) never does — it runs long, always
/// feeds results back, and at budget exhaustion or a runaway gets one wrap-up
/// request without tools. An empty `tools` slice makes this exactly one
/// request at the provider's default temperature; tool turns sample at
/// `temperature` instead (`brain.save_temperature`) — saving is
/// transcription, not creativity. Every delta and write reaches `emit`.
pub async fn run_turn(
    provider: &dyn ChatProvider,
    model: &str,
    mut messages: Vec<Message>,
    tools: &[ToolDef],
    effects: &mut Effects<'_>,
    temperature: f32,
    mut emit: impl FnMut(TurnEvent<'_>) -> std::io::Result<()>,
) -> Result<TurnReply, TurnError> {
    let agentic = effects.workspace.is_some();
    let budget = if agentic {
        agent::AGENT_ROUNDS
    } else {
        MAX_TOOL_ROUNDS
    };
    let mut text = String::new();
    let mut usage: Option<Usage> = None;
    let mut saved = Vec::new();
    let mut updated = Vec::new();
    let mut deleted = Vec::new();
    let mut linked = Vec::new();
    let mut unlinked = Vec::new();
    let mut reminders = Vec::new();
    let mut schedules = Vec::new();
    let mut repeats: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut rounds = 0;
    let mut answered = false;
    'rounds: for _ in 0..=budget {
        rounds += 1;
        let (round_text, calls, reported) =
            stream_round(provider, model, &messages, tools, temperature, &mut emit).await?;
        usage = add_usage(usage, reported);
        text.push_str(&round_text);
        if calls.is_empty() {
            answered = true;
            break;
        }
        if agentic {
            emit(TurnEvent::Round {
                used: rounds,
                budget,
            })
            .map_err(TurnError::Write)?;
        }
        messages.push(Message::tool_request(round_text, calls.clone()));
        let mut clean = Vec::new();
        let mut failed = false;
        for call in calls {
            let seen = repeats
                .entry((call.name.clone(), call.arguments.to_string()))
                .or_insert(0);
            *seen += 1;
            let runaway = *seen >= RUNAWAY_CALLS;
            let detail = agent_detail(&call, agentic);
            if let Some(detail) = &detail {
                emit(TurnEvent::AgentCall {
                    tool: &call.name,
                    detail,
                })
                .map_err(TurnError::Write)?;
            }
            let (result, written) = run_tool(effects, &call).await;
            match written {
                Some(Written::Agent { truncated }) => {
                    emit(TurnEvent::AgentOut {
                        text: &result,
                        truncated,
                    })
                    .map_err(TurnError::Write)?;
                }
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
                Some(Written::Unlinked(from, to)) => {
                    emit(TurnEvent::Unlinked {
                        from: &from,
                        to: &to,
                    })
                    .map_err(TurnError::Write)?;
                    unlinked.push((from, to));
                    clean.push(result.clone());
                }
                Some(Written::Reminded(text, due_at)) => {
                    emit(TurnEvent::Reminded {
                        text: &text,
                        due_at,
                    })
                    .map_err(TurnError::Write)?;
                    reminders.push((text, due_at));
                    clean.push(result.clone());
                }
                Some(Written::Scheduled(prompt, next_at)) => {
                    emit(TurnEvent::Scheduled {
                        prompt: &prompt,
                        next_at,
                    })
                    .map_err(TurnError::Write)?;
                    schedules.push((prompt, next_at));
                    clean.push(result.clone());
                }
                None => failed = true,
            }
            messages.push(Message::tool_result(call, result));
            if runaway {
                break 'rounds;
            }
        }
        // A failed call keeps the loop so the model can read the error and
        // retry. A clean round of memory writes ends the turn on Odyn's own
        // confirmation: handing a small model another round over the recalled
        // notes invites it to recite them instead of confirming.
        if agentic || failed || clean.is_empty() {
            continue;
        }
        let confirmation = if text.is_empty() {
            clean.join(" · ")
        } else {
            format!("\n{}", clean.join(" · "))
        };
        emit(TurnEvent::Delta(&confirmation)).map_err(TurnError::Write)?;
        text.push_str(&confirmation);
        answered = true;
        break;
    }
    // The work stopped mid-task: ask the agent to account for itself, with no
    // tools on the table so the answer has to be words.
    if agentic && !answered {
        messages.push(Message::new(Role::User, WRAP_UP));
        let (round_text, _, reported) =
            stream_round(provider, model, &messages, &[], temperature, &mut emit).await?;
        usage = add_usage(usage, reported);
        text.push_str(&round_text);
    }
    Ok(TurnReply {
        text,
        usage,
        saved,
        updated,
        deleted,
        linked,
        unlinked,
        reminders,
        schedules,
        rounds,
    })
}

/// One provider request: deltas emitted as they stream, calls collected.
async fn stream_round(
    provider: &dyn ChatProvider,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    temperature: f32,
    emit: &mut impl FnMut(TurnEvent<'_>) -> std::io::Result<()>,
) -> Result<(String, Vec<ToolCall>, Option<Usage>), TurnError> {
    let mut req = ChatRequest::new(messages, model);
    req.tools = tools;
    if !tools.is_empty() {
        req.temperature = Some(temperature);
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut usage = None;
    let mut stream = provider.chat_stream(req);
    while let Some(event) = stream.next().await {
        match event? {
            ChatEvent::TextDelta(delta) => {
                emit(TurnEvent::Delta(&delta)).map_err(TurnError::Write)?;
                text.push_str(&delta);
            }
            ChatEvent::ToolCall(call) => calls.push(call),
            ChatEvent::Done { usage: reported } => {
                usage = reported;
                break;
            }
        }
    }
    Ok((text, calls, usage))
}

/// The one-line argument an agent call renders as, `None` for memory tools.
fn agent_detail(call: &ToolCall, offered: bool) -> Option<String> {
    if !offered {
        return None;
    }
    let path = || text_arg(call, "path").unwrap_or(".").to_string();
    match call.name.as_str() {
        agent::READ_FILE | agent::WRITE_FILE | agent::EDIT_FILE | agent::LS => Some(path()),
        agent::GLOB => Some(text_arg(call, "pattern").unwrap_or_default().to_string()),
        agent::GREP => {
            let pattern = text_arg(call, "pattern").unwrap_or_default();
            Some(match text_arg(call, "path") {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.to_string(),
            })
        }
        agent::BASH => Some(text_arg(call, "command").unwrap_or_default().to_string()),
        _ => None,
    }
}

/// What a call did, for the trace events.
enum Written {
    Saved(String),
    Updated(String),
    Deleted(String),
    Linked(String, String),
    Unlinked(String, String),
    Reminded(String, i64),
    Scheduled(String, i64),
    /// An agent call, error answers included; its result is the event.
    Agent {
        truncated: bool,
    },
}

/// Answers the call with a result the model can read; a bad call gets its
/// error the same way, never a failed turn. Only bash and its approval await.
async fn run_tool(effects: &mut Effects<'_>, call: &ToolCall) -> (String, Option<Written>) {
    if let Some(workspace) = effects.workspace {
        let flat = |result: String| (result, Some(Written::Agent { truncated: false }));
        match call.name.as_str() {
            agent::READ_FILE => {
                let Some(path) = text_arg(call, "path") else {
                    return flat("error: read_file needs a non-empty string `path`".to_string());
                };
                let offset = int_arg(call, "offset").unwrap_or(0).max(0) as usize;
                let limit = int_arg(call, "limit")
                    .filter(|limit| *limit > 0)
                    .map(|limit| limit as usize);
                return flat(agent::read_file(workspace, path, offset, limit));
            }
            agent::WRITE_FILE => {
                let (Some(path), Some(content)) =
                    (text_arg(call, "path"), call.arguments.get("content"))
                else {
                    return flat(
                        "error: write_file needs a string `path` and a `content`".to_string(),
                    );
                };
                let Some(content) = content.as_str() else {
                    return flat("error: `content` must be a string".to_string());
                };
                return flat(agent::write_file(workspace, path, content));
            }
            agent::EDIT_FILE => {
                let (Some(path), Some(old)) = (text_arg(call, "path"), text_arg(call, "old"))
                else {
                    return flat(
                        "error: edit_file needs non-empty strings `path` and `old`".to_string(),
                    );
                };
                let new = call
                    .arguments
                    .get("new")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                return flat(agent::edit_file(workspace, path, old, new));
            }
            agent::LS => {
                return flat(agent::ls(workspace, text_arg(call, "path").unwrap_or(".")));
            }
            agent::GLOB => {
                let Some(pattern) = text_arg(call, "pattern") else {
                    return flat("error: glob needs a non-empty string `pattern`".to_string());
                };
                return flat(agent::glob(workspace, pattern));
            }
            agent::GREP => {
                let Some(pattern) = text_arg(call, "pattern") else {
                    return flat("error: grep needs a non-empty string `pattern`".to_string());
                };
                return flat(agent::grep(
                    workspace,
                    pattern,
                    text_arg(call, "path").unwrap_or("."),
                ));
            }
            agent::BASH => return bash(workspace, effects, call).await,
            _ => {}
        }
    }
    match call.name.as_str() {
        SAVE_MEMORY => save(effects.brain_dir, call),
        UPDATE_MEMORY => update(effects.brain_dir, call),
        DELETE_MEMORY => delete(effects.brain_dir, call),
        LINK_MEMORY => link(effects.brain_dir, call),
        UNLINK_MEMORY => unlink(effects.brain_dir, call),
        SET_REMINDER => remind(effects, call),
        SCHEDULE_ASK => schedule(effects, call),
        other => (format!("error: no tool named `{other}`"), None),
    }
}

/// Blocklist first — a floor hit is never even asked about — then the user;
/// only an approved command reaches the shell.
async fn bash(
    workspace: &Path,
    effects: &mut Effects<'_>,
    call: &ToolCall,
) -> (String, Option<Written>) {
    let flat = |result: String| (result, Some(Written::Agent { truncated: false }));
    let Some(command) = text_arg(call, "command") else {
        return flat("error: bash needs a non-empty string `command`".to_string());
    };
    if let Some(reason) = agent::blocked(command) {
        return flat(format!(
            "error: this command is blocked and will never run — {reason}"
        ));
    }
    if (effects.approve)(command.to_string()).await == Verdict::Deny {
        return flat("error: the user denied this command".to_string());
    }
    let spawned = tokio::process::Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(err) => return flat(format!("error: could not start the command: {err}")),
    };
    let output = match tokio::time::timeout(BASH_TIMEOUT, child.wait_with_output()).await {
        // The timed-out future owns the child; dropping it kills the process.
        Err(_) => {
            return flat(format!(
                "error: the command was killed after {} s",
                BASH_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(err)) => return flat(format!("error: the command failed to run: {err}")),
        Ok(Ok(output)) => output,
    };
    let mut merged = String::from_utf8_lossy(&output.stdout).into_owned();
    merged.push_str(&String::from_utf8_lossy(&output.stderr));
    let mut truncated = false;
    if merged.len() > agent::OUT_CAP {
        let mut cut = merged.len() - agent::OUT_CAP;
        while !merged.is_char_boundary(cut) {
            cut += 1;
        }
        // The tail is kept: errors and summaries land at the end.
        merged = format!(
            "[truncated to the last {} KB]\n{}",
            agent::OUT_CAP / 1024,
            &merged[cut..]
        );
        truncated = true;
    }
    if !output.status.success() {
        let status = match output.status.code() {
            Some(code) => format!("[exit status: {code}]"),
            None => "[killed by a signal]".to_string(),
        };
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(&status);
    }
    if merged.is_empty() {
        merged = "(no output)".to_string();
    }
    (merged, Some(Written::Agent { truncated }))
}

/// Written before the model is told, so a confirmation never promises a
/// reminder the database refused.
fn remind(effects: &mut Effects<'_>, call: &ToolCall) -> (String, Option<Written>) {
    let Some(text) = text_arg(call, "text") else {
        return (
            "error: set_reminder needs a non-empty string `text`".to_string(),
            None,
        );
    };
    let every = match text_arg(call, "every")
        .map(reminder::parse_repeat)
        .transpose()
    {
        Ok(every) => every,
        Err(err) => return (format!("error: {err}"), None),
    };
    let now = now_secs();
    let timed = int_arg(call, "in_minutes").is_some() || text_arg(call, "due_at").is_some();
    // A repeat alone fixes the first firing; an explicit time overrides it.
    let due = match (&every, timed) {
        (Some(repeat), false) => match reminder::next_fire(repeat, now) {
            Some(due) => due,
            None => {
                return (
                    "error: could not work out the first firing time".to_string(),
                    None,
                )
            }
        },
        _ => {
            match reminder::resolve_due(now, int_arg(call, "in_minutes"), text_arg(call, "due_at"))
            {
                Ok(due) => due,
                Err(err) => return (format!("error: {err}"), None),
            }
        }
    };
    let phrase = every.as_ref().map(reminder::Repeat::canonical);
    match (effects.set_reminder)(text, due, phrase.as_deref()) {
        Ok(_) => {
            let stamp = reminder::local_stamp(due);
            let mut when = String::new();
            if !stamp.is_empty() {
                when.push_str(&format!(" for {stamp}"));
            }
            if let Some(every) = &phrase {
                when.push_str(&format!(", {every}"));
            }
            (
                format!("reminder set{when}: {text}"),
                Some(Written::Reminded(text.to_string(), due)),
            )
        }
        Err(err) => (format!("error: {err}"), None),
    }
}

/// Stored before the model is told, like a reminder. The first run comes from
/// the phrase alone; the sink is where the surface refuses what it must.
fn schedule(effects: &mut Effects<'_>, call: &ToolCall) -> (String, Option<Written>) {
    let (Some(prompt), Some(every)) = (text_arg(call, "prompt"), text_arg(call, "every")) else {
        return (
            "error: schedule_ask needs non-empty strings `prompt` and `every`".to_string(),
            None,
        );
    };
    let repeat = match reminder::parse_repeat(every) {
        Ok(repeat) => repeat,
        Err(err) => return (format!("error: {err}"), None),
    };
    let Some(next) = reminder::next_fire(&repeat, now_secs()) else {
        return (
            "error: could not work out the first run time".to_string(),
            None,
        );
    };
    match (effects.set_schedule)(prompt, &repeat.canonical(), next) {
        Ok(_) => {
            let stamp = reminder::local_stamp(next);
            (
                format!(
                    "scheduled {}, first run {stamp}: {prompt}",
                    repeat.canonical()
                ),
                Some(Written::Scheduled(prompt.to_string(), next)),
            )
        }
        Err(err) => (format!("error: {err}"), None),
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

/// A pair that was not linked is a success too: the folder ends up the way the
/// user asked, which is all the turn was for.
fn unlink(brain_dir: &Path, call: &ToolCall) -> (String, Option<Written>) {
    let (Some(from), Some(to)) = (text_arg(call, "from"), text_arg(call, "to")) else {
        return (
            "error: unlink_memory needs non-empty strings `from` and `to`".to_string(),
            None,
        );
    };
    match notes::unlink_note(brain_dir, from, to) {
        Ok(removed) => (
            if removed {
                format!("unlinked {from} from {to}")
            } else {
                format!("{from} does not link to {to}")
            },
            Some(Written::Unlinked(from.to_string(), to.to_string())),
        ),
        Err(err) => (format!("error: {err}"), None),
    }
}

#[cfg(test)]
/// A reminder call in a folder-only test would be a routing bug.
fn refuse_reminders() -> impl FnMut(&str, i64, Option<&str>) -> Result<i64, String> {
    |_, _, _| Err("this turn should not have set a reminder".to_string())
}

#[cfg(test)]
/// A schedule call outside a schedule test would be a routing bug too.
fn refuse_schedules() -> impl FnMut(&str, &str, i64) -> Result<i64, String> {
    |_, _, _| Err("this turn should not have scheduled anything".to_string())
}

#[cfg(test)]
/// An approval request outside a bash test would be a routing bug.
fn refuse_bash() -> impl FnMut(String) -> BoxFuture<'static, Verdict> + Send {
    |command: String| -> BoxFuture<'static, Verdict> {
        panic!("this turn should not have asked to run bash: {command}")
    }
}

fn text_arg<'a>(call: &'a ToolCall, name: &str) -> Option<&'a str> {
    call.arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Small models quote numbers as often as they send them, and some send a
/// whole number as a float; all three read the same.
fn int_arg(call: &ToolCall, name: &str) -> Option<i64> {
    let value = call.arguments.get(name)?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse().ok())
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
            Written::Linked(from, to) | Written::Unlinked(from, to) => format!("{from}->{to}"),
            Written::Reminded(text, due_at) | Written::Scheduled(text, due_at) => {
                format!("{text}@{due_at}")
            }
            Written::Agent { truncated } => format!("agent(truncated={truncated})"),
        })
    }

    fn label(event: TurnEvent<'_>) -> String {
        match event {
            TurnEvent::Delta(delta) => format!("delta:{delta}"),
            TurnEvent::Saved(slug) => format!("saved:{slug}"),
            TurnEvent::Updated(slug) => format!("updated:{slug}"),
            TurnEvent::Deleted(slug) => format!("deleted:{slug}"),
            TurnEvent::Linked { from, to } => format!("linked:{from}->{to}"),
            TurnEvent::Unlinked { from, to } => format!("unlinked:{from}->{to}"),
            TurnEvent::Reminded { text, due_at } => format!("reminded:{text}@{due_at}"),
            TurnEvent::Scheduled { prompt, next_at } => format!("scheduled:{prompt}@{next_at}"),
            TurnEvent::AgentCall { tool, detail } => format!("agent:{tool}:{detail}"),
            TurnEvent::AgentOut { text, truncated } => {
                format!("out({truncated}):{}", text.lines().next().unwrap_or(""))
            }
            TurnEvent::Round { used, budget } => format!("round:{used}/{budget}"),
        }
    }

    /// A clean write ends the turn on Odyn's own confirmation: one request,
    /// and the model never speaks over the recalled notes again.
    #[test]
    fn a_save_call_writes_the_note_and_ends_the_turn_with_its_own_confirmation() {
        let dir = TempDir::new("save");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
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
            &mut effects,
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
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
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
            &mut effects,
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
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
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
            &mut effects,
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
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        notes::write_note(&dir.0, Some("espresso"), "already here").expect("seed");
        let (result, written) = block_on(run_tool(
            &mut effects,
            &call(serde_json::json!({"content": "Fresh note.", "slug": "espresso"})),
        ));
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
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
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
            &mut effects,
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
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        notes::write_note(&dir.0, Some("car-keys"), "on the desk").expect("seed");
        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(DELETE_MEMORY, serde_json::json!({"slug": "car-keys"})),
        ));
        assert_eq!(slug_of(written).as_deref(), Some("car-keys"));
        assert_eq!(result, "deleted car-keys");
        assert!(!dir.0.join("car-keys.md").exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join(".trash").join("car-keys.md")).expect("kept"),
            "on the desk\n"
        );

        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(DELETE_MEMORY, serde_json::json!({"slug": "ghost"})),
        ));
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
    }

    /// A second call for a pair that is already connected still confirms: the
    /// folder is where it ends up that matters, not who wrote the link.
    #[test]
    fn a_link_call_connects_the_pair_and_repeats_are_not_errors() {
        let dir = TempDir::new("link");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
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
            &mut effects,
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

        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                LINK_MEMORY,
                serde_json::json!({"from": "football", "to": "mitul"}),
            ),
        ));
        assert_eq!(slug_of(written).as_deref(), Some("football->mitul"));
        assert_eq!(result, "football already links to mitul");

        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                LINK_MEMORY,
                serde_json::json!({"from": "ghost", "to": "mitul"}),
            ),
        ));
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
    }

    #[test]
    fn an_unlink_call_removes_the_edge_and_a_missing_one_is_not_an_error() {
        let dir = TempDir::new("unlink");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        notes::write_note(&dir.0, Some("football"), "He plays on Sundays.").expect("seed");
        notes::write_note(&dir.0, Some("mitul"), "The user.").expect("seed");
        notes::link_note(&dir.0, "football", "mitul").expect("seed link");
        let asked = named_call(
            UNLINK_MEMORY,
            serde_json::json!({"from": "football", "to": "mitul"}),
        );
        let provider = Scripted::new(vec![vec![Ok(ChatEvent::ToolCall(asked)), done()]]);
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "those two are unrelated")],
            &[unlink_memory_tool()],
            &mut effects,
            0.3,
            |event| {
                seen.push(label(event));
                Ok(())
            },
        ))
        .expect("turn");

        assert_eq!(
            reply.unlinked,
            vec![("football".to_string(), "mitul".to_string())]
        );
        assert!(reply.linked.is_empty());
        assert_eq!(reply.text, "unlinked football from mitul");
        assert_eq!(
            seen,
            vec![
                "unlinked:football->mitul",
                "delta:unlinked football from mitul"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("football.md")).expect("note"),
            "He plays on Sundays.\n"
        );

        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                UNLINK_MEMORY,
                serde_json::json!({"from": "football", "to": "mitul"}),
            ),
        ));
        assert_eq!(slug_of(written).as_deref(), Some("football->mitul"));
        assert_eq!(result, "football does not link to mitul");

        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                UNLINK_MEMORY,
                serde_json::json!({"from": "ghost", "to": "mitul"}),
            ),
        ));
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
    }

    #[test]
    fn an_update_of_a_missing_note_answers_with_an_error() {
        let dir = TempDir::new("update-missing");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                UPDATE_MEMORY,
                serde_json::json!({"slug": "nowhere", "content": "anything"}),
            ),
        ));
        assert!(slug_of(written).is_none());
        assert!(result.starts_with("error:"), "{result}");
        assert_eq!(std::fs::read_dir(&dir.0).expect("dir").count(), 0);
    }

    #[test]
    fn a_reminder_is_stored_before_it_is_confirmed() {
        let dir = TempDir::new("remind");
        let mut stored: Vec<(String, i64)> = Vec::new();
        let asked = named_call(
            SET_REMINDER,
            serde_json::json!({"text": "call mum", "in_minutes": "30"}),
        );
        let provider = Scripted::new(vec![vec![Ok(ChatEvent::ToolCall(asked)), done()]]);
        let mut seen = Vec::new();
        let reply = {
            let mut sink = |text: &str, due_at: i64, _: Option<&str>| {
                stored.push((text.to_string(), due_at));
                Ok(1)
            };
            let mut plans = refuse_schedules();
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_turn(
                &provider,
                "llama3.2:3b",
                vec![Message::new(Role::User, "remind me to call mum in 30")],
                &[set_reminder_tool()],
                &mut effects,
                0.3,
                |event| {
                    seen.push(label(event));
                    Ok(())
                },
            ))
            .expect("turn")
        };

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, "call mum");
        assert_eq!(reply.reminders.len(), 1);
        assert!(reply.text.starts_with("reminder set"), "{}", reply.text);
        assert!(seen.iter().any(|event| event.starts_with("reminded:")));
    }

    /// `every` alone is a complete call: the first firing comes from the
    /// phrase, and the stored row carries its canonical form.
    #[test]
    fn a_repeating_reminder_needs_no_time_and_carries_its_phrase() {
        let dir = TempDir::new("remind-every");
        let mut stored: Vec<(i64, Option<String>)> = Vec::new();
        let before = now_secs();
        let (result, written) = {
            let mut sink = |_: &str, due_at: i64, every: Option<&str>| {
                stored.push((due_at, every.map(str::to_string)));
                Ok(1)
            };
            let mut plans = refuse_schedules();
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_tool(
                &mut effects,
                &named_call(
                    SET_REMINDER,
                    serde_json::json!({"text": "stand up", "every": "every 45m"}),
                ),
            ))
        };
        assert!(written.is_some());
        assert!(result.contains("every 45m"), "{result}");
        assert_eq!(stored[0].1.as_deref(), Some("every 45m"));
        assert!(stored[0].0 >= before + 45 * 60, "{}", stored[0].0);

        let mut calls = 0;
        let bad = {
            let mut sink = |_: &str, _: i64, _: Option<&str>| {
                calls += 1;
                Ok(1)
            };
            let mut plans = refuse_schedules();
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_tool(
                &mut effects,
                &named_call(
                    SET_REMINDER,
                    serde_json::json!({"text": "x", "every": "every fortnight"}),
                ),
            ))
            .0
        };
        assert!(bad.starts_with("error:"), "{bad}");
        assert_eq!(calls, 0);
    }

    /// The phrase is validated and canonicalized before the sink sees it, and
    /// a refusal from the sink comes back as a readable error.
    #[test]
    fn a_schedule_call_is_stored_canonically_or_refused_readably() {
        let dir = TempDir::new("schedule");
        let mut stored: Vec<(String, String, i64)> = Vec::new();
        let before = now_secs();
        let (result, written) = {
            let mut sink = refuse_reminders();
            let mut plans = |prompt: &str, every: &str, next_at: i64| {
                stored.push((prompt.to_string(), every.to_string(), next_at));
                Ok(1)
            };
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_tool(
                &mut effects,
                &named_call(
                    SCHEDULE_ASK,
                    serde_json::json!({"prompt": "brief me", "every": "Every Day 9:00"}),
                ),
            ))
        };
        assert!(written.is_some());
        assert!(result.starts_with("scheduled every day 09:00"), "{result}");
        assert_eq!(stored[0].0, "brief me");
        assert_eq!(stored[0].1, "every day 09:00");
        assert!(stored[0].2 > before);

        let refused = {
            let mut sink = refuse_reminders();
            let mut plans =
                |_: &str, _: &str, _: i64| Err("a scheduled ask cannot save memories".to_string());
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_tool(
                &mut effects,
                &named_call(
                    SCHEDULE_ASK,
                    serde_json::json!({"prompt": "/memory save this", "every": "every 45m"}),
                ),
            ))
            .0
        };
        assert!(refused.starts_with("error:"), "{refused}");

        let bad = {
            let mut sink = refuse_reminders();
            let mut plans = refuse_schedules();
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            block_on(run_tool(
                &mut effects,
                &named_call(SCHEDULE_ASK, serde_json::json!({"prompt": "brief me"})),
            ))
            .0
        };
        assert!(bad.starts_with("error:"), "{bad}");
    }

    #[test]
    fn a_reminder_the_clock_refuses_never_reaches_the_store() {
        let dir = TempDir::new("remind-bad");
        let mut calls = 0;
        let (result, written, missing) = {
            let mut sink = |_: &str, _: i64, _: Option<&str>| {
                calls += 1;
                Ok(1)
            };
            let mut plans = refuse_schedules();
            let mut gate = refuse_bash();
            let mut effects = Effects {
                brain_dir: &dir.0,
                workspace: None,
                set_reminder: &mut sink,
                set_schedule: &mut plans,
                approve: &mut gate,
            };
            let (result, written) = block_on(run_tool(
                &mut effects,
                &named_call(
                    SET_REMINDER,
                    serde_json::json!({"text": "too late", "due_at": "2020-01-01 09:00"}),
                ),
            ));
            let (missing, _) = block_on(run_tool(
                &mut effects,
                &named_call(SET_REMINDER, serde_json::json!({"text": "when?"})),
            ));
            (result, written, missing)
        };
        assert!(written.is_none());
        assert!(result.starts_with("error:"), "{result}");
        assert!(missing.starts_with("error:"), "{missing}");
        assert_eq!(calls, 0);
    }

    fn answer(verdict: Verdict) -> impl FnMut(String) -> BoxFuture<'static, Verdict> + Send {
        move |_| Box::pin(async move { verdict })
    }

    fn agent_call(name: &str, arguments: serde_json::Value) -> Result<ChatEvent, ChatError> {
        Ok(ChatEvent::ToolCall(named_call(name, arguments)))
    }

    /// The agent loop: results feed back, no confirmation is synthesized, and
    /// the turn runs past the 4-round memory budget.
    #[test]
    fn an_agent_turn_feeds_results_back_and_runs_past_the_memory_budget() {
        let dir = TempDir::new("agent-loop");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: Some(&dir.0),
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let provider = Scripted::new(vec![
            vec![
                agent_call(
                    agent::WRITE_FILE,
                    serde_json::json!({"path": "notes/plan.md", "content": "step one"}),
                ),
                done(),
            ],
            vec![
                agent_call(
                    agent::READ_FILE,
                    serde_json::json!({"path": "notes/plan.md"}),
                ),
                done(),
            ],
            vec![agent_call(agent::LS, serde_json::json!({})), done()],
            vec![
                agent_call(agent::GLOB, serde_json::json!({"pattern": "**/*.md"})),
                done(),
            ],
            vec![
                agent_call(
                    agent::GREP,
                    serde_json::json!({"pattern": "STEP", "path": "notes"}),
                ),
                done(),
            ],
            vec![
                agent_call(
                    agent::EDIT_FILE,
                    serde_json::json!({"path": "notes/plan.md", "old": "one", "new": "two"}),
                ),
                done(),
            ],
            vec![Ok(ChatEvent::TextDelta("All done.".to_string())), done()],
        ]);
        let mut seen = Vec::new();
        let reply = block_on(run_turn(
            &provider,
            "qwen3:8b",
            vec![Message::new(Role::User, "make a plan file")],
            &agent::tool_defs(),
            &mut effects,
            0.3,
            |event| {
                seen.push(label(event));
                Ok(())
            },
        ))
        .expect("turn");

        assert_eq!(reply.text, "All done.", "nothing synthesized");
        assert_eq!(reply.rounds, 7);
        assert_eq!(
            std::fs::read_to_string(dir.0.join("notes/plan.md")).expect("note"),
            "step two"
        );
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 7, "past the memory budget of 4");
        // Every result went back to the model as a tool message.
        let results: Vec<&str> = requests[6]
            .0
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(results[0], "wrote 8 bytes to notes/plan.md");
        assert_eq!(results[1], "step one\n");
        assert_eq!(results[2], "notes/");
        assert_eq!(results[3], "notes/plan.md");
        assert_eq!(results[4], "notes/plan.md:1: step one");
        assert_eq!(results[5], "edited notes/plan.md");
        assert!(seen.contains(&"round:1/30".to_string()), "{seen:?}");
        assert!(seen.contains(&"round:6/30".to_string()), "{seen:?}");
        assert!(
            seen.contains(&"agent:read_file:notes/plan.md".to_string()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"out(false):step one".to_string()),
            "{seen:?}"
        );
    }

    /// The same call with the same arguments 15 times is a stuck model: the
    /// loop stops and asks for a plain-text accounting, with no tools offered.
    #[test]
    fn a_runaway_call_trips_the_wrap_up() {
        let dir = TempDir::new("runaway");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: Some(&dir.0),
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let mut rounds: Vec<Vec<Result<ChatEvent, ChatError>>> = (0..15)
            .map(|_| vec![agent_call(agent::LS, serde_json::json!({})), done()])
            .collect();
        rounds.push(vec![
            Ok(ChatEvent::TextDelta(
                "I kept listing the folder.".to_string(),
            )),
            done(),
        ]);
        let provider = Scripted::new(rounds);
        let reply = block_on(run_turn(
            &provider,
            "qwen3:8b",
            vec![Message::new(Role::User, "loop please")],
            &agent::tool_defs(),
            &mut effects,
            0.3,
            |_| Ok(()),
        ))
        .expect("turn");

        assert_eq!(reply.text, "I kept listing the folder.");
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 16, "15 stuck rounds and one wrap-up");
        let wrap_up = requests.last().expect("wrap-up request");
        assert_eq!(wrap_up.1, 0, "the wrap-up offers no tools");
        let last = wrap_up.0.last().expect("wrap-up message");
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content, WRAP_UP);
    }

    #[test]
    fn a_denied_command_answers_the_model_without_running() {
        let dir = TempDir::new("deny");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut asked = Vec::new();
        let mut gate = |command: String| -> BoxFuture<'static, Verdict> {
            asked.push(command);
            Box::pin(async { Verdict::Deny })
        };
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: Some(&dir.0),
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let provider = Scripted::new(vec![
            vec![
                agent_call(
                    agent::BASH,
                    serde_json::json!({"command": "touch made.txt"}),
                ),
                done(),
            ],
            vec![
                Ok(ChatEvent::TextDelta("Understood, skipping it.".to_string())),
                done(),
            ],
        ]);
        let reply = block_on(run_turn(
            &provider,
            "qwen3:8b",
            vec![Message::new(Role::User, "touch a file")],
            &agent::tool_defs(),
            &mut effects,
            0.3,
            |_| Ok(()),
        ))
        .expect("turn");

        assert_eq!(reply.text, "Understood, skipping it.");
        assert_eq!(asked, vec!["touch made.txt".to_string()]);
        assert!(!dir.0.join("made.txt").exists(), "deny must not run it");
        let requests = provider.requests.lock().expect("requests");
        let result = requests[1]
            .0
            .iter()
            .find(|message| message.role == Role::Tool)
            .expect("tool result");
        assert_eq!(result.content, "error: the user denied this command");
    }

    /// A floor hit answers the model directly; `refuse_bash` panics if the
    /// approver is ever consulted.
    #[test]
    fn a_blocked_command_never_reaches_the_approver() {
        let dir = TempDir::new("blocked");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: Some(&dir.0),
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(agent::BASH, serde_json::json!({"command": "sudo rm -rf /"})),
        ));
        assert!(
            result.starts_with("error: this command is blocked"),
            "{result}"
        );
        assert!(matches!(written, Some(Written::Agent { truncated: false })));
    }

    #[tokio::test]
    async fn an_approved_command_runs_in_the_workspace() {
        let dir = TempDir::new("bash");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = answer(Verdict::Run);
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: Some(&dir.0),
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let (result, written) = run_tool(
            &mut effects,
            &named_call(agent::BASH, serde_json::json!({"command": "pwd"})),
        )
        .await;
        assert!(matches!(written, Some(Written::Agent { truncated: false })));
        let root = dir.0.canonicalize().expect("root");
        assert_eq!(result.trim(), root.to_str().expect("utf8"));

        let (result, _) = run_tool(
            &mut effects,
            &named_call(
                agent::BASH,
                serde_json::json!({"command": "echo warned >&2; exit 3"}),
            ),
        )
        .await;
        assert!(result.contains("warned"), "{result}");
        assert!(result.ends_with("[exit status: 3]"), "{result}");

        let (result, _) = run_tool(
            &mut effects,
            &named_call(agent::BASH, serde_json::json!({"command": "true"})),
        )
        .await;
        assert_eq!(result, "(no output)");
    }

    #[test]
    fn a_memory_mention_and_a_workspace_offer_both_families() {
        let both = offered(true, false, false, false, false, false, false, true);
        let names: Vec<&str> = both.iter().map(|tool| tool.name.as_str()).collect();
        assert!(names.contains(&SAVE_MEMORY));
        assert!(names.contains(&agent::BASH));
        assert!(names.contains(&agent::READ_FILE));
        assert_eq!(both.len(), 1 + agent::tool_defs().len());
        assert!(offered(false, false, false, false, false, false, false, false).is_empty());
    }

    /// Without a workspace the agent names are not tools at all: the model is
    /// answered, the filesystem is never touched.
    #[test]
    fn agent_calls_without_a_workspace_answer_as_unknown_tools() {
        let dir = TempDir::new("no-workspace");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let (result, written) = block_on(run_tool(
            &mut effects,
            &named_call(
                agent::WRITE_FILE,
                serde_json::json!({"path": "x", "content": "y"}),
            ),
        ));
        assert!(result.starts_with("error: no tool named"), "{result}");
        assert!(written.is_none());
        assert!(!dir.0.join("x").exists());
    }

    #[test]
    fn no_tools_means_exactly_one_request() {
        let dir = TempDir::new("plain");
        let mut sink = refuse_reminders();
        let mut plans = refuse_schedules();
        let mut gate = refuse_bash();
        let mut effects = Effects {
            brain_dir: &dir.0,
            workspace: None,
            set_reminder: &mut sink,
            set_schedule: &mut plans,
            approve: &mut gate,
        };
        let provider = Scripted::new(vec![vec![
            Ok(ChatEvent::TextDelta("hi".to_string())),
            done(),
        ]]);
        let reply = block_on(run_turn(
            &provider,
            "llama3.2:3b",
            vec![Message::new(Role::User, "hello")],
            &[],
            &mut effects,
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
