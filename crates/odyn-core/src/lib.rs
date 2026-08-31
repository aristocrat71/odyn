//! odyn-core — all Odyn logic lives here; the CLI and app are thin adapters.

pub mod brain;
pub mod brevity;
pub mod catalog;
pub mod chat;
pub mod config;
pub mod config_edit;
pub mod embed;
pub mod graph;
pub mod notes;
pub mod providers;
pub mod reasoning;
pub mod reminder;
pub mod storage;
pub mod tools;

/// Serializes tests that read or set environment variables: the environment is
/// process-wide and the crate's tests share a process.
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
