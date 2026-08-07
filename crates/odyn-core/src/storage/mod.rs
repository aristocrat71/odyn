//! SQLite persistence: conversations and their messages.
//!
//! One file, opened in WAL mode so a reader never blocks the writer. The
//! schema is versioned through `PRAGMA user_version` and migrated at open.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, Row, TransactionBehavior};

use crate::brevity::Brevity;
use crate::chat::{Role, Usage};

mod memory;

pub use memory::{Injection, Memory, MemorySort, MemoryStats, NotePlan};

/// Note and sync helpers shared by other modules' tests.
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
    // The brain v2 wipe: memories move out of SQLite into a folder of
    // markdown notes, and these tables become an index derived from it —
    // one flat pool, no tiers, slugs for ids, wikilinks as edges. Old rows
    // are dropped, not migrated (authorized: dev-stage data).
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
    // Which embedding model built the index. The vector table's width is that
    // model's, so it can no longer be declared here — `ensure_index` owns it
    // from now on. The row states what migration 5 created, so an existing
    // index stays valid and nothing re-embeds just for this upgrade.
    r"
CREATE TABLE brain_meta (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    model TEXT    NOT NULL,
    dim   INTEGER NOT NULL
);
INSERT INTO brain_meta (id, model, dim) VALUES (1, 'bge-small', 384);
",
];

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("conversation {0} not found")]
    ConversationNotFound(i64),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: i64,
    pub title: String,
    pub model: String,
    pub provider: String,
    /// Unix epoch seconds.
    pub created_at: i64,
    pub updated_at: i64,
    /// `None` until the user explicitly picks a level for this conversation;
    /// callers fall back to the `[style]` config default.
    pub brevity: Option<Brevity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: i64,
    pub conversation_id: i64,
    pub role: Role,
    pub content: String,
    /// Unix epoch seconds.
    pub created_at: i64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
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

    /// Opens the default database only if it already exists: reading memory
    /// must not conjure a database on a machine that never saved anything.
    pub fn open_default_existing() -> Result<Option<Self>, StorageError> {
        let path = default_db_path()?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Self::open(path)?))
    }

    pub fn create_conversation(
        &self,
        title: &str,
        provider: &str,
        model: &str,
    ) -> Result<Conversation, StorageError> {
        let now = now_secs();
        self.conn.execute(
            "INSERT INTO conversations (title, model, provider, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![title, model, provider, now],
        )?;
        Ok(Conversation {
            id: self.conn.last_insert_rowid(),
            title: title.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            created_at: now,
            updated_at: now,
            brevity: None,
        })
    }

    /// Most recently active first; ties broken by id so the order is stable.
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, provider, created_at, updated_at, brevity
             FROM conversations ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], to_conversation)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The most recently active conversation — the head of `list_conversations`,
    /// read on its own so a new chat can open on the last one's target.
    pub fn latest_conversation(&self) -> Result<Option<Conversation>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, provider, created_at, updated_at, brevity
             FROM conversations ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], to_conversation)?;
        Ok(rows.next().transpose()?)
    }

    /// Writes an explicit brevity choice; the column stays NULL until one.
    pub fn set_conversation_brevity(&self, id: i64, brevity: Brevity) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            "UPDATE conversations SET brevity = ?2 WHERE id = ?1",
            params![id, brevity],
        )?;
        found(changed, id)
    }

    pub fn rename_conversation(&self, id: i64, title: &str) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            "UPDATE conversations SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        found(changed, id)
    }

    /// Messages go with it, through the foreign key's `ON DELETE CASCADE`.
    pub fn delete_conversation(&self, id: i64) -> Result<(), StorageError> {
        let changed = self
            .conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        found(changed, id)
    }

    pub fn set_conversation_model(
        &self,
        id: i64,
        provider: &str,
        model: &str,
    ) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            "UPDATE conversations SET provider = ?2, model = ?3 WHERE id = ?1",
            params![id, provider, model],
        )?;
        found(changed, id)
    }

    pub fn append_message(
        &self,
        conversation_id: i64,
        role: Role,
        content: &str,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<StoredMessage, StorageError> {
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        touch(&tx, conversation_id, now)?;
        let id = insert(
            &tx,
            conversation_id,
            role,
            content,
            now,
            input_tokens,
            output_tokens,
        )?;
        tx.commit()?;
        Ok(StoredMessage {
            id,
            conversation_id,
            role,
            content: content.to_string(),
            created_at: now,
            input_tokens,
            output_tokens,
        })
    }

    /// A question and the answer to it are one write: a failure between the two
    /// rows would leave a question on disk that nothing ever answers. The
    /// memories injected for the question commit in the same transaction, so a
    /// saved turn can never disagree with its recorded injections.
    pub fn append_turn(
        &self,
        conversation_id: i64,
        prompt: &str,
        answer: &str,
        usage: Option<Usage>,
        injected: &[i64],
    ) -> Result<(), StorageError> {
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        touch(&tx, conversation_id, now)?;
        let prompt_id = insert(&tx, conversation_id, Role::User, prompt, now, None, None)?;
        insert(
            &tx,
            conversation_id,
            Role::Assistant,
            answer,
            now,
            usage.map(|usage| usage.input_tokens),
            usage.map(|usage| usage.output_tokens),
        )?;
        for memory_id in injected {
            tx.execute(
                "INSERT INTO injections (conversation_id, message_id, memory_id, injected_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![conversation_id, prompt_id, memory_id, now],
            )?;
        }
        Ok(tx.commit()?)
    }

    pub fn messages(&self, conversation_id: i64) -> Result<Vec<StoredMessage>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, role, content, created_at, input_tokens, output_tokens
             FROM messages WHERE conversation_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![conversation_id], to_message)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// The version that decides what to run is read under the write lock: two
/// processes opening the same fresh file would otherwise both read 0 and both
/// run the DDL, and the loser would fail on a table the winner just created.
/// The unlocked read before it keeps an up-to-date database from taking the
/// write lock at all, so opening never queues behind someone else's write.
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

/// Bumping `updated_at` doubles as the existence check, with a clearer error
/// than the foreign key would give.
fn touch(conn: &Connection, conversation_id: i64, now: i64) -> Result<(), StorageError> {
    let changed = conn.execute(
        "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation_id, now],
    )?;
    found(changed, conversation_id)
}

fn insert(
    conn: &Connection,
    conversation_id: i64,
    role: Role,
    content: &str,
    now: i64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> Result<i64, StorageError> {
    conn.execute(
        "INSERT INTO messages
             (conversation_id, role, content, created_at, input_tokens, output_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            conversation_id,
            role,
            content,
            now,
            input_tokens,
            output_tokens
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn found(changed: usize, id: i64) -> Result<(), StorageError> {
    if changed == 0 {
        return Err(StorageError::ConversationNotFound(id));
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

fn to_conversation(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: row.get(0)?,
        title: row.get(1)?,
        model: row.get(2)?,
        provider: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        brevity: row.get(6)?,
    })
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

fn to_message(row: &Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        role: row.get(2)?,
        content: row.get(3)?,
        created_at: row.get(4)?,
        input_tokens: row.get(5)?,
        output_tokens: row.get(6)?,
    })
}

/// Stored as the names `Role`'s serde derive uses, so rows and wire payloads
/// agree.
impl ToSql for Role {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }))
    }
}

impl FromSql for Role {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "system" => Ok(Role::System),
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
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

    /// A unique directory under the system temp dir, removed on drop — the
    /// `-wal` and `-shm` sidecars go with it.
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
        assert!(tables.contains(&"conversations".to_string()), "{tables:?}");
        assert!(tables.contains(&"messages".to_string()), "{tables:?}");

        let created = storage
            .create_conversation("first", "ollama", "llama3.2:3b")
            .expect("create");
        drop(storage);

        let reopened = Storage::open(dir.db()).expect("reopen");
        assert_eq!(
            user_version(&reopened.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        assert_eq!(reopened.list_conversations().expect("list"), vec![created]);
    }

    /// The brain v2 migration wipes the old tiered memory rows but must keep
    /// every conversation, and the reborn index must accept notes.
    #[test]
    fn upgrading_a_tiered_database_wipes_memories_and_keeps_conversations() {
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
                "INSERT INTO conversations (title, model, provider, created_at, updated_at)
                 VALUES ('kept', 'm', 'p', 5, 5)",
                [],
            )
            .expect("insert conversation");
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
        assert_eq!(storage.list_conversations().expect("list")[0].title, "kept");
        assert_eq!(
            storage.count_memories().expect("count"),
            0,
            "old tiered rows are wiped, not migrated"
        );
        // The reborn index accepts a note under the new schema.
        let notes = vec![memory::tests::note("fresh", "works after the wipe")];
        memory::tests::sync_spread(&storage, &notes);
        assert_eq!(storage.list_memories().expect("list")[0].slug, "fresh");
    }

    /// Two processes reaching a fresh file at once: the one that loses the race
    /// must find the migration applied, not run it again.
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
        assert!(table_names(&storage).contains(&"conversations".to_string()));
        assert!(storage.list_conversations().expect("list").is_empty());
    }

    #[test]
    fn conversation_and_message_round_trip() {
        let dir = TempDir::new("crud");
        let storage = Storage::open(dir.db()).expect("open");

        let first = storage
            .create_conversation("first", "ollama", "llama3.2:3b")
            .expect("create first");
        let second = storage
            .create_conversation("second", "deepseek", "deepseek-chat")
            .expect("create second");
        touch(&storage.conn, first.id, 100).expect("backdate first");
        touch(&storage.conn, second.id, 200).expect("backdate second");

        let listed = storage.list_conversations().expect("list");
        assert_eq!(
            listed.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second.id, first.id]
        );

        storage
            .rename_conversation(first.id, "renamed")
            .expect("rename");
        storage
            .set_conversation_model(first.id, "deepseek", "deepseek-reasoner")
            .expect("set model");
        let reloaded = storage.list_conversations().expect("list after rename");
        let updated = reloaded
            .iter()
            .find(|row| row.id == first.id)
            .expect("renamed conversation");
        assert_eq!(updated.title, "renamed");
        assert_eq!(updated.provider, "deepseek");
        assert_eq!(updated.model, "deepseek-reasoner");

        let question = storage
            .append_message(first.id, Role::User, "hi", None, None)
            .expect("append user message");
        let answer = storage
            .append_message(first.id, Role::Assistant, "hello", Some(26), Some(7))
            .expect("append assistant message");
        assert_eq!(question.input_tokens, None);
        assert_eq!(answer.input_tokens, Some(26));

        let messages = storage.messages(first.id).expect("messages");
        assert_eq!(messages, vec![question, answer]);
        assert!(storage.messages(second.id).expect("messages").is_empty());

        // Appending bumped `updated_at` to now, well past the backdated value.
        let listed = storage.list_conversations().expect("list after append");
        assert_eq!(
            listed.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![first.id, second.id]
        );

        storage.delete_conversation(first.id).expect("delete");
        assert!(storage.messages(first.id).expect("messages").is_empty());
        assert!(matches!(
            storage.rename_conversation(first.id, "gone"),
            Err(StorageError::ConversationNotFound(id)) if id == first.id
        ));

        storage.delete_conversation(second.id).expect("delete");
        assert!(storage.list_conversations().expect("list").is_empty());
    }

    /// What a new chat inherits its provider and model from.
    #[test]
    fn the_latest_conversation_is_the_head_of_the_list() {
        let dir = TempDir::new("latest");
        let storage = Storage::open(dir.db()).expect("open");
        assert!(storage.latest_conversation().expect("latest").is_none());

        let older = storage
            .create_conversation("older", "ollama", "llama3.2:3b")
            .expect("create older");
        let newer = storage
            .create_conversation("newer", "deepseek", "deepseek-chat")
            .expect("create newer");
        touch(&storage.conn, older.id, 100).expect("backdate older");
        touch(&storage.conn, newer.id, 200).expect("backdate newer");

        let latest = storage.latest_conversation().expect("latest");
        assert_eq!(
            latest.map(|row| (row.provider, row.model)),
            Some(("deepseek".to_string(), "deepseek-chat".to_string()))
        );

        // Answering in the older one makes it the one a new chat follows.
        storage
            .append_message(older.id, Role::User, "hi", None, None)
            .expect("append");
        let latest = storage.latest_conversation().expect("latest after append");
        assert_eq!(latest.map(|row| row.id), Some(older.id));
    }

    #[test]
    fn a_turn_that_cannot_be_finished_stores_neither_of_its_messages() {
        let dir = TempDir::new("turn");
        let storage = Storage::open(dir.db()).expect("open");
        let conversation = storage
            .create_conversation("turn", "ollama", "llama3.2:3b")
            .expect("create");

        storage
            .append_turn(
                conversation.id,
                "hi",
                "hello",
                Some(Usage {
                    input_tokens: 26,
                    output_tokens: 7,
                }),
                &[],
            )
            .expect("append a turn");
        // A count SQLite cannot hold fails the answer, with the question of the
        // same turn already inserted.
        storage
            .append_turn(
                conversation.id,
                "and again",
                "never stored",
                Some(Usage {
                    input_tokens: u64::MAX,
                    output_tokens: 0,
                }),
                &[],
            )
            .expect_err("an out-of-range token count must fail the turn");

        let messages = storage.messages(conversation.id).expect("messages");
        let stored: Vec<(Role, &str)> = messages
            .iter()
            .map(|message| (message.role, message.content.as_str()))
            .collect();
        assert_eq!(
            stored,
            vec![(Role::User, "hi"), (Role::Assistant, "hello")],
            "the failed turn must have rolled back whole"
        );
        assert_eq!(messages[1].input_tokens, Some(26));
    }

    #[test]
    fn a_second_connection_reads_while_a_write_is_open() {
        let dir = TempDir::new("wal");
        let writer = Storage::open(dir.db()).expect("open writer");
        let committed = writer
            .create_conversation("committed", "ollama", "llama3.2:3b")
            .expect("create");

        let tx = writer.conn.unchecked_transaction().expect("begin");
        tx.execute(
            "INSERT INTO conversations (title, model, provider, created_at, updated_at)
             VALUES ('pending', 'm', 'p', 1, 1)",
            [],
        )
        .expect("insert inside transaction");

        let reader = Storage::open(dir.db()).expect("open reader");
        assert_eq!(
            reader.list_conversations().expect("read during write"),
            vec![committed.clone()]
        );

        tx.commit().expect("commit");
        let titles: Vec<String> = reader
            .list_conversations()
            .expect("read after commit")
            .into_iter()
            .map(|row| row.title)
            .collect();
        assert_eq!(titles, vec![committed.title, "pending".to_string()]);
    }

    #[test]
    fn open_default_honours_the_env_override() {
        let _env = crate::lock_env();
        let dir = TempDir::new("env");
        let path = dir.db();
        let previous = std::env::var_os(DB_PATH_ENV);
        std::env::set_var(DB_PATH_ENV, &path);

        let storage = Storage::open_default().expect("open default");
        let created = storage
            .create_conversation("env", "ollama", "llama3.2:3b")
            .expect("create");
        drop(storage);

        match previous {
            Some(value) => std::env::set_var(DB_PATH_ENV, value),
            None => std::env::remove_var(DB_PATH_ENV),
        }

        assert!(path.exists(), "database was not created at {path:?}");
        let reopened = Storage::open(&path).expect("reopen at the override path");
        assert_eq!(reopened.list_conversations().expect("list"), vec![created]);
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
    #[test]
    fn brevity_is_null_until_chosen_and_then_persists() {
        let dir = TempDir::new("brevity");
        let storage = Storage::open(dir.db()).expect("open");
        let created = storage
            .create_conversation("terse", "ollama", "llama3.2:3b")
            .expect("create");
        assert_eq!(created.brevity, None);

        // The fallback chain: column first, then the config default.
        let config = crate::config::StyleConfig::default();
        assert_eq!(
            created.brevity.unwrap_or(config.brevity),
            crate::brevity::Brevity::Off
        );

        storage
            .set_conversation_brevity(created.id, crate::brevity::Brevity::Ultra)
            .expect("set");
        let reloaded = storage.list_conversations().expect("list");
        assert_eq!(reloaded[0].brevity, Some(crate::brevity::Brevity::Ultra));
        assert!(matches!(
            storage.set_conversation_brevity(9999, crate::brevity::Brevity::Lite),
            Err(StorageError::ConversationNotFound(9999))
        ));
    }
}
