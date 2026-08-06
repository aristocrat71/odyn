//! What the commands share: the config, the providers built from it, the one
//! database connection, and the replies streaming right now.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use odyn_core::config::{Config, ProviderRegistry};
use odyn_core::storage::Storage;
use tauri::async_runtime::JoinHandle;

pub struct AppState(Result<Ready, String>);

pub struct Ready {
    pub config: Config,
    pub registry: ProviderRegistry,
    pub streams: Streams,
    storage: Mutex<Storage>,
}

impl AppState {
    /// A broken config or an unopenable database is a state, not a crash: the
    /// window still opens, and every command answers with the reason so the
    /// frontend can print it where the data would have been.
    pub fn load() -> Self {
        Self(Self::open())
    }

    fn open() -> Result<Ready, String> {
        let config = Config::load().map_err(|err| err.to_string())?;
        let registry = ProviderRegistry::from_config(&config).map_err(|err| err.to_string())?;
        let storage = Storage::open_default().map_err(|err| err.to_string())?;
        Ok(Ready {
            config,
            registry,
            streams: Streams::default(),
            storage: Mutex::new(storage),
        })
    }

    pub fn ready(&self) -> Result<&Ready, String> {
        self.0.as_ref().map_err(Clone::clone)
    }
}

impl Ready {
    /// A panic while holding the lock leaves the connection usable — SQLite
    /// state lives in the file, not in the guard — so poisoning is ignored.
    pub fn storage(&self) -> MutexGuard<'_, Storage> {
        lock(&self.storage)
    }
}

/// The replies in flight, so a cancel can reach the task that produces one and
/// the text it had already produced.
#[derive(Default)]
pub struct Streams {
    next: AtomicU64,
    live: Mutex<HashMap<u64, Arc<Stream>>>,
}

pub struct Stream {
    pub conversation_id: i64,
    partial: Mutex<String>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Streams {
    /// The entry exists before the task does, so closing it is what decides who
    /// finishes a reply — the stream itself or the cancel that beat it.
    pub fn open(&self, conversation_id: i64) -> (u64, Arc<Stream>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let stream = Arc::new(Stream {
            conversation_id,
            partial: Mutex::new(String::new()),
            task: Mutex::new(None),
        });
        lock(&self.live).insert(id, Arc::clone(&stream));
        (id, stream)
    }

    pub fn close(&self, id: u64) -> Option<Arc<Stream>> {
        lock(&self.live).remove(&id)
    }
}

impl Stream {
    pub fn attach(&self, task: JoinHandle<()>) {
        *lock(&self.task) = Some(task);
    }

    pub fn push(&self, delta: &str) {
        lock(&self.partial).push_str(delta);
    }

    pub fn text(&self) -> String {
        lock(&self.partial).clone()
    }

    /// Abrupt on purpose: a stream waiting on a provider that stopped answering
    /// has no await point left at which to notice a flag.
    pub fn abort(&self) {
        if let Some(task) = lock(&self.task).as_ref() {
            task.abort();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
