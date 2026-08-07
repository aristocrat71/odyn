//! The brain's index: rows derived from the note files, never authored here.
//!
//! `sync_notes` is the only writer of memory rows; the folder is the truth. A
//! memory row and its `memories_vec` row share a rowid and are written in one
//! transaction, because an index entry the folder no longer backs corrupts
//! retrieval silently.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Row};

use super::{now_secs, Storage, StorageError};
use crate::notes::NoteFile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    /// The note's file stem.
    pub slug: String,
    pub content: String,
    /// Unix epoch seconds — of the index row, not the file.
    pub created_at: i64,
    pub updated_at: i64,
    /// chars/4 approximation, mirrored from the note.
    pub tokens: i64,
}

impl Memory {
    pub fn display_id(&self) -> String {
        self.slug.clone()
    }
}

/// One recorded use of a memory. `message_id` outlives its message as `None`.
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
pub enum MemorySort {
    Recent,
    Hits,
    Created,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStats {
    pub memory: Memory,
    pub hits: i64,
    pub last_injected_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotePlan {
    /// New or edited notes, in folder order. Embed exactly these.
    pub stale: Vec<String>,
    /// True when a sync would touch the index — including pure deletions.
    pub changed: bool,
}

impl Storage {
    /// Whether the index was built by `model`; if not it must be rebuilt before
    /// it can be trusted.
    pub fn index_matches(&self, model: &str) -> Result<bool, StorageError> {
        Ok(self.active_model()?.as_deref() == Some(model))
    }

    /// Points the index at `model`, whose vectors are `dim` wide, dropping every
    /// vector the previous model produced: vectors from two models are
    /// incomparable. Memory rows, ids, hit counts and injections all survive.
    pub fn rebuild_index(&self, model: &str, dim: usize) -> Result<(), StorageError> {
        if dim == 0 {
            return Err(StorageError::EmbeddingDimensions {
                expected: 1,
                got: 0,
            });
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(&format!(
            "DROP TABLE IF EXISTS memories_vec;
             CREATE VIRTUAL TABLE memories_vec USING vec0(embedding float[{dim}]);"
        ))?;
        tx.execute(
            "INSERT OR REPLACE INTO brain_meta (id, model, dim) VALUES (1, ?1, ?2)",
            params![model, dim as i64],
        )?;
        // Similarity edges are the old model's opinion; they go with it.
        invalidate_graph(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn active_model(&self) -> Result<Option<String>, StorageError> {
        let found = self
            .conn
            .query_row("SELECT model FROM brain_meta WHERE id = 1", [], |row| {
                row.get(0)
            });
        match found {
            Ok(model) => Ok(Some(model)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(other) => Err(StorageError::Sqlite(other)),
        }
    }

    /// 0 before an index has been built.
    pub fn index_dim(&self) -> Result<usize, StorageError> {
        let found = self
            .conn
            .query_row("SELECT dim FROM brain_meta WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            });
        match found {
            Ok(dim) => Ok(dim as usize),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(other) => Err(StorageError::Sqlite(other)),
        }
    }

    /// Compares the folder against the index. Read-only, so callers can embed
    /// the stale notes without holding the lock that guards this storage.
    pub fn note_sync_plan(&self, notes: &[NoteFile]) -> Result<NotePlan, StorageError> {
        let indexed = self.indexed_hashes()?;
        let vectored = self.vec_rowids()?;
        let stale: Vec<String> = notes
            .iter()
            .filter(|note| is_stale(&indexed, &vectored, note))
            .map(|note| note.slug.clone())
            .collect();
        let kept: HashSet<&str> = notes.iter().map(|note| note.slug.as_str()).collect();
        let deleted = indexed.keys().any(|slug| !kept.contains(slug.as_str()));
        Ok(NotePlan {
            changed: !stale.is_empty() || deleted,
            stale,
        })
    }

    /// Mirrors the folder into the index. An edit rewrites in place, keeping the
    /// id and so the hit history. `embeddings` must cover every stale slug.
    /// Answers whether anything changed, and invalidates the graph when so.
    pub fn sync_notes(
        &self,
        notes: &[NoteFile],
        embeddings: &[(String, Vec<f32>)],
    ) -> Result<bool, StorageError> {
        let dim = self.index_dim()?;
        for (_, embedding) in embeddings {
            check_dimensions(embedding, dim)?;
        }
        let vectors: HashMap<&str, &[f32]> = embeddings
            .iter()
            .map(|(slug, embedding)| (slug.as_str(), embedding.as_slice()))
            .collect();
        let indexed = self.indexed_hashes()?;
        let vectored = self.vec_rowids()?;
        let now = now_secs();
        let mut changed = false;
        let tx = self.conn.unchecked_transaction()?;

        let kept: HashSet<&str> = notes.iter().map(|note| note.slug.as_str()).collect();
        for (slug, (id, _)) in &indexed {
            if kept.contains(slug.as_str()) {
                continue;
            }
            tx.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
            tx.execute("DELETE FROM memories_vec WHERE rowid = ?1", params![id])?;
            changed = true;
        }

        for note in notes {
            // The same staleness rule the plan used, so a note whose vector went
            // missing is rebuilt even though its content never changed.
            if !is_stale(&indexed, &vectored, note) {
                continue;
            }
            let id = match indexed.get(&note.slug) {
                Some((id, _)) => {
                    tx.execute(
                        "UPDATE memories
                         SET content = ?2, hash = ?3, tokens = ?4, updated_at = ?5
                         WHERE id = ?1",
                        params![id, note.content, note.hash, note.tokens, now],
                    )?;
                    tx.execute("DELETE FROM memories_vec WHERE rowid = ?1", params![id])?;
                    tx.execute("DELETE FROM memory_links WHERE from_id = ?1", params![id])?;
                    *id
                }
                None => {
                    tx.execute(
                        "INSERT INTO memories (slug, content, hash, tokens, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![note.slug, note.content, note.hash, note.tokens, now],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            let embedding = vectors
                .get(note.slug.as_str())
                .ok_or_else(|| StorageError::MissingEmbedding(note.slug.clone()))?;
            tx.execute(
                "INSERT INTO memories_vec (rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_blob(embedding)],
            )?;
            for target in &note.links {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_links (from_id, to_slug) VALUES (?1, ?2)",
                    params![id, target],
                )?;
            }
            changed = true;
        }

        if changed {
            invalidate_graph(&tx)?;
        }
        tx.commit()?;
        Ok(changed)
    }

    fn indexed_hashes(&self) -> Result<HashMap<String, (i64, i64)>, StorageError> {
        let mut stmt = self.conn.prepare("SELECT slug, id, hash FROM memories")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    /// Which memories currently have a vector. A row without one is invisible to
    /// retrieval, so this is what keeps the index self-healing.
    fn vec_rowids(&self) -> Result<HashSet<i64>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id FROM memories AS m WHERE EXISTS (
                         SELECT 1 FROM memories_vec AS v WHERE v.rowid = m.id)",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    pub fn list_memories(&self) -> Result<Vec<Memory>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, content, created_at, updated_at, tokens
             FROM memories ORDER BY id",
        )?;
        let rows = stmt.query_map([], to_memory)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn count_memories(&self) -> Result<i64, StorageError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))?)
    }

    pub fn memory(&self, id: i64) -> Result<Memory, StorageError> {
        self.conn
            .query_row(
                "SELECT id, slug, content, created_at, updated_at, tokens
                 FROM memories WHERE id = ?1",
                params![id],
                to_memory,
            )
            .map_err(|error| not_found(error, id))
    }

    /// Records what was injected for a message, replacing any earlier record for
    /// it: a retried turn's context is the one the model last saw, and counting
    /// it twice would inflate every hit statistic.
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

    /// One page of the brain list, with hit counts from the injections log.
    pub fn memories_overview(
        &self,
        sort: MemorySort,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryStats>, StorageError> {
        let order = match sort {
            MemorySort::Recent => "m.updated_at DESC, m.id DESC",
            MemorySort::Hits => "hits DESC, m.id DESC",
            MemorySort::Created => "m.created_at DESC, m.id DESC",
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id, m.slug, m.content, m.created_at, m.updated_at, m.tokens,
                    count(i.id) AS hits, max(i.injected_at)
             FROM memories AS m LEFT JOIN injections AS i ON i.memory_id = m.id
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

    /// The same stats for an arbitrary set, which keeps its order.
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

    /// Nearest memories, closest first, with their L2 distances.
    pub fn knn(&self, embedding: &[f32], k: usize) -> Result<Vec<(Memory, f64)>, StorageError> {
        check_dimensions(embedding, self.index_dim()?)?;
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.slug, m.content, m.created_at, m.updated_at, m.tokens, k.distance
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

    /// The k nearest neighbors of a stored memory, excluding itself.
    /// (vec0 only understands MATCH and k, so the self-row is dropped here.)
    pub fn neighbors(&self, id: i64, k: usize) -> Result<Vec<(i64, f64)>, StorageError> {
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

    /// Pairs injected for the same message at least `min` times, smaller id
    /// first, with the count.
    pub fn co_injections(&self, min: i64) -> Result<Vec<(i64, i64, i64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT a.memory_id, b.memory_id, count(*)
             FROM injections AS a
             JOIN injections AS b
               ON a.message_id = b.message_id AND a.memory_id < b.memory_id
             WHERE a.message_id IS NOT NULL
             GROUP BY a.memory_id, b.memory_id
             HAVING count(*) >= ?1",
        )?;
        let rows = stmt.query_map(params![min], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `[[wikilink]]` edges with both ends resolved, self-links dropped.
    /// Targets are stored lowercased; slugs resolve case-insensitively.
    pub fn links(&self) -> Result<Vec<(i64, i64)>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT l.from_id, m.id
             FROM memory_links AS l
             JOIN memories AS m ON lower(m.slug) = l.to_slug
             WHERE l.from_id != m.id
             ORDER BY l.from_id, m.id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
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

/// Stale when the content moved or the vector is missing — the latter is how a
/// model swap re-embeds a folder whose files never changed.
fn is_stale(
    indexed: &HashMap<String, (i64, i64)>,
    vectored: &HashSet<i64>,
    note: &NoteFile,
) -> bool {
    match indexed.get(&note.slug) {
        None => true,
        Some((id, hash)) => *hash != note.hash || !vectored.contains(id),
    }
}

fn check_dimensions(embedding: &[f32], expected: usize) -> Result<(), StorageError> {
    if embedding.len() != expected {
        return Err(StorageError::EmbeddingDimensions {
            expected,
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

fn not_found(error: rusqlite::Error, id: i64) -> StorageError {
    match error {
        rusqlite::Error::QueryReturnedNoRows => StorageError::MemoryNotFound(id),
        other => StorageError::Sqlite(other),
    }
}

fn to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        slug: row.get(1)?,
        content: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        tokens: row.get(5)?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::tests::TempDir;
    use super::*;
    use crate::notes::NoteFile;

    pub(crate) fn open(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        (dir, storage)
    }

    pub(crate) fn note(slug: &str, content: &str) -> NoteFile {
        note_with_links(slug, content, &[])
    }

    pub(crate) fn note_with_links(slug: &str, content: &str, links: &[&str]) -> NoteFile {
        NoteFile {
            slug: slug.to_string(),
            content: content.to_string(),
            links: links.iter().map(|link| link.to_lowercase()).collect(),
            hash: content
                .bytes()
                .fold(0i64, |hash, byte| hash.wrapping_mul(31) + i64::from(byte)),
            tokens: crate::notes::approx_tokens(content),
        }
    }

    /// A unit vector along `axis` at the default model's width; a bigger `lean`
    /// tilts it towards the next axis, and so further from the query.
    pub(crate) fn vector(axis: usize, lean: f32) -> Vec<f32> {
        wide_vector(axis, lean, crate::embed::FAKE_DIM)
    }

    pub(crate) fn wide_vector(axis: usize, lean: f32, dim: usize) -> Vec<f32> {
        let mut values = vec![0.0f32; dim];
        values[axis] = 1.0 - lean;
        values[axis + 1] = lean;
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut values {
            *value /= norm;
        }
        values
    }

    /// Syncs notes with axis-spread embeddings: note `i` lands on axis `10*i`.
    pub(crate) fn sync_spread(storage: &Storage, notes: &[NoteFile]) {
        let embeddings: Vec<(String, Vec<f32>)> = notes
            .iter()
            .enumerate()
            .map(|(index, note)| (note.slug.clone(), vector(index * 10, 0.0)))
            .collect();
        storage.sync_notes(notes, &embeddings).expect("sync");
    }

    fn vec_rows(storage: &Storage) -> i64 {
        storage
            .conn
            .query_row("SELECT count(*) FROM memories_vec", [], |row| row.get(0))
            .expect("count vec rows")
    }

    #[test]
    fn the_plan_names_new_edited_and_deleted_notes() {
        let (_dir, storage) = open("plan");
        let first = vec![note("alpha", "one"), note("beta", "two")];
        let plan = storage.note_sync_plan(&first).expect("plan");
        assert_eq!(plan.stale, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(plan.changed);
        sync_spread(&storage, &first);

        let unchanged = storage.note_sync_plan(&first).expect("plan again");
        assert!(unchanged.stale.is_empty());
        assert!(!unchanged.changed);

        let edited = vec![note("alpha", "one, edited"), note("beta", "two")];
        let plan = storage.note_sync_plan(&edited).expect("plan edited");
        assert_eq!(plan.stale, vec!["alpha".to_string()]);
        assert!(plan.changed);

        let deleted = vec![note("beta", "two")];
        let plan = storage.note_sync_plan(&deleted).expect("plan deleted");
        assert!(plan.stale.is_empty());
        assert!(plan.changed, "a pure deletion still changes the index");
    }

    #[test]
    fn sync_mirrors_the_folder_and_keeps_ids_across_edits() {
        let (_dir, storage) = open("sync");
        let notes = vec![
            note_with_links("alpha", "links to [[beta]]", &["beta"]),
            note("beta", "plain"),
        ];
        sync_spread(&storage, &notes);
        let listed = storage.list_memories().expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].slug, "alpha");
        assert_eq!(listed[0].display_id(), "alpha");
        assert_eq!(vec_rows(&storage), 2);
        assert_eq!(
            storage.links().expect("links"),
            vec![(listed[0].id, listed[1].id)]
        );
        let alpha_id = listed[0].id;

        let edited = vec![
            note_with_links("alpha", "now links to nothing", &[]),
            note("beta", "plain"),
        ];
        let embeddings = vec![("alpha".to_string(), vector(50, 0.0))];
        assert!(storage.sync_notes(&edited, &embeddings).expect("resync"));
        let relisted = storage.list_memories().expect("relist");
        assert_eq!(relisted[0].id, alpha_id);
        assert_eq!(relisted[0].content, "now links to nothing");
        assert!(relisted[0].updated_at >= relisted[0].created_at);
        assert!(storage.links().expect("links").is_empty());
        let (nearest, distance) = storage.knn(&vector(50, 0.0), 1).expect("knn").remove(0);
        assert_eq!(nearest.id, alpha_id, "the embedding moved with the edit");
        assert!(distance.abs() < 1e-6);

        let conversation = storage
            .create_conversation("c", "ollama", "llama3.2:3b")
            .expect("create");
        storage
            .record_injections(conversation.id, None, &[alpha_id])
            .expect("inject");
        assert!(storage
            .sync_notes(&[note("beta", "plain")], &[])
            .expect("prune"));
        assert_eq!(storage.count_memories().expect("count"), 1);
        assert_eq!(vec_rows(&storage), 1);
        assert!(storage
            .injections(conversation.id)
            .expect("gone")
            .is_empty());

        assert!(!storage
            .sync_notes(&[note("beta", "plain")], &[])
            .expect("noop"));
    }

    #[test]
    fn sync_requires_an_embedding_for_every_stale_note() {
        let (_dir, storage) = open("missing");
        let err = storage
            .sync_notes(&[note("alpha", "text")], &[])
            .expect_err("no embedding given");
        assert!(matches!(err, StorageError::MissingEmbedding(slug) if slug == "alpha"));
        assert_eq!(storage.count_memories().expect("count"), 0, "rolled back");

        assert!(matches!(
            storage.sync_notes(
                &[note("alpha", "text")],
                &[("alpha".to_string(), vec![1.0, 2.0])]
            ),
            Err(StorageError::EmbeddingDimensions { expected, got })
                if expected == crate::embed::FAKE_DIM && got == 2
        ));
    }

    #[test]
    fn changing_the_model_rebuilds_the_index_but_keeps_history() {
        let (_dir, storage) = open("swap");
        let notes = vec![note("alpha", "one"), note("beta", "two")];
        sync_spread(&storage, &notes);
        let alpha = storage.list_memories().expect("list")[0].clone();

        let conversation = storage
            .create_conversation("c", "ollama", "llama3.2:3b")
            .expect("create");
        let message = storage
            .append_message(conversation.id, crate::chat::Role::User, "q", None, None)
            .expect("message");
        storage
            .record_injections(conversation.id, Some(message.id), &[alpha.id])
            .expect("inject");
        assert_eq!(
            storage.active_model().expect("model").as_deref(),
            Some("bge-small")
        );
        assert!(storage.index_matches("bge-small").expect("matches"));

        const WIDE_DIM: usize = 768;
        assert!(!storage.index_matches("bge-base").expect("differs"));
        storage.rebuild_index("bge-base", WIDE_DIM).expect("swap");
        assert_eq!(
            storage.active_model().expect("model").as_deref(),
            Some("bge-base")
        );
        assert!(storage.index_matches("bge-base").expect("settled"));

        let plan = storage.note_sync_plan(&notes).expect("plan");
        assert_eq!(
            plan.stale,
            vec!["alpha".to_string(), "beta".to_string()],
            "every note re-embeds, though no file changed"
        );

        assert!(matches!(
            storage.sync_notes(&notes, &[("alpha".to_string(), vector(0, 0.0))]),
            Err(StorageError::EmbeddingDimensions { expected, got })
                if expected == WIDE_DIM && got == crate::embed::FAKE_DIM
        ));
        let embeddings = vec![
            ("alpha".to_string(), wide_vector(0, 0.0, WIDE_DIM)),
            ("beta".to_string(), wide_vector(10, 0.0, WIDE_DIM)),
        ];
        assert!(storage.sync_notes(&notes, &embeddings).expect("re-embed"));

        let relisted = storage.list_memories().expect("relist");
        assert_eq!(relisted[0].id, alpha.id, "ids survive the swap");
        assert_eq!(relisted[0].created_at, alpha.created_at);
        assert_eq!(
            storage.stats_for(vec![relisted[0].clone()]).expect("stats")[0].hits,
            1,
            "hit history survives the swap"
        );
        let found = storage
            .knn(&wide_vector(0, 0.0, WIDE_DIM), 1)
            .expect("knn at the new width");
        assert_eq!(found[0].0.id, alpha.id);
        assert!(storage
            .note_sync_plan(&notes)
            .expect("settled")
            .stale
            .is_empty());
    }

    #[test]
    fn a_memory_without_a_vector_is_stale_even_when_its_content_is_unchanged() {
        let (_dir, storage) = open("orphan");
        let notes = vec![note("alpha", "one")];
        sync_spread(&storage, &notes);
        assert!(storage
            .note_sync_plan(&notes)
            .expect("plan")
            .stale
            .is_empty());

        let id = storage.list_memories().expect("list")[0].id;
        storage
            .conn
            .execute("DELETE FROM memories_vec WHERE rowid = ?1", params![id])
            .expect("drop the vector");

        let plan = storage.note_sync_plan(&notes).expect("plan");
        assert_eq!(plan.stale, vec!["alpha".to_string()]);
        assert!(plan.changed);
        sync_spread(&storage, &notes);
        assert_eq!(vec_rows(&storage), 1, "the sync put it back");
    }

    #[test]
    fn knn_returns_the_query_cluster_closest_first() {
        let (_dir, storage) = open("knn");
        let notes = vec![note("a1", "x"), note("a2", "x"), note("a3", "x")];
        let embeddings = vec![
            ("a1".to_string(), vector(0, 0.0)),
            ("a2".to_string(), vector(0, 0.1)),
            ("a3".to_string(), vector(10, 0.0)),
        ];
        storage.sync_notes(&notes, &embeddings).expect("sync");

        let neighbors = storage.knn(&vector(0, 0.0), 2).expect("knn");
        let slugs: Vec<&str> = neighbors
            .iter()
            .map(|(memory, _)| memory.slug.as_str())
            .collect();
        assert_eq!(slugs, vec!["a1", "a2"]);
        assert!(neighbors[0].1 < neighbors[1].1);
        assert_eq!(storage.knn(&vector(0, 0.0), 100).expect("all").len(), 3);
        assert!(matches!(
            storage.knn(&[1.0], 3),
            Err(StorageError::EmbeddingDimensions { .. })
        ));

        let near = storage.neighbors(neighbors[0].0.id, 1).expect("neighbors");
        assert_eq!(near.len(), 1);
        assert_ne!(near[0].0, neighbors[0].0.id, "self is excluded");
    }

    #[test]
    fn links_resolve_case_insensitively_and_skip_dangling_and_self() {
        let (_dir, storage) = open("links");
        let notes = vec![
            note_with_links(
                "Alpha",
                "see [[beta]], [[ghost]] and [[alpha]]",
                &["beta", "ghost", "alpha"],
            ),
            note("beta", "linked to"),
        ];
        sync_spread(&storage, &notes);
        let ids: HashMap<String, i64> = storage
            .list_memories()
            .expect("list")
            .into_iter()
            .map(|memory| (memory.slug.clone(), memory.id))
            .collect();
        assert_eq!(
            storage.links().expect("links"),
            vec![(ids["Alpha"], ids["beta"])],
            "dangling and self links resolve to nothing"
        );
    }

    #[test]
    fn recording_injections_again_for_a_message_replaces_the_record() {
        let (_dir, storage) = open("reinject");
        let notes = vec![note("first", "x"), note("second", "y")];
        sync_spread(&storage, &notes);
        let ids: Vec<i64> = storage
            .list_memories()
            .expect("list")
            .into_iter()
            .map(|memory| memory.id)
            .collect();
        let conversation = storage
            .create_conversation("c", "ollama", "llama3.2:3b")
            .expect("create");
        let message = storage
            .append_message(conversation.id, crate::chat::Role::User, "q", None, None)
            .expect("message");

        storage
            .record_injections(conversation.id, Some(message.id), &[ids[0]])
            .expect("record");
        storage
            .record_injections(conversation.id, Some(message.id), &[ids[1]])
            .expect("re-record");

        let recorded: Vec<i64> = storage
            .injections(conversation.id)
            .expect("injections")
            .into_iter()
            .map(|injection| injection.memory_id)
            .collect();
        assert_eq!(recorded, vec![ids[1]], "the retry must replace, not add");
    }

    #[test]
    fn co_injections_count_pairs_used_together() {
        let (_dir, storage) = open("co");
        let notes = vec![note("a", "x"), note("b", "y"), note("c", "z")];
        sync_spread(&storage, &notes);
        let ids: Vec<i64> = storage
            .list_memories()
            .expect("list")
            .into_iter()
            .map(|memory| memory.id)
            .collect();
        let conversation = storage
            .create_conversation("co", "ollama", "llama3.2:3b")
            .expect("create");
        for question in ["one", "two", "three"] {
            let row = storage
                .append_message(
                    conversation.id,
                    crate::chat::Role::User,
                    question,
                    None,
                    None,
                )
                .expect("message");
            storage
                .record_injections(conversation.id, Some(row.id), &[ids[0], ids[1]])
                .expect("inject");
        }
        assert_eq!(
            storage.co_injections(2).expect("pairs"),
            vec![(ids[0], ids[1], 3)]
        );
        assert!(storage.co_injections(4).expect("pairs").is_empty());
    }

    #[test]
    fn the_overview_sorts_by_recency_hits_and_creation() {
        let (_dir, storage) = open("overview");
        let notes = vec![note("a", "x"), note("b", "y"), note("c", "z")];
        sync_spread(&storage, &notes);
        let listed = storage.list_memories().expect("list");
        let (a, b, c) = (listed[0].id, listed[1].id, listed[2].id);
        for (id, created, updated) in [(a, 10, 40), (b, 20, 60), (c, 30, 50)] {
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
            .record_injections(conversation.id, None, &[a])
            .expect("inject a");
        storage
            .record_injections(conversation.id, None, &[a, c])
            .expect("inject a and c");

        let ids = |rows: Vec<MemoryStats>| {
            rows.into_iter()
                .map(|row| row.memory.id)
                .collect::<Vec<_>>()
        };
        let recent = storage
            .memories_overview(MemorySort::Recent, 10, 0)
            .expect("recent");
        assert_eq!(ids(recent), vec![b, c, a]);
        let hits = storage
            .memories_overview(MemorySort::Hits, 10, 0)
            .expect("hits");
        assert_eq!(hits[0].memory.id, a, "a was injected twice");
        assert_eq!(hits[0].hits, 2);
        assert!(hits[0].last_injected_at.is_some());
        assert_eq!(
            ids(storage
                .memories_overview(MemorySort::Created, 10, 0)
                .expect("created")),
            vec![c, b, a]
        );
        assert_eq!(
            ids(storage
                .memories_overview(MemorySort::Created, 2, 1)
                .expect("page")),
            vec![b, a]
        );

        let listed = storage.list_memories().expect("list");
        let stats = storage
            .stats_for(vec![listed[0].clone(), listed[1].clone()])
            .expect("stats");
        assert_eq!(stats[0].hits, 2);
        assert_eq!(stats[1].hits, 0);
        assert_eq!(stats[1].last_injected_at, None);
    }
}
