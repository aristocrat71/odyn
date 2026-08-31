//! SQLite persistence: conversations and their messages.
//!
//! One file, opened in WAL mode so a reader never blocks the writer. The
//! schema is versioned through `PRAGMA user_version` and migrated at open.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, TransactionBehavior};

use crate::brevity::Brevity;
use crate::chat::Role;

mod memory;
mod reminder;

pub use memory::{Injection, Memory, MemorySort, MemoryStats, NotePlan};
pub use reminder::Reminder;

#[cfg(test)]
pub(crate) use memory::tests as memory_tests;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
const DB_FILE_NAME: &str = "odyn.db";
const DB_PATH_ENV: &str = "ODYN_DB";

/// Index + 1 is the `user_version` the statements bring the database to, so
/// later migrations are appended and never edited.
const MIGRATIONS: &[&str] = &[
    r"
CREATE TABLE conversations (
    id         INTEGER PRIMARY KEY,
    title      TEXT    NOT NULL,
    model      TEXT    NOT NULL,
    provider   TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role            TEXT    NOT NULL,
    content         TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    input_tokens    INTEGER,
    output_tokens   INTEGER
);
",
    r"
CREATE TABLE memories (
    id         INTEGER PRIMARY KEY,
    tier       TEXT    NOT NULL CHECK (tier IN ('core', 'episodic')),
    content    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    tokens     INTEGER NOT NULL
);
CREATE VIRTUAL TABLE memories_vec USING vec0(embedding float[384]);
CREATE TABLE injections (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id      INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    memory_id       INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    injected_at     INTEGER NOT NULL
);
",
    r"
CREATE TABLE graph_cache (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    payload     TEXT    NOT NULL,
    computed_at INTEGER NOT NULL
);
",
    // NULL means the conversation never chose: the [style] config decides.
    r"
ALTER TABLE conversations ADD COLUMN brevity TEXT;
",
    // The brain v2 wipe: memories move to a folder of markdown notes and these
    // tables become an index derived from it. Old rows are dropped, not
    // migrated (authorized: dev-stage data).
    r"
DROP TABLE injections;
DROP TABLE memories;
DROP TABLE memories_vec;
DELETE FROM graph_cache;
CREATE TABLE memories (
    id         INTEGER PRIMARY KEY,
    slug       TEXT    NOT NULL UNIQUE,
    content    TEXT    NOT NULL,
    hash       INTEGER NOT NULL,
    tokens     INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE VIRTUAL TABLE memories_vec USING vec0(embedding float[384]);
CREATE TABLE memory_links (
    from_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    to_slug TEXT    NOT NULL,
    PRIMARY KEY (from_id, to_slug)
);
CREATE TABLE injections (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id      INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    memory_id       INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    injected_at     INTEGER NOT NULL
);
",
    // Which embedding model built the index. The seed row states what migration
    // 5 created, so an existing index stays valid and nothing re-embeds.
    r"
CREATE TABLE brain_meta (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    model TEXT    NOT NULL,
    dim   INTEGER NOT NULL
);
INSERT INTO brain_meta (id, model, dim) VALUES (1, 'bge-small', 384);
",
    // Ephemeral spotlight asks record injections too: conversation_id becomes
    // nullable, and `turn` — the message id, or a negative id for asks that
    // never became one — is the recall event co-use edges join on.
    r"
CREATE TABLE injections_next (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER REFERENCES conversations(id) ON DELETE CASCADE,
    message_id      INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    turn            INTEGER NOT NULL,
    memory_id       INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    injected_at     INTEGER NOT NULL
);
INSERT INTO injections_next (id, conversation_id, message_id, turn, memory_id, injected_at)
    SELECT id, conversation_id, message_id, COALESCE(message_id, -id), memory_id, injected_at
    FROM injections;
DROP TABLE injections;
ALTER TABLE injections_next RENAME TO injections;
DELETE FROM graph_cache;
",
    // Reminders are state with a deadline rather than memories, so they live in
    // rows and not in the brain folder. The partial index is what the scheduler
    // asks for the next wake-up, which happens on every write.
    r"
CREATE TABLE reminders (
    id         INTEGER PRIMARY KEY,
    text       TEXT    NOT NULL,
    due_at     INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    fired_at   INTEGER
);
CREATE INDEX reminders_pending ON reminders(due_at) WHERE fired_at IS NULL;
",
    // Full-text search over message contents. External-content: the index
    // stores no second copy of the text, and triggers keep it in step.
    r"
CREATE VIRTUAL TABLE messages_fts USING fts5(content, content='messages', content_rowid='id');
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;
INSERT INTO messages_fts (rowid, content) SELECT id, content FROM messages;
",
    // NULL means one-shot; otherwise the `every`-phrase the clock re-arms by.
    r"
ALTER TABLE reminders ADD COLUMN repeat TEXT;
",
    // Scheduled asks: prompts the clock runs as normal conversations. The
    // provider and model are frozen at creation, like a conversation's.
    r"
CREATE TABLE schedules (
    id          INTEGER PRIMARY KEY,
    prompt      TEXT    NOT NULL,
    provider    TEXT    NOT NULL,
    model       TEXT    NOT NULL,
    repeat      TEXT    NOT NULL,
    next_at     INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    last_run_at INTEGER,
    last_error  TEXT
);
CREATE INDEX schedules_next ON schedules(next_at);
",
    // A workspace folder makes a conversation an agent conversation; NULL is a
    // normal one. `agent_allow` holds its approved bash commands, verbatim.
    r"
ALTER TABLE conversations ADD COLUMN workspace TEXT;
CREATE TABLE agent_allow (
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    command         TEXT    NOT NULL,
    PRIMARY KEY (conversation_id, command)
);
",
    // What a reply actually ran, hung off its stored message: answers "what
    // did the agent do" after the live log is gone. Id order = run order.
    r"
CREATE TABLE agent_commands (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    command    TEXT    NOT NULL
);
",
    // Agent mode is gone; migrations 12 and 13 stay as history.
    r"
DROP TABLE IF EXISTS agent_commands;
DROP TABLE IF EXISTS agent_allow;
ALTER TABLE conversations DROP COLUMN workspace;
",
    // Scheduled asks are gone; migration 11 stays as history.
    r"
DROP TABLE IF EXISTS schedules;
",
    // Chat is gone: spotlight asks are ephemeral, so nothing is transcribed.
    // The injections log survives as recall accounting, keyed on `turn` alone.
    r"
DROP TRIGGER IF EXISTS messages_fts_insert;
DROP TRIGGER IF EXISTS messages_fts_delete;
DROP TABLE IF EXISTS messages_fts;
CREATE TABLE injections_next (
    id          INTEGER PRIMARY KEY,
    turn        INTEGER NOT NULL,
    memory_id   INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    injected_at INTEGER NOT NULL
);
INSERT INTO injections_next (id, turn, memory_id, injected_at)
    SELECT id, turn, memory_id, injected_at FROM injections;
DROP TABLE injections;
ALTER TABLE injections_next RENAME TO injections;
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS conversations;
DELETE FROM graph_cache;
",
];

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create {}: {source}", path.display())]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not locate a data directory; set {DB_PATH_ENV} to a database path")]
    NoDataDir,
    #[error("vector search could not be initialized: {0}")]
    VecInit(String),
    #[error("memory {0} not found")]
    MemoryNotFound(i64),
    #[error("an embedding with {expected} dimensions was required, but {got} were given")]
    EmbeddingDimensions { expected: usize, got: usize },
    #[error("note `{0}` changed but no embedding for it was given")]
    MissingEmbedding(String),
    #[error("a reminder needs something to say")]
    EmptyReminder,
}

#[derive(Debug)]
pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        odyn_vec::register().map_err(StorageError::VecInit)?;
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::Directory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // `journal_mode` answers with a row, which `pragma_update` rejects.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Opens the database in the platform data directory, or at `ODYN_DB`.
    pub fn open_default() -> Result<Self, StorageError> {
        Self::open(default_db_path()?)
    }

    /// Opens the default database only if it exists: reading memory must not
    /// conjure a database on a machine that never saved anything.
    pub fn open_default_existing() -> Result<Option<Self>, StorageError> {
        let path = default_db_path()?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::open(path)?))
    }
}

/// The deciding version is read under the write lock: two processes opening the
/// same fresh file would otherwise both read 0 and both run the DDL. The
/// unlocked read before it keeps an up-to-date database off the write lock.
fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    if user_version(conn)? >= MIGRATIONS.len() as i64 {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let applied = user_version(&tx)?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= applied {
            continue;
        }
        tx.execute_batch(sql)?;
        // `user_version` takes no bound parameters.
        tx.pragma_update(None, "user_version", version)?;
    }
    Ok(tx.commit()?)
}

fn user_version(conn: &Connection) -> Result<i64, StorageError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn default_db_path() -> Result<PathBuf, StorageError> {
    if let Some(path) = std::env::var_os(DB_PATH_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(StorageError::NoDataDir)?;
    Ok(dirs.data_dir().join(DB_FILE_NAME))
}

pub(crate) fn now_secs() -> i64 {
    crate::reminder::now_secs()
}

/// Stored under the same lowercase names the config file uses.
impl ToSql for Brevity {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.to_string()))
    }
}

impl FromSql for Brevity {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|err: crate::brevity::BadBrevity| FromSqlError::Other(err.to_string().into()))
    }
}

/// Stored as the names `Role`'s serde derive uses, so rows and wire payloads
/// agree.
impl ToSql for Role {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }))
    }
}

impl FromSql for Role {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "system" => Ok(Role::System),
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            other => Err(FromSqlError::Other(
                format!("unknown message role {other:?}").into(),
            )),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique directory under the system temp dir, removed on drop with its
    /// `-wal` and `-shm` sidecars.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("odyn-test-{}-{label}-{unique}", std::process::id()));
            Self(dir)
        }

        pub(crate) fn db(&self) -> PathBuf {
            self.0.join(DB_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn table_names(storage: &Storage) -> Vec<String> {
        let mut stmt = storage
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare");
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        names
    }

    #[test]
    fn open_migrates_a_fresh_file_and_reopening_preserves_data() {
        let dir = TempDir::new("migrate");
        let storage = Storage::open(dir.db()).expect("open fresh");

        assert_eq!(
            user_version(&storage.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        let tables = table_names(&storage);
        assert!(tables.contains(&"memories".to_string()), "{tables:?}");
        assert!(tables.contains(&"injections".to_string()), "{tables:?}");
        assert!(!tables.contains(&"conversations".to_string()), "{tables:?}");
        assert!(!tables.contains(&"messages".to_string()), "{tables:?}");

        let created = storage.add_reminder("call mum", 900, None).expect("add");
        drop(storage);

        let reopened = Storage::open(dir.db()).expect("reopen");
        assert_eq!(
            user_version(&reopened.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        assert_eq!(reopened.pending_reminders().expect("list"), vec![created]);
    }

    #[test]
    fn upgrading_a_tiered_database_wipes_memories_and_drops_the_chat_tables() {
        odyn_vec::register().expect("register sqlite-vec");
        let dir = TempDir::new("wipe");
        std::fs::create_dir_all(&dir.0).expect("create the directory");
        {
            let conn = Connection::open(dir.db()).expect("open raw");
            for (index, sql) in MIGRATIONS.iter().take(4).enumerate() {
                conn.execute_batch(sql).expect("apply old schema");
                conn.pragma_update(None, "user_version", index as i64 + 1)
                    .expect("set version");
            }
            conn.execute(
                "INSERT INTO memories (tier, content, created_at, updated_at, tokens)
                 VALUES ('core', 'wiped', 5, 5, 2)",
                [],
            )
            .expect("insert tiered memory");
        }

        let storage = Storage::open(dir.db()).expect("open upgrades");
        assert_eq!(
            user_version(&storage.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        let tables = table_names(&storage);
        assert!(!tables.contains(&"conversations".to_string()), "{tables:?}");
        assert_eq!(
            storage.count_memories().expect("count"),
            0,
            "old tiered rows are wiped, not migrated"
        );
        let notes = vec![memory::tests::note("fresh", "works after the wipe")];
        memory::tests::sync_spread(&storage, &notes);
        assert_eq!(storage.list_memories().expect("list")[0].slug, "fresh");
    }

    #[test]
    fn opening_while_another_connection_migrates_waits_for_it() {
        // The raw holder connection runs the vec0 DDL of migration 2 itself.
        odyn_vec::register().expect("register sqlite-vec");
        let dir = TempDir::new("race");
        std::fs::create_dir_all(&dir.0).expect("create the directory");
        let holder = Connection::open(dir.db()).expect("open the holder");
        holder.busy_timeout(BUSY_TIMEOUT).expect("busy timeout");
        holder
            .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .expect("wal");

        let (locked, is_locked) = std::sync::mpsc::channel();
        let migrating = std::thread::spawn(move || {
            holder
                .execute_batch("BEGIN IMMEDIATE")
                .expect("take the write lock");
            for (index, sql) in MIGRATIONS.iter().enumerate() {
                holder.execute_batch(sql).expect("migrate");
                holder
                    .pragma_update(None, "user_version", index as i64 + 1)
                    .expect("set user_version");
            }
            locked.send(()).expect("announce the lock");
            std::thread::sleep(Duration::from_millis(200));
            holder.execute_batch("COMMIT").expect("commit");
        });

        is_locked.recv().expect("wait for the lock");
        let storage = Storage::open(dir.db()).expect("open against an in-flight migration");
        migrating.join().expect("holder thread");

        assert_eq!(
            user_version(&storage.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        assert!(table_names(&storage).contains(&"memories".to_string()));
        assert!(storage.pending_reminders().expect("list").is_empty());
    }

    #[test]
    fn open_default_honours_the_env_override() {
        let _env = crate::lock_env();
        let dir = TempDir::new("env");
        let path = dir.db();
        let previous = std::env::var_os(DB_PATH_ENV);
        std::env::set_var(DB_PATH_ENV, &path);

        let storage = Storage::open_default().expect("open default");
        let created = storage.add_reminder("call mum", 900, None).expect("add");
        drop(storage);

        match previous {
            Some(value) => std::env::set_var(DB_PATH_ENV, value),
            None => std::env::remove_var(DB_PATH_ENV),
        }

        assert!(path.exists(), "database was not created at {path:?}");
        let reopened = Storage::open(&path).expect("reopen at the override path");
        assert_eq!(reopened.pending_reminders().expect("list"), vec![created]);
    }

    #[test]
    fn roles_are_stored_under_their_serde_names() {
        for role in [Role::System, Role::User, Role::Assistant] {
            let stored = role.to_sql().expect("to_sql");
            let serde_name = serde_json::to_string(&role).expect("serialize role");
            assert_eq!(
                stored,
                ToSqlOutput::from(serde_name.trim_matches('"')),
                "{role:?}"
            );
            assert_eq!(
                Role::column_result(ValueRef::from(serde_name.trim_matches('"')))
                    .expect("from_sql"),
                role
            );
        }
    }
}
