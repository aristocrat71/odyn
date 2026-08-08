//! `odyn ask`: one question, streamed to stdout, then exit.

use std::io::Write;

use odyn_core::chat::{Message, Role};
use odyn_core::storage::Storage;

use odyn_core::tools::TurnEvent;

use crate::session::{
    memory_context, print_context, save_turn, stream_reply, title_from, trace, warn, with_context,
    write_failure, Failure, Reply, Session,
};

pub async fn run(
    session: Session,
    prompt: Option<String>,
    json: bool,
    save: bool,
    show_context: bool,
) -> Result<(), Failure> {
    let prompt = match prompt {
        Some(prompt) => prompt,
        None => std::io::read_to_string(std::io::stdin())
            .map_err(|err| Failure::run(format!("could not read stdin: {err}")))?,
    };
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(Failure::config(
            "no prompt: pass one as an argument or on stdin",
        ));
    }
    // A `/brain` mention turns recall on and never reaches the model or the
    // transcript.
    let ask = odyn_core::brain::parse_ask(prompt);

    // `--save` creates the database; a plain ask must not conjure one, so it
    // opens an existing one or stays ephemeral.
    let storage = if save {
        match Storage::open_default() {
            Ok(storage) => Some(storage),
            Err(err) => return Err(Failure::run(format!("could not open the database: {err}"))),
        }
    } else {
        match Storage::open_default_existing() {
            Ok(storage) => storage,
            Err(err) => {
                warn(&format!("memory unavailable: {err}"));
                None
            }
        }
    };
    let context = memory_context(
        storage.as_ref(),
        &session.config,
        &[],
        &ask,
        session.brevity,
    );
    if show_context {
        print_context(context.as_ref(), &session.config.brain, json)?;
    }
    let messages = with_context(
        context.as_ref(),
        &[Message::new(Role::User, ask.message.as_str())],
    );

    let mut out = anstream::stdout().lock();
    let mut streamed = false;
    let reply = stream_reply(
        session.handle.as_ref(),
        &session.model,
        messages,
        &session.config,
        &ask,
        |event| {
            match event {
                TurnEvent::Delta(delta) => {
                    streamed = true;
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "delta", "text": delta})
                        )?;
                    } else {
                        write!(out, "{delta}")?;
                    }
                }
                TurnEvent::Saved(slug) => {
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "saved", "slug": slug})
                        )?;
                    } else {
                        trace(&format!("saved {slug}"));
                    }
                }
                TurnEvent::Updated(slug) => {
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "updated", "slug": slug})
                        )?;
                    } else {
                        trace(&format!("updated {slug}"));
                    }
                }
                TurnEvent::Deleted(slug) => {
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "deleted", "slug": slug})
                        )?;
                    } else {
                        trace(&format!("deleted {slug}"));
                    }
                }
                TurnEvent::Linked { from, to } => {
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "linked", "from": from, "to": to})
                        )?;
                    } else {
                        trace(&format!("linked {from} to {to}"));
                    }
                }
                TurnEvent::Unlinked { from, to } => {
                    if json {
                        writeln!(
                            out,
                            "{}",
                            serde_json::json!({"type": "unlinked", "from": from, "to": to})
                        )?;
                    } else {
                        trace(&format!("unlinked {from} from {to}"));
                    }
                }
            }
            out.flush()
        },
    )
    .await;

    let reply = match reply {
        Ok(reply) => reply,
        Err(failure) => {
            if streamed && !json {
                let _ = writeln!(out);
            }
            return Err(failure);
        }
    };

    if json {
        writeln!(
            out,
            "{}",
            serde_json::json!({"type": "done", "usage": reply.usage})
        )
    } else {
        writeln!(out)
    }
    .map_err(write_failure)?;

    if save {
        let Some(storage) = storage else {
            return Err(Failure::run("could not save: the database is unavailable"));
        };
        let injected = context
            .map(|context| context.memory_ids())
            .unwrap_or_default();
        persist(&storage, &session, &ask.message, &reply, &injected)?;
    }
    Ok(())
}

fn persist(
    storage: &Storage,
    session: &Session,
    prompt: &str,
    reply: &Reply,
    injected: &[i64],
) -> Result<(), Failure> {
    let saved = |err| Failure::run(format!("could not save the conversation: {err}"));
    let conversation = storage
        .create_conversation(&title_from(prompt), &session.provider, &session.model)
        .map_err(saved)?;
    save_turn(storage, conversation.id, prompt, reply, injected).map_err(saved)
}
