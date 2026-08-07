//! odyn-core — all Odyn logic lives here; the CLI and app are thin adapters.

pub mod brain;
pub mod brevity;
pub mod catalog;
pub mod chat;
pub mod config;
pub mod config_edit;
pub mod embed;
pub mod graph;
pub mod providers;
pub mod storage;

/// Tests that read or set environment variables run one at a time: the
/// environment is process-wide, and the crate's tests share a process with each
/// other and with the developer's own shell.
#[cfg(test)]
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
