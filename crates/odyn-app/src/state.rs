//! What the commands share: the config, the providers built from it, the one
//! database connection, and the replies streaming right now.

use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};

use odyn_core::config::{Config, ProviderRegistry};
use odyn_core::storage::Storage;

pub struct AppState {
    /// Behind a lock so the providers view can swap in an edited config;
    /// a command that already holds a guard finishes on the state it saw.
    ready: RwLock<Result<Ready, String>>,
}

pub struct Ready {
    pub config: Config,
    pub registry: ProviderRegistry,
    storage: Mutex<Storage>,
}

impl AppState {
    /// A broken config or an unopenable database is a state, not a crash: the
    /// window still opens and every command answers with the reason.
    pub fn load() -> Self {
        Self {
            ready: RwLock::new(Self::open()),
        }
    }

    fn open() -> Result<Ready, String> {
        let config = Config::load().map_err(|err| err.to_string())?;
        let registry = ProviderRegistry::from_config(&config).map_err(|err| err.to_string())?;
        let storage = Storage::open_default().map_err(|err| err.to_string())?;
        Ok(Ready {
            config,
            registry,
            storage: Mutex::new(storage),
        })
    }

    pub fn ready(&self) -> Result<ReadyGuard<'_>, String> {
        let guard = read_lock(&self.ready);
        match &*guard {
            Ok(_) => Ok(ReadyGuard(guard)),
            Err(err) => Err(err.clone()),
        }
    }

    /// Rereads the config and reopens the database. A failed reload keeps the
    /// failure as the new state: the file on disk is the truth, broken or not.
    pub fn reload(&self) -> Result<(), String> {
        let fresh = Self::open();
        let outcome = fresh.as_ref().map(|_| ()).map_err(Clone::clone);
        *self
            .ready
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fresh;
        outcome
    }
}

/// Proof the state was `Ok` when the lock was taken: one consistent read.
pub struct ReadyGuard<'a>(RwLockReadGuard<'a, Result<Ready, String>>);

impl Deref for ReadyGuard<'_> {
    type Target = Ready;

    fn deref(&self) -> &Ready {
        self.0.as_ref().expect("checked when the guard was made")
    }
}

fn read_lock(lock: &RwLock<Result<Ready, String>>) -> RwLockReadGuard<'_, Result<Ready, String>> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Ready {
    /// A panic while holding the lock leaves the connection usable — SQLite
    /// state lives in the file, not in the guard — so poisoning is ignored.
    pub fn storage(&self) -> MutexGuard<'_, Storage> {
        lock(&self.storage)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
