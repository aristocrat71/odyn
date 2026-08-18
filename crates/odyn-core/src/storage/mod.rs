//! SQLite persistence: conversations and their messages.
//!
//! One file, opened in WAL mode so a reader never blocks the writer. The
//! schema is versioned through `PRAGMA user_version` and migrated at open.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, Row, TransactionBehavior};

use crate::brevity::Brevity;
use crate::chat::{Role, Usage};

mod memory;
mod reminder;
mod schedule;

pub use memory::{Injection, Memory, MemorySort, MemoryStats, NotePlan};
pub use reminder::Reminder;
pub use schedule::Schedule;

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
];

/// Marks a matched term in a search snippet; its closer is `SNIPPET_END`.
pub const SNIPPET_START: char = '\u{1}';
pub const SNIPPET_END: char = '\u{2}';

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
    #[error("a reminder needs something to say")]
    EmptyReminder,
    #[error("a scheduled ask needs a prompt")]
    EmptySchedule,
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
    /// The agent workspace folder; `None` is a normal conversation.
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub conversation_id: i64,
    pub title: String,
    pub message_id: i64,
    pub role: Role,
    pub snippet: String,
    /// Unix epoch seconds.
    pub created_at: i64,
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

    /// Opens the default database only if it exists: reading memory must not
    /// conjure a database on a machine that never saved anything.
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
            workspace: None,
        })
    }

    /// Most recently active first; ties broken by id so the order is stable.
    pub fn list_conversations(&self) -> Result<Vec<Conversation>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, provider, created_at, updated_at, brevity, workspace
             FROM conversations ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], to_conversation)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The head of `list_conversations`, read on its own so a new chat can open
    /// on the last one's provider and model.
    pub fn latest_conversation(&self) -> Result<Option<Conversation>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, provider, created_at, updated_at, brevity, workspace
             FROM conversations ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], to_conversation)?;
        Ok(rows.next().transpose()?)
    }

    /// `None` turns the conversation back into a normal one; its allowlist
    /// stays, since the approvals were the user's own.
    pub fn set_conversation_workspace(
        &self,
        id: i64,
        workspace: Option<&str>,
    ) -> Result<(), StorageError> {
        let changed = self.conn.execute(
            "UPDATE conversations SET workspace = ?2 WHERE id = ?1",
            params![id, workspace],
        )?;
        found(changed, id)
    }

    /// Idempotent: approving the same command twice is one row. The foreign
    /// key refuses a conversation that does not exist.
    pub fn allow_command(&self, conversation_id: i64, command: &str) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO agent_allow (conversation_id, command) VALUES (?1, ?2)",
            params![conversation_id, command],
        )?;
        Ok(())
    }

    pub fn allowed_commands(&self, conversation_id: i64) -> Result<Vec<String>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT command FROM agent_allow WHERE conversation_id = ?1 ORDER BY command",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The tool actions a reply ran, in run order, against its message row.
    /// Each write also sweeps expired rows, so the log cannot outgrow its
    /// retention while it is the thing growing.
    pub fn record_commands(
        &self,
        message_id: i64,
        commands: &[String],
    ) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        prune_commands(&tx, now_secs())?;
        for command in commands {
            tx.execute(
                "INSERT INTO agent_commands (message_id, command) VALUES (?1, ?2)",
                params![message_id, command],
            )?;
        }
        Ok(tx.commit()?)
    }

    /// Every recorded action in the conversation, as `(message_id, command)`.
    /// The retention window filters here too, so an expired row is invisible
    /// even before a write has swept it — opening must never take the write
    /// lock, a reader never blocks the writer.
    pub fn agent_commands(&self, conversation_id: i64) -> Result<Vec<(i64, String)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT c.message_id, c.command FROM agent_commands c
             JOIN messages m ON m.id = c.message_id
             WHERE m.conversation_id = ?1 AND m.created_at >= ?2 ORDER BY c.id",
        )?;
        let rows = stmt.query_map(
            params![conversation_id, now_secs() - COMMAND_RETENTION_SECS],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

    /// Messages are deleted explicitly rather than by cascade, so the search
    /// index's delete trigger always sees them go.
    pub fn delete_conversation(&self, id: i64) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            params![id],
        )?;
        let changed = tx.execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        found(changed, id)?;
        Ok(tx.commit()?)
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

    /// A question, its answer and the memories injected for it are one write, so
    /// a saved turn can never disagree with its recorded injections.
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
                "INSERT INTO injections (conversation_id, message_id, turn, memory_id, injected_at)
                 VALUES (?1, ?2, ?2, ?3, ?4)",
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

    /// Full-text search over every message, best match first. Matched terms in
    /// the snippet sit between `SNIPPET_START` and `SNIPPET_END`.
    pub fn search_messages(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StorageError> {
        let terms = fts_query(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.conversation_id, c.title, m.id, m.role,
                    snippet(messages_fts, 0, char(1), char(2), ' … ', 12), m.created_at
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN conversations c ON c.id = m.conversation_id
             WHERE messages_fts MATCH ?1
             ORDER BY bm25(messages_fts), m.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![terms, limit as i64], |row| {
            Ok(SearchHit {
                conversation_id: row.get(0)?,
                title: row.get(1)?,
                message_id: row.get(2)?,
                role: row.get(3)?,
                snippet: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// Every whitespace-separated term, quoted so FTS operators and stray quotes
/// read as text, joined as an AND; the last term matches as a prefix so the
/// search works while a word is still being typed.
fn fts_query(text: &str) -> String {
    let mut terms: Vec<String> = text
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    if let Some(last) = terms.last_mut() {
        last.push('*');
    }
    terms.join(" ")
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

/// The command log is a recent-history convenience, not an archive: rows
/// older than five days are hidden from reads and swept on every new write.
const COMMAND_RETENTION_SECS: i64 = 5 * 24 * 60 * 60;

fn prune_commands(conn: &Connection, now: i64) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM agent_commands WHERE message_id IN
             (SELECT id FROM messages WHERE created_at < ?1)",
        params![now - COMMAND_RETENTION_SECS],
    )?;
    Ok(())
}

pub(crate) fn now_secs() -> i64 {
    crate::reminder::now_secs()
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
        workspace: row.get(7)?,
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
        // A count SQLite cannot hold fails the answer, after its question row.
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
    fn search_reads_message_contents_and_marks_the_match() {
        let dir = TempDir::new("search");
        let storage = Storage::open(dir.db()).expect("open");
        let coffee = storage
            .create_conversation("coffee talk", "ollama", "llama3.2:3b")
            .expect("create");
        let other = storage
            .create_conversation("other", "ollama", "llama3.2:3b")
            .expect("create");
        storage
            .append_message(coffee.id, Role::User, "how do I pull espresso?", None, None)
            .expect("append");
        storage
            .append_message(other.id, Role::Assistant, "tokio spawns tasks", None, None)
            .expect("append");

        let hits = storage.search_messages("espresso", 40).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, coffee.id);
        assert_eq!(hits[0].title, "coffee talk");
        assert_eq!(hits[0].role, Role::User);
        assert!(hits[0]
            .snippet
            .contains(&format!("{SNIPPET_START}espresso{SNIPPET_END}")));

        // As-you-type: the last term matches as a prefix.
        assert_eq!(
            storage.search_messages("espre", 40).expect("prefix").len(),
            1
        );
        // Terms AND together across the message.
        assert!(
            storage
                .search_messages("pull espresso", 40)
                .expect("and")
                .len()
                == 1
        );
        assert!(storage
            .search_messages("pull tokio", 40)
            .expect("miss")
            .is_empty());
        // Operators and quotes are text, never syntax; blank finds nothing.
        assert!(storage
            .search_messages("\"espr AND (", 40)
            .expect("quoted")
            .is_empty());
        assert!(storage
            .search_messages("   ", 40)
            .expect("blank")
            .is_empty());

        storage.delete_conversation(coffee.id).expect("delete");
        assert!(storage
            .search_messages("espresso", 40)
            .expect("pruned")
            .is_empty());
    }

    /// Messages stored before the search index existed are backfilled by the
    /// migration that creates it.
    #[test]
    fn upgrading_backfills_the_search_index() {
        odyn_vec::register().expect("register sqlite-vec");
        let dir = TempDir::new("fts-upgrade");
        std::fs::create_dir_all(&dir.0).expect("create the directory");
        {
            let conn = Connection::open(dir.db()).expect("open raw");
            for (index, sql) in MIGRATIONS.iter().take(8).enumerate() {
                conn.execute_batch(sql).expect("apply old schema");
                conn.pragma_update(None, "user_version", index as i64 + 1)
                    .expect("set version");
            }
            conn.execute(
                "INSERT INTO conversations (title, model, provider, created_at, updated_at)
                 VALUES ('old', 'm', 'p', 5, 5)",
                [],
            )
            .expect("insert conversation");
            conn.execute(
                "INSERT INTO messages (conversation_id, role, content, created_at)
                 VALUES (1, 'assistant', 'rustls everywhere', 5)",
                [],
            )
            .expect("insert message");
        }

        let storage = Storage::open(dir.db()).expect("open upgrades");
        let hits = storage.search_messages("rustls", 40).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "old");
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
    fn a_workspace_round_trips_and_clears() {
        let dir = TempDir::new("workspace");
        let storage = Storage::open(dir.db()).expect("open");
        let created = storage
            .create_conversation("agent", "ollama", "qwen3:8b")
            .expect("create");
        assert_eq!(created.workspace, None);

        storage
            .set_conversation_workspace(created.id, Some("/tmp/notes"))
            .expect("set");
        assert_eq!(
            storage.list_conversations().expect("list")[0].workspace,
            Some("/tmp/notes".to_string())
        );
        assert_eq!(
            storage
                .latest_conversation()
                .expect("latest")
                .and_then(|row| row.workspace),
            Some("/tmp/notes".to_string())
        );

        storage
            .set_conversation_workspace(created.id, None)
            .expect("clear");
        assert_eq!(
            storage.list_conversations().expect("list")[0].workspace,
            None
        );
        assert!(matches!(
            storage.set_conversation_workspace(9999, Some("/tmp")),
            Err(StorageError::ConversationNotFound(9999))
        ));
    }

    #[test]
    fn the_allowlist_round_trips_idempotently_and_dies_with_its_conversation() {
        let dir = TempDir::new("allow");
        let storage = Storage::open(dir.db()).expect("open");
        let kept = storage
            .create_conversation("kept", "ollama", "qwen3:8b")
            .expect("create");
        let doomed = storage
            .create_conversation("doomed", "ollama", "qwen3:8b")
            .expect("create");

        storage.allow_command(kept.id, "cargo test").expect("allow");
        storage.allow_command(kept.id, "ls -la").expect("allow");
        storage
            .allow_command(kept.id, "cargo test")
            .expect("allow again");
        storage.allow_command(doomed.id, "make").expect("allow");
        assert_eq!(
            storage.allowed_commands(kept.id).expect("list"),
            vec!["cargo test".to_string(), "ls -la".to_string()]
        );

        storage.delete_conversation(doomed.id).expect("delete");
        assert!(storage
            .allowed_commands(doomed.id)
            .expect("list")
            .is_empty());
        assert_eq!(storage.allowed_commands(kept.id).expect("list").len(), 2);
        assert!(
            storage.allow_command(9999, "anything").is_err(),
            "the foreign key must refuse an unknown conversation"
        );
    }

    #[test]
    fn ran_commands_persist_with_their_reply_and_die_with_the_conversation() {
        let dir = TempDir::new("ran");
        let storage = Storage::open(dir.db()).expect("open");
        let chat = storage
            .create_conversation("agent", "ollama", "qwen3:8b")
            .expect("create");
        let reply = storage
            .append_message(chat.id, Role::Assistant, "built it", None, None)
            .expect("append");

        storage
            .record_commands(
                reply.id,
                &["cargo build".to_string(), "cargo test".to_string()],
            )
            .expect("record");
        storage
            .record_commands(reply.id, &[])
            .expect("empty is fine");
        assert_eq!(
            storage.agent_commands(chat.id).expect("list"),
            vec![
                (reply.id, "cargo build".to_string()),
                (reply.id, "cargo test".to_string()),
            ],
            "run order survives"
        );

        storage.delete_conversation(chat.id).expect("delete");
        assert!(storage.agent_commands(chat.id).expect("list").is_empty());
        assert!(
            storage.record_commands(9999, &["x".to_string()]).is_err(),
            "the foreign key must refuse an unknown message"
        );
    }

    #[test]
    fn commands_past_the_retention_window_are_hidden_and_swept() {
        let dir = TempDir::new("sweep");
        let storage = Storage::open(dir.db()).expect("open");
        let chat = storage
            .create_conversation("agent", "ollama", "qwen3:8b")
            .expect("create");
        let old = storage
            .append_message(chat.id, Role::Assistant, "long ago", None, None)
            .expect("append");
        storage
            .record_commands(old.id, &["make old".to_string()])
            .expect("record");
        // Backdate the message past the five-day window: the read filter
        // hides its commands immediately, before any sweep.
        storage
            .conn
            .execute(
                "UPDATE messages SET created_at = ?2 WHERE id = ?1",
                params![old.id, now_secs() - COMMAND_RETENTION_SECS - 60],
            )
            .expect("backdate");
        assert!(storage.agent_commands(chat.id).expect("list").is_empty());

        // The next write physically sweeps the expired rows.
        let fresh = storage
            .append_message(chat.id, Role::Assistant, "just now", None, None)
            .expect("append");
        storage
            .record_commands(fresh.id, &["make fresh".to_string()])
            .expect("record");
        let rows: i64 = storage
            .conn
            .query_row("SELECT COUNT(*) FROM agent_commands", [], |row| row.get(0))
            .expect("count");
        assert_eq!(rows, 1, "the expired row is gone from disk");
        assert_eq!(
            storage.agent_commands(chat.id).expect("list"),
            vec![(fresh.id, "make fresh".to_string())]
        );
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
