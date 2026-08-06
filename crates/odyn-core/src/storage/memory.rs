//! The brain's rows: two memory tiers and one vector index.
//!
//! Core memories are always injected, so they are never embedded and never
//! touch `memories_vec`. Episodic memories are retrieved by similarity, so
//! every episodic row has a `memories_vec` row under the same rowid — written
//! in the same transaction, because a memory the index cannot see (or an index
//! entry pointing at nothing) silently corrupts retrieval.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, Row};

use super::{now_secs, Storage, StorageError};
use crate::embed::EMBEDDING_DIM;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    Core,
    Episodic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    pub tier: MemoryTier,
    pub content: String,
    /// Unix epoch seconds.
    pub created_at: i64,
    pub updated_at: i64,
    /// chars/4 approximation, computed at write time.
    pub tokens: i64,
}

impl Memory {
    /// `c-01` / `e-0142` — the id every surface (template, ledger, CLI) shows.
    pub fn display_id(&self) -> String {
        match self.tier {
            MemoryTier::Core => format!("c-{:02}", self.id),
            MemoryTier::Episodic => format!("e-{:04}", self.id),
        }
    }
}

/// One recorded use of a memory: which conversation and message it was
/// injected for. `message_id` outlives its message as `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub id: i64,
    pub conversation_id: i64,
    pub message_id: Option<i64>,
    pub memory_id: i64,
    /// Unix epoch seconds.
    pub injected_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodicSort {
    /// Most recently touched first.
    Recent,
    /// Most often injected first.
    Hits,
    /// Newest first.
    Created,
}

/// A memory with what the injections log says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStats {
    pub memory: Memory,
    pub hits: i64,
    pub last_injected_at: Option<i64>,
}

impl Storage {
    /// Content is stored single-line: whitespace runs containing a newline
    /// collapse to one space, and the edges are trimmed. Anything else would
    /// break out of its `- [id] content` line in the injected context.
    pub fn add_memory(
        &self,
        tier: MemoryTier,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<Memory, StorageError> {
        let content = single_line(content);
        if content.is_empty() {
            return Err(StorageError::EmptyMemory);
        }
        check_tier(tier, embedding)?;
        let tokens = approx_tokens(&content);
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO memories (tier, content, created_at, updated_at, tokens)
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![tier, content, now, tokens],
        )?;
        let id = tx.last_insert_rowid();
        if let Some(embedding) = embedding {
            tx.execute(
                "INSERT INTO memories_vec (rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_blob(embedding)],
            )?;
        }
        invalidate_graph(&tx)?;
        tx.commit()?;
        Ok(Memory {
            id,
            tier,
            content,
            created_at: now,
            updated_at: now,
            tokens,
        })
    }

    pub fn list_memories(&self, tier: Option<MemoryTier>) -> Result<Vec<Memory>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, tier, content, created_at, updated_at, tokens
             FROM memories WHERE ?1 IS NULL OR tier = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![tier], to_memory)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Records what was injected for a message, replacing any earlier record
    /// for it: a retried turn's context is the one the model last saw, and
    /// counting it twice would inflate every hit statistic built on this table.
    pub fn record_injections(
        &self,
        conversation_id: i64,
        message_id: Option<i64>,
        memory_ids: &[i64],
    ) -> Result<(), StorageError> {
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        if let Some(message_id) = message_id {
            tx.execute(
                "DELETE FROM injections WHERE message_id = ?1",
                params![message_id],
            )?;
        }
        for memory_id in memory_ids {
            tx.execute(
                "INSERT INTO injections (conversation_id, message_id, memory_id, injected_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![conversation_id, message_id, memory_id, now],
            )?;
        }
        // Injections move co-injection edges and hit counts, so the graph too.
        invalidate_graph(&tx)?;
        Ok(tx.commit()?)
    }

    pub fn injections(&self, conversation_id: i64) -> Result<Vec<Injection>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, message_id, memory_id, injected_at
             FROM injections WHERE conversation_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![conversation_id], |row| {
            Ok(Injection {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                message_id: row.get(2)?,
                memory_id: row.get(3)?,
                injected_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One page of the episodic column, with hit counts from the injections
    /// log. Pages by offset: the list is append-mostly and the UI tolerates a
    /// row sliding between pages.
    pub fn episodic_overview(
        &self,
        sort: EpisodicSort,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryStats>, StorageError> {
        let order = match sort {
            EpisodicSort::Recent => "m.updated_at DESC, m.id DESC",
            EpisodicSort::Hits => "hits DESC, m.id DESC",
            EpisodicSort::Created => "m.created_at DESC, m.id DESC",
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.tier, m.content, m.created_at, m.updated_at, m.tokens,
                    count(i.id) AS hits, max(i.injected_at)
             FROM memories AS m LEFT JOIN injections AS i ON i.memory_id = m.id
             WHERE m.tier = 'episodic'
             GROUP BY m.id ORDER BY {order} LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
            Ok(MemoryStats {
                memory: to_memory(row)?,
                hits: row.get(6)?,
                last_injected_at: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The same stats for an arbitrary set — search results keep their order.
    pub fn stats_for(&self, memories: Vec<Memory>) -> Result<Vec<MemoryStats>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT count(*), max(injected_at) FROM injections WHERE memory_id = ?1")?;
        memories
            .into_iter()
            .map(|memory| {
                let (hits, last_injected_at) =
                    stmt.query_row(params![memory.id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                Ok(MemoryStats {
                    memory,
                    hits,
                    last_injected_at,
                })
            })
            .collect()
    }

    pub fn count_memories(&self, tier: Option<MemoryTier>) -> Result<i64, StorageError> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM memories WHERE ?1 IS NULL OR tier = ?1",
            params![tier],
            |row| row.get(0),
        )?)
    }

    pub fn memory(&self, id: i64) -> Result<Memory, StorageError> {
        self.conn
            .query_row(
                "SELECT id, tier, content, created_at, updated_at, tokens
                 FROM memories WHERE id = ?1",
                params![id],
                to_memory,
            )
            .map_err(|error| not_found(error, id))
    }

    /// The tier is fixed at creation; the embedding argument must match it,
    /// so an episodic edit always arrives with its re-embedded content.
    pub fn update_memory(
        &self,
        id: i64,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<Memory, StorageError> {
        let content = single_line(content);
        if content.is_empty() {
            return Err(StorageError::EmptyMemory);
        }
        let tokens = approx_tokens(&content);
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        let (tier, created_at) = tx
            .query_row(
                "SELECT tier, created_at FROM memories WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, MemoryTier>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(|error| not_found(error, id))?;
        check_tier(tier, embedding)?;
        tx.execute(
            "UPDATE memories SET content = ?2, tokens = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, content, tokens, now],
        )?;
        if let Some(embedding) = embedding {
            tx.execute("DELETE FROM memories_vec WHERE rowid = ?1", params![id])?;
            tx.execute(
                "INSERT INTO memories_vec (rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_blob(embedding)],
            )?;
        }
        invalidate_graph(&tx)?;
        tx.commit()?;
        Ok(Memory {
            id,
            tier,
            content,
            created_at,
            updated_at: now,
            tokens,
        })
    }

    pub fn delete_memory(&self, id: i64) -> Result<(), StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(StorageError::MemoryNotFound(id));
        }
        // A no-op for core memories, which have no index row.
        tx.execute("DELETE FROM memories_vec WHERE rowid = ?1", params![id])?;
        invalidate_graph(&tx)?;
        Ok(tx.commit()?)
    }

    /// Nearest episodic memories, closest first, with their L2 distances.
    pub fn knn_episodic(
        &self,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(Memory, f64)>, StorageError> {
        check_dimensions(embedding)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.tier, m.content, m.created_at, m.updated_at, m.tokens, k.distance
             FROM (SELECT rowid, distance FROM memories_vec
                   WHERE embedding MATCH ?1 AND k = ?2) AS k
             JOIN memories AS m ON m.id = k.rowid
             ORDER BY k.distance",
        )?;
        let rows = stmt.query_map(params![vec_blob(embedding), k as i64], |row| {
            Ok((to_memory(row)?, row.get::<_, f64>(6)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The k nearest episodic neighbors of a stored memory, excluding itself.
    /// (vec0 only understands MATCH and k, so the self-row is dropped here.)
    pub fn episodic_neighbors(&self, id: i64, k: usize) -> Result<Vec<(i64, f64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid, distance FROM memories_vec
             WHERE embedding MATCH (SELECT embedding FROM memories_vec WHERE rowid = ?1)
               AND k = ?2
             ORDER BY distance",
        )?;
        let rows = stmt.query_map(params![id, k as i64 + 1], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut neighbors = rows.collect::<Result<Vec<_>, _>>()?;
        neighbors.retain(|(other, _)| *other != id);
        neighbors.truncate(k);
        Ok(neighbors)
    }

    /// Episodic pairs injected for the same message at least `min` times,
    /// smaller id first.
    pub fn co_injections(&self, min: i64) -> Result<Vec<(i64, i64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT a.memory_id, b.memory_id
             FROM injections AS a
             JOIN injections AS b
               ON a.message_id = b.message_id AND a.memory_id < b.memory_id
             JOIN memories AS ma ON ma.id = a.memory_id AND ma.tier = 'episodic'
             JOIN memories AS mb ON mb.id = b.memory_id AND mb.tier = 'episodic'
             WHERE a.message_id IS NOT NULL
             GROUP BY a.memory_id, b.memory_id
             HAVING count(*) >= ?1",
        )?;
        let rows = stmt.query_map(params![min], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn cached_graph(&self) -> Result<Option<String>, StorageError> {
        let payload =
            self.conn
                .query_row("SELECT payload FROM graph_cache WHERE id = 1", [], |row| {
                    row.get(0)
                });
        match payload {
            Ok(payload) => Ok(Some(payload)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(other) => Err(StorageError::Sqlite(other)),
        }
    }

    pub fn store_graph(&self, payload: &str) -> Result<(), StorageError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO graph_cache (id, payload, computed_at) VALUES (1, ?1, ?2)",
            params![payload, now_secs()],
        )?;
        Ok(())
    }
}

fn invalidate_graph(conn: &rusqlite::Connection) -> Result<(), StorageError> {
    conn.execute("DELETE FROM graph_cache", [])?;
    Ok(())
}

fn check_tier(tier: MemoryTier, embedding: Option<&[f32]>) -> Result<(), StorageError> {
    match (tier, embedding) {
        (MemoryTier::Core, None) => Ok(()),
        (MemoryTier::Core, Some(_)) => Err(StorageError::EmbeddingForbidden),
        (MemoryTier::Episodic, None) => Err(StorageError::EmbeddingRequired),
        (MemoryTier::Episodic, Some(embedding)) => check_dimensions(embedding),
    }
}

fn check_dimensions(embedding: &[f32]) -> Result<(), StorageError> {
    if embedding.len() != EMBEDDING_DIM {
        return Err(StorageError::EmbeddingDimensions {
            expected: EMBEDDING_DIM,
            got: embedding.len(),
        });
    }
    Ok(())
}

/// sqlite-vec's expected encoding: the raw f32 values, little-endian.
fn vec_blob(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// What any content becomes before it is stored: whitespace runs containing a
/// newline collapse to one space, edges trimmed. Public so callers embedding
/// content embed exactly the text that will be stored (it is idempotent).
pub fn normalize_content(content: &str) -> String {
    single_line(content)
}

fn single_line(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut run = String::new();
    let mut run_has_newline = false;
    for ch in content.chars() {
        if ch.is_whitespace() {
            run.push(ch);
            run_has_newline |= ch == '\n' || ch == '\r';
        } else {
            if !run.is_empty() {
                // A run before the first word is leading whitespace: dropped.
                if !out.is_empty() {
                    if run_has_newline {
                        out.push(' ');
                    } else {
                        out.push_str(&run);
                    }
                }
                run.clear();
                run_has_newline = false;
            }
            out.push(ch);
        }
    }
    out
}

fn approx_tokens(content: &str) -> i64 {
    content.chars().count().div_ceil(4) as i64
}

fn not_found(error: rusqlite::Error, id: i64) -> StorageError {
    match error {
        rusqlite::Error::QueryReturnedNoRows => StorageError::MemoryNotFound(id),
        other => StorageError::Sqlite(other),
    }
}

fn to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        tier: row.get(1)?,
        content: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        tokens: row.get(5)?,
    })
}

/// Stored as the strings the schema's CHECK constraint names.
impl ToSql for MemoryTier {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            MemoryTier::Core => "core",
            MemoryTier::Episodic => "episodic",
        }))
    }
}

impl FromSql for MemoryTier {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "core" => Ok(MemoryTier::Core),
            "episodic" => Ok(MemoryTier::Episodic),
            other => Err(FromSqlError::Other(
                format!("unknown memory tier {other:?}").into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::TempDir;
    use super::super::{user_version, MIGRATIONS};
    use super::*;
    use rusqlite::Connection;

    fn open(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        (dir, storage)
    }

    /// A 384-dim unit vector along `axis`, optionally leaning towards the next
    /// axis — leaning further means farther from the pure axis vector.
    fn vector(axis: usize, lean: f32) -> Vec<f32> {
        let mut values = vec![0.0f32; EMBEDDING_DIM];
        values[axis] = 1.0 - lean;
        values[axis + 1] = lean;
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut values {
            *value /= norm;
        }
        values
    }

    fn vec_rows(storage: &Storage) -> i64 {
        storage
            .conn
            .query_row("SELECT count(*) FROM memories_vec", [], |row| row.get(0))
            .expect("count vec rows")
    }

    #[test]
    fn a_version_1_database_upgrades_in_place_and_keeps_its_rows() {
        let dir = TempDir::new("upgrade");
        std::fs::create_dir_all(&dir.0).expect("create the directory");
        {
            let conn = Connection::open(dir.db()).expect("open raw");
            conn.execute_batch(MIGRATIONS[0]).expect("apply v1");
            conn.pragma_update(None, "user_version", 1).expect("set v1");
            conn.execute(
                "INSERT INTO conversations (title, model, provider, created_at, updated_at)
                 VALUES ('kept', 'm', 'p', 5, 5)",
                [],
            )
            .expect("insert conversation");
            conn.execute(
                "INSERT INTO messages (conversation_id, role, content, created_at)
                 VALUES (1, 'user', 'kept too', 5)",
                [],
            )
            .expect("insert message");
        }

        let storage = Storage::open(dir.db()).expect("open upgrades");
        assert_eq!(
            user_version(&storage.conn).expect("user_version"),
            MIGRATIONS.len() as i64
        );
        let conversations = storage.list_conversations().expect("list");
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].title, "kept");
        assert_eq!(
            storage.messages(conversations[0].id).expect("messages")[0].content,
            "kept too"
        );

        let added = storage
            .add_memory(
                MemoryTier::Episodic,
                "works after upgrade",
                Some(&vector(0, 0.0)),
            )
            .expect("add after upgrade");
        assert_eq!(vec_rows(&storage), 1);
        assert_eq!(
            storage.memory(added.id).expect("get").content,
            "works after upgrade"
        );
    }

    #[test]
    fn an_episodic_memory_stores_its_embedding_under_the_same_rowid() {
        let (_dir, storage) = open("episodic");
        let memory = storage
            .add_memory(
                MemoryTier::Episodic,
                "mitul prefers rustls",
                Some(&vector(0, 0.0)),
            )
            .expect("add");

        assert_eq!(memory.tier, MemoryTier::Episodic);
        let indexed: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM memories_vec WHERE rowid = ?1",
                params![memory.id],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(indexed, 1);

        let (nearest, distance) = storage
            .knn_episodic(&vector(0, 0.0), 1)
            .expect("knn")
            .remove(0);
        assert_eq!(nearest, memory);
        assert!(distance.abs() < 1e-6, "distance {distance}");
    }

    #[test]
    fn core_memories_never_touch_the_vec_table() {
        let (_dir, storage) = open("core");
        storage
            .add_memory(MemoryTier::Core, "name is Mitul", None)
            .expect("add core");
        assert_eq!(vec_rows(&storage), 0);

        let memory = storage.list_memories(Some(MemoryTier::Core)).expect("list")[0].clone();
        storage
            .update_memory(memory.id, "name is still Mitul", None)
            .expect("update core");
        assert_eq!(vec_rows(&storage), 0);

        storage.delete_memory(memory.id).expect("delete core");
        assert!(storage.list_memories(None).expect("list").is_empty());
    }

    #[test]
    fn tier_rules_are_enforced() {
        let (_dir, storage) = open("tiers");
        assert!(matches!(
            storage.add_memory(MemoryTier::Core, "core", Some(&vector(0, 0.0))),
            Err(StorageError::EmbeddingForbidden)
        ));
        assert!(matches!(
            storage.add_memory(MemoryTier::Episodic, "episodic", None),
            Err(StorageError::EmbeddingRequired)
        ));
        assert!(matches!(
            storage.add_memory(MemoryTier::Episodic, "short", Some(&[1.0, 0.0])),
            Err(StorageError::EmbeddingDimensions {
                expected: EMBEDDING_DIM,
                got: 2
            })
        ));
        assert!(matches!(
            storage.add_memory(MemoryTier::Core, " \n ", None),
            Err(StorageError::EmptyMemory)
        ));
        assert!(matches!(
            storage.knn_episodic(&[1.0], 3),
            Err(StorageError::EmbeddingDimensions {
                expected: EMBEDDING_DIM,
                got: 1
            })
        ));
        assert!(storage.list_memories(None).expect("list").is_empty());
        assert_eq!(vec_rows(&storage), 0);
    }

    #[test]
    fn content_is_stored_single_line_with_chars_over_four_tokens() {
        let (_dir, storage) = open("normalize");
        let memory = storage
            .add_memory(MemoryTier::Core, "  a\nb\r\n\nc  d\te ", None)
            .expect("add");
        assert_eq!(memory.content, "a b c  d\te");
        assert_eq!(memory.tokens, 3, "10 chars round up to 3");
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
    }

    #[test]
    fn knn_returns_the_query_cluster_closest_first() {
        let (_dir, storage) = open("knn");
        let a1 = storage
            .add_memory(MemoryTier::Episodic, "a1", Some(&vector(0, 0.0)))
            .expect("a1");
        let a2 = storage
            .add_memory(MemoryTier::Episodic, "a2", Some(&vector(0, 0.1)))
            .expect("a2");
        let a3 = storage
            .add_memory(MemoryTier::Episodic, "a3", Some(&vector(0, 0.2)))
            .expect("a3");
        for (axis, name) in [(10, "b1"), (10, "b2"), (20, "c1")] {
            let lean = if name == "b2" { 0.1 } else { 0.0 };
            storage
                .add_memory(MemoryTier::Episodic, name, Some(&vector(axis, lean)))
                .expect(name);
        }
        storage
            .add_memory(MemoryTier::Core, "never retrieved", None)
            .expect("core");

        let neighbors = storage.knn_episodic(&vector(0, 0.0), 3).expect("knn");
        let ids: Vec<i64> = neighbors.iter().map(|(memory, _)| memory.id).collect();
        assert_eq!(ids, vec![a1.id, a2.id, a3.id]);
        assert!(neighbors[0].1 < neighbors[1].1 && neighbors[1].1 < neighbors[2].1);

        let all = storage.knn_episodic(&vector(0, 0.0), 100).expect("knn all");
        assert_eq!(all.len(), 6, "core rows must never be retrievable");
    }

    #[test]
    fn updating_an_episodic_memory_replaces_its_index_entry() {
        let (_dir, storage) = open("update");
        let memory = storage
            .add_memory(MemoryTier::Episodic, "old spot", Some(&vector(0, 0.0)))
            .expect("add");
        storage
            .add_memory(MemoryTier::Episodic, "bystander", Some(&vector(20, 0.0)))
            .expect("add bystander");

        let updated = storage
            .update_memory(memory.id, "new\nspot", Some(&vector(10, 0.0)))
            .expect("update");
        assert_eq!(updated.content, "new spot");
        assert_eq!(updated.created_at, memory.created_at);
        assert!(updated.updated_at >= memory.updated_at);
        assert_eq!(vec_rows(&storage), 2);

        let (nearest, distance) = storage
            .knn_episodic(&vector(10, 0.0), 1)
            .expect("knn")
            .remove(0);
        assert_eq!(nearest.id, memory.id);
        assert!(distance.abs() < 1e-6);
        let (at_old_spot, old_distance) = storage
            .knn_episodic(&vector(0, 0.0), 1)
            .expect("knn old")
            .remove(0);
        assert!(
            at_old_spot.id != memory.id || old_distance > 1.0,
            "the old embedding must be gone"
        );

        assert!(matches!(
            storage.update_memory(memory.id, "no embedding", None),
            Err(StorageError::EmbeddingRequired)
        ));
        assert!(matches!(
            storage.update_memory(9999, "ghost", None),
            Err(StorageError::MemoryNotFound(9999))
        ));
    }

    #[test]
    fn deleting_an_episodic_memory_removes_its_index_entry() {
        let (_dir, storage) = open("delete");
        let memory = storage
            .add_memory(MemoryTier::Episodic, "gone soon", Some(&vector(0, 0.0)))
            .expect("add");
        storage.delete_memory(memory.id).expect("delete");
        assert_eq!(vec_rows(&storage), 0);
        assert!(matches!(
            storage.memory(memory.id),
            Err(StorageError::MemoryNotFound(_))
        ));
        assert!(matches!(
            storage.delete_memory(memory.id),
            Err(StorageError::MemoryNotFound(_))
        ));
        assert!(storage
            .knn_episodic(&vector(0, 0.0), 5)
            .expect("knn")
            .is_empty());
    }

    #[test]
    fn list_filters_by_tier() {
        let (_dir, storage) = open("list");
        let core = storage
            .add_memory(MemoryTier::Core, "core row", None)
            .expect("core");
        let episodic = storage
            .add_memory(MemoryTier::Episodic, "episodic row", Some(&vector(0, 0.0)))
            .expect("episodic");

        let all = storage.list_memories(None).expect("all");
        assert_eq!(all, vec![core.clone(), episodic.clone()]);
        assert_eq!(
            storage.list_memories(Some(MemoryTier::Core)).expect("core"),
            vec![core]
        );
        assert_eq!(
            storage
                .list_memories(Some(MemoryTier::Episodic))
                .expect("episodic"),
            vec![episodic]
        );
    }
    #[test]
    fn recording_injections_again_for_a_message_replaces_the_record() {
        let (_dir, storage) = open("reinject");
        let first = storage
            .add_memory(MemoryTier::Episodic, "first", Some(&vector(0, 0.0)))
            .expect("first");
        let second = storage
            .add_memory(MemoryTier::Episodic, "second", Some(&vector(10, 0.0)))
            .expect("second");
        let conversation = storage
            .create_conversation("c", "ollama", "llama3.2:3b")
            .expect("create");
        let message = storage
            .append_message(conversation.id, crate::chat::Role::User, "q", None, None)
            .expect("message");

        storage
            .record_injections(conversation.id, Some(message.id), &[first.id])
            .expect("record");
        storage
            .record_injections(conversation.id, Some(message.id), &[second.id])
            .expect("re-record");

        let recorded: Vec<i64> = storage
            .injections(conversation.id)
            .expect("injections")
            .into_iter()
            .map(|injection| injection.memory_id)
            .collect();
        assert_eq!(recorded, vec![second.id], "the retry must replace, not add");
    }
    #[test]
    fn the_episodic_overview_sorts_by_recency_hits_and_creation() {
        let (_dir, storage) = open("overview");
        let a = storage
            .add_memory(MemoryTier::Episodic, "a", Some(&vector(0, 0.0)))
            .expect("a");
        let b = storage
            .add_memory(MemoryTier::Episodic, "b", Some(&vector(10, 0.0)))
            .expect("b");
        let c = storage
            .add_memory(MemoryTier::Episodic, "c", Some(&vector(20, 0.0)))
            .expect("c");
        storage
            .add_memory(MemoryTier::Core, "never listed", None)
            .expect("core");
        for (id, created, updated) in [(a.id, 10, 40), (b.id, 20, 60), (c.id, 30, 50)] {
            storage
                .conn
                .execute(
                    "UPDATE memories SET created_at = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, created, updated],
                )
                .expect("backdate");
        }
        let conversation = storage
            .create_conversation("s", "ollama", "llama3.2:3b")
            .expect("create");
        storage
            .record_injections(conversation.id, None, &[a.id])
            .expect("inject a");
        storage
            .record_injections(conversation.id, None, &[a.id, c.id])
            .expect("inject a and c");

        let ids = |rows: Vec<MemoryStats>| {
            rows.into_iter()
                .map(|row| row.memory.id)
                .collect::<Vec<_>>()
        };
        let recent = storage
            .episodic_overview(EpisodicSort::Recent, 10, 0)
            .expect("recent");
        assert_eq!(recent[0].memory.id, b.id);
        assert_eq!(ids(recent), vec![b.id, c.id, a.id]);
        let hits = storage
            .episodic_overview(EpisodicSort::Hits, 10, 0)
            .expect("hits");
        assert_eq!(hits[0].memory.id, a.id, "a was injected twice");
        assert_eq!(hits[0].hits, 2);
        assert!(hits[0].last_injected_at.is_some());
        assert_eq!(
            ids(storage
                .episodic_overview(EpisodicSort::Created, 10, 0)
                .expect("created")),
            vec![c.id, b.id, a.id]
        );
        assert_eq!(
            ids(storage
                .episodic_overview(EpisodicSort::Created, 2, 1)
                .expect("page")),
            vec![b.id, a.id]
        );

        let stats = storage
            .stats_for(vec![a.clone(), b.clone()])
            .expect("stats");
        assert_eq!(stats[0].hits, 2);
        assert_eq!(stats[1].hits, 0);
        assert_eq!(stats[1].last_injected_at, None);
    }
}
