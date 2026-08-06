//! `odyn ask`: one question, streamed to stdout, then exit.

use std::io::Write;

use odyn_core::chat::{Message, Role};
use odyn_core::storage::Storage;

use crate::session::{
    memory_context, print_context, save_turn, stream_reply, title_from, warn, with_context,
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

    // `--save` needs the database and creates it; a plain ask only reads
    // memory, so it opens the database if one exists and otherwise stays
    // ephemeral — asking must not conjure a database.
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
        &session.memory,
        &[],
        prompt,
        session.brevity,
    );
    if show_context {
        print_context(context.as_ref(), &session.memory, json)?;
    }
    let messages = with_context(context.as_ref(), &[Message::new(Role::User, prompt)]);

    let mut out = anstream::stdout().lock();
    let mut streamed = false;
    let reply = stream_reply(
        session.handle.as_ref(),
        &session.model,
        &messages,
        |delta| {
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
            out.flush()
        },
    )
    .await;

    let reply = match reply {
        Ok(reply) => reply,
        Err(failure) => {
            // Keep the partial answer, but end its line before the error.
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
        persist(&storage, &session, prompt, &reply, &injected)?;
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
