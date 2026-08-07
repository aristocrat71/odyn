//! The Odyn CLI: a thin adapter over odyn-core.

mod ask;
mod config;
mod mem;
mod repl;
mod session;

use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::session::{warn, Failure, Session};

#[derive(Parser)]
#[command(name = "odyn", version, about = "odyn — chat with open-weight models")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// provider to use; defaults to default_provider in odyn.toml
    #[arg(long, global = true, value_name = "NAME")]
    provider: Option<String>,
    /// model to use; defaults to the provider's default_model
    #[arg(long, global = true, value_name = "NAME")]
    model: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// ask one question; reads the prompt from stdin when none is given
    Ask {
        /// the question; omit it to read the prompt from stdin
        prompt: Option<String>,
        /// emit ndjson events instead of plain text
        #[arg(long)]
        json: bool,
        /// keep the exchange as a conversation
        #[arg(long)]
        save: bool,
        /// print the injected memory context before the answer
        #[arg(long)]
        show_context: bool,
        /// answer style: off, lite, full or ultra; overrides the config
        #[arg(long, value_name = "LEVEL")]
        brevity: Option<odyn_core::brevity::Brevity>,
    },
    /// open the chat repl
    Chat {
        /// print the injected memory context before each answer
        #[arg(long)]
        show_context: bool,
        /// answer style: off, lite, full or ultra; overrides the config
        #[arg(long, value_name = "LEVEL")]
        brevity: Option<odyn_core::brevity::Brevity>,
    },
    /// read or edit odyn.toml
    Config {
        #[command(subcommand)]
        action: config::Action,
    },
    /// remember, browse and prune what Odyn knows
    Mem {
        #[command(subcommand)]
        action: mem::Action,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = matches!(cli.command, Command::Ask { json: true, .. });
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            report(&failure, json);
            ExitCode::from(failure.code)
        }
    }
}

fn run(cli: Cli) -> Result<(), Failure> {
    match cli.command {
        Command::Config { action } => config::run(action),
        Command::Mem { action } => mem::run(action),
        Command::Ask {
            prompt,
            json,
            save,
            show_context,
            brevity,
        } => {
            let mut session = Session::start(cli.provider, cli.model)?;
            if let Some(brevity) = brevity {
                session.brevity = brevity;
            }
            runtime()?.block_on(ask::run(session, prompt, json, save, show_context))
        }
        Command::Chat {
            show_context,
            brevity,
        } => {
            let mut session = Session::start(cli.provider, cli.model)?;
            if let Some(brevity) = brevity {
                session.brevity = brevity;
            }
            repl::run(&runtime()?, session, show_context)
        }
    }
}

/// Current-thread: one stream at a time never needs a thread pool.
fn runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| Failure::run(format!("could not start the async runtime: {err}")))
}

/// In `--json` mode the error is the stream's last event, not stderr.
fn report(failure: &Failure, json: bool) {
    if json {
        let event = serde_json::json!({"type": "error", "message": failure.message});
        let _ = writeln!(anstream::stdout(), "{event}");
        return;
    }
    warn(&failure.message);
}
