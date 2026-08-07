//! `odyn chat`: a REPL over the same streaming path as `odyn ask`. Nothing
//! inside the loop is fatal — a failure is reported and the prompt comes back.

use std::io::Write;

use odyn_core::chat::{Message, Role};
use odyn_core::storage::{Storage, StorageError};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use tokio::runtime::Runtime;

use odyn_core::tools::TurnEvent;

use crate::session::{
    memory_context, print_context, save_turn, stream_reply, title_from, trace, warn, with_context,
    write_failure, Failure, Reply, Session,
};

const PROMPT: &str = "odyn> ";

/// One saved exchange: the prompt, the reply, and what was injected for it.
type Turn = (String, Reply, Vec<i64>);

pub fn run(runtime: &Runtime, mut session: Session, show_context: bool) -> Result<(), Failure> {
    let storage = Storage::open_default()
        .map_err(|err| Failure::run(format!("could not open the database: {err}")))?;
    let mut editor = DefaultEditor::new()
        .map_err(|err| Failure::run(format!("could not start the line editor: {err}")))?;
    let mut out = anstream::stdout();
    let mut history: Vec<Message> = Vec::new();
    let mut conversation: Option<i64> = None;
    // `/brevity` switches it mid-session; each send uses the value at send time.
    let mut brevity = session.brevity;
    // Turns a failed save left unwritten, oldest first.
    let mut pending: Vec<Turn> = Vec::new();

    writeln!(out, "{} / {}", session.provider, session.model).map_err(write_failure)?;
    loop {
        let line = match editor.readline(PROMPT) {
            Ok(line) => line,
            // Ctrl-C already cleared the line; Ctrl-D leaves.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(err) => return Err(Failure::run(format!("line editor failed: {err}"))),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line);

        // `/brain`, `/memory` and `/update-memory` look like commands but are
        // chat messages, so they must never fall into the command parser.
        let ask = odyn_core::brain::parse_ask(line);
        if let Some(command) = line
            .strip_prefix('/')
            .filter(|_| !ask.recall && !ask.memorize && !ask.update)
        {
            let (name, arg) = match command.split_once(char::is_whitespace) {
                Some((name, arg)) => (name, arg.trim()),
                None => (command, ""),
            };
            match name {
                "quit" => break,
                "new" => {
                    // Unwritten turns belong to the conversation being left.
                    if let Err(err) = record(
                        &storage,
                        &mut conversation,
                        &session.provider,
                        &session.model,
                        &mut pending,
                    ) {
                        warn(&format!("could not save the conversation: {err}"));
                    }
                    conversation = None;
                    history.clear();
                    writeln!(out, "new conversation").map_err(write_failure)?;
                }
                "model" if arg.is_empty() => {
                    writeln!(out, "{} / {}", session.provider, session.model)
                        .map_err(write_failure)?;
                }
                "model" => {
                    let (provider, model) = split_model(&session, arg);
                    if let Err(failure) = session.switch(provider, model) {
                        warn(&failure.message);
                        continue;
                    }
                    if let Some(id) = conversation {
                        if let Err(err) =
                            storage.set_conversation_model(id, &session.provider, &session.model)
                        {
                            warn(&format!("could not save the model: {err}"));
                        }
                    }
                    writeln!(out, "{} / {}", session.provider, session.model)
                        .map_err(write_failure)?;
                }
                "brevity" if arg.is_empty() => {
                    writeln!(out, "{brevity}").map_err(write_failure)?;
                }
                "brevity" => match arg.parse() {
                    Ok(level) => {
                        brevity = level;
                        writeln!(out, "{brevity}").map_err(write_failure)?;
                    }
                    Err(err) => warn(&err.to_string()),
                },
                _ => warn(&format!(
                    "unknown command: /{name}; try /model, /brevity, /new, /quit — \
                     or mention /brain to recall memory, /memory to save one, \
                     /update-memory to rewrite one"
                )),
            }
            continue;
        }

        let context = memory_context(Some(&storage), &session.config, &history, &ask, brevity);
        if show_context {
            print_context(context.as_ref(), &session.config.brain, false)?;
        }
        history.push(Message::new(Role::User, ask.message.as_str()));
        let outgoing = with_context(context.as_ref(), &history);
        let mut streamed = false;
        let reply = runtime.block_on(stream_reply(
            session.handle.as_ref(),
            &session.model,
            outgoing,
            &session.config,
            ask.memorize,
            ask.update,
            |event| {
                match event {
                    TurnEvent::Delta(delta) => {
                        streamed = true;
                        write!(out, "{delta}")?;
                    }
                    TurnEvent::Saved(slug) => trace(&format!("saved {slug}")),
                    TurnEvent::Updated(slug) => trace(&format!("updated {slug}")),
                }
                out.flush()
            },
        ));
        let reply = match reply {
            Ok(reply) => reply,
            Err(failure) => {
                history.pop();
                if streamed {
                    let _ = writeln!(out);
                }
                warn(&failure.message);
                continue;
            }
        };
        writeln!(out, "\n").map_err(write_failure)?;

        history.push(Message::new(Role::Assistant, reply.text.as_str()));
        let injected = context
            .map(|context| context.memory_ids())
            .unwrap_or_default();
        pending.push((ask.message, reply, injected));
        if let Err(err) = record(
            &storage,
            &mut conversation,
            &session.provider,
            &session.model,
            &mut pending,
        ) {
            warn(&format!("could not save the conversation: {err}"));
        }
    }
    Ok(())
}

/// Named after the oldest turn still waiting to be written: a save that failed
/// once must not leave its turns behind in a second conversation.
fn record(
    storage: &Storage,
    conversation: &mut Option<i64>,
    provider: &str,
    model: &str,
    pending: &mut Vec<Turn>,
) -> Result<(), StorageError> {
    let Some((first, _, _)) = pending.first() else {
        return Ok(());
    };
    let id = match *conversation {
        Some(id) => id,
        None => {
            let created = storage.create_conversation(&title_from(first), provider, model)?;
            *conversation = Some(created.id);
            created.id
        }
    };
    while !pending.is_empty() {
        let (prompt, reply, injected) = &pending[0];
        save_turn(storage, id, prompt, reply, injected)?;
        pending.remove(0);
    }
    Ok(())
}

/// `provider/model` only when the left side names a configured provider:
/// model names carry slashes of their own.
fn split_model(session: &Session, arg: &str) -> (Option<String>, String) {
    match arg.split_once('/') {
        Some((provider, model)) if session.knows(provider) && !model.is_empty() => {
            (Some(provider.to_string()), model.to_string())
        }
        _ => (None, arg.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A unique temp directory, removed on drop — `-wal` and `-shm` go with it.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            Self(
                std::env::temp_dir().join(format!("odyn-repl-test-{}-{label}", std::process::id())),
            )
        }

        fn db(&self) -> PathBuf {
            self.0.join("odyn.db")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn turn(prompt: &str, answer: &str) -> Turn {
        (
            prompt.to_string(),
            Reply {
                text: answer.to_string(),
                usage: None,
            },
            Vec::new(),
        )
    }

    /// SQLite opens an unwritable file read-only: a briefly unavailable database.
    fn set_writable(path: &Path, writable: bool) {
        let mut permissions = std::fs::metadata(path)
            .expect("stat the database")
            .permissions();
        permissions.set_readonly(!writable);
        std::fs::set_permissions(path, permissions).expect("set the database's permissions");
    }

    /// The turn after a failed save must not become a conversation of its own.
    #[test]
    fn turns_that_could_not_be_saved_are_written_by_the_next_save() {
        let dir = TempDir::new("pending");
        let storage = Storage::open(dir.db()).expect("open the database");
        set_writable(&dir.db(), false);
        let read_only = Storage::open(dir.db()).expect("open the read-only database");
        set_writable(&dir.db(), true);

        let mut conversation = None;
        let mut pending = vec![turn("first question", "first answer")];
        record(
            &read_only,
            &mut conversation,
            "ollama",
            "llama3.2:3b",
            &mut pending,
        )
        .expect_err("a read-only database cannot be written");
        assert_eq!(conversation, None);
        assert_eq!(pending.len(), 1, "the turn must stay queued");

        pending.push(turn("second question", "second answer"));
        record(
            &storage,
            &mut conversation,
            "ollama",
            "llama3.2:3b",
            &mut pending,
        )
        .expect("the database is writable again");

        assert!(pending.is_empty());
        let conversations = storage.list_conversations().expect("list conversations");
        assert_eq!(conversations.len(), 1, "{conversations:?}");
        assert_eq!(conversations[0].title, "first question");
        let messages = storage
            .messages(conversations[0].id)
            .expect("list messages");
        let written: Vec<(Role, &str)> = messages
            .iter()
            .map(|message| (message.role, message.content.as_str()))
            .collect();
        assert_eq!(
            written,
            vec![
                (Role::User, "first question"),
                (Role::Assistant, "first answer"),
                (Role::User, "second question"),
                (Role::Assistant, "second answer"),
            ]
        );
    }
}
