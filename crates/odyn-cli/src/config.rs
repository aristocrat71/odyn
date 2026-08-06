//! `odyn config`: where the file is, and what is in it.

use std::io::Write;

use clap::Subcommand;
use odyn_core::config::config_path;
use odyn_core::config_edit;

use crate::session::{config_failure, write_failure, Failure};

#[derive(Subcommand)]
pub enum Action {
    /// print the path of odyn.toml
    Path,
    /// print the value at a dotted key
    Get {
        /// dotted key, e.g. providers.ollama.base_url
        key: String,
    },
    /// set a dotted key, keeping the file's comments and formatting
    Set {
        /// dotted key, e.g. providers.ollama.base_url
        key: String,
        /// numbers and booleans are stored as such, anything else as a string
        value: String,
    },
}

pub fn run(action: Action) -> Result<(), Failure> {
    let path = config_path().map_err(config_failure)?;
    let line = match action {
        Action::Path => path.display().to_string(),
        Action::Get { key } => config_edit::get(&path, &key).map_err(config_failure)?,
        Action::Set { key, value } => {
            return config_edit::set(&path, &key, &value).map_err(config_failure)
        }
    };
    writeln!(anstream::stdout(), "{line}").map_err(write_failure)
}
