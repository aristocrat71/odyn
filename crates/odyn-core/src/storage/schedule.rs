//! Scheduled asks: prompts the clock runs as normal conversations. A row is
//! re-armed before its run, so a crash mid-run can never tight-loop it.

use rusqlite::{params, Row};

use super::{now_secs, Storage, StorageError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: i64,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    /// The `every`-phrase deciding the next run.
    pub repeat: String,
    /// Unix epoch seconds of the next run.
    pub next_at: i64,
    pub created_at: i64,
    pub last_run_at: Option<i64>,
    /// What the last run failed with; `None` after a clean one.
    pub last_error: Option<String>,
}

impl Storage {
    pub fn add_schedule(
        &self,
        prompt: &str,
        provider: &str,
        model: &str,
        repeat: &str,
        next_at: i64,
    ) -> Result<Schedule, StorageError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(StorageError::EmptySchedule);
        }
        let created_at = now_secs();
        self.conn.execute(
            "INSERT INTO schedules (prompt, provider, model, repeat, next_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![prompt, provider, model, repeat, next_at, created_at],
        )?;
        Ok(Schedule {
            id: self.conn.last_insert_rowid(),
            prompt: prompt.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            repeat: repeat.to_string(),
            next_at,
            created_at,
            last_run_at: None,
            last_error: None,
        })
    }

    /// Due to run, in the order they came due.
    pub fn due_schedules(&self, now: i64) -> Result<Vec<Schedule>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, prompt, provider, model, repeat, next_at, created_at,
                    last_run_at, last_error
             FROM schedules WHERE next_at <= ?1 ORDER BY next_at, id",
        )?;
        let rows = stmt.query_map(params![now], to_schedule)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// When the clock should next wake for a schedule, if any exist.
    pub fn next_scheduled(&self) -> Result<Option<i64>, StorageError> {
        Ok(self
            .conn
            .query_row("SELECT min(next_at) FROM schedules", [], |row| row.get(0))?)
    }

    /// Soonest first, for the view.
    pub fn list_schedules(&self) -> Result<Vec<Schedule>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, prompt, provider, model, repeat, next_at, created_at,
                    last_run_at, last_error
             FROM schedules ORDER BY next_at, id",
        )?;
        let rows = stmt.query_map([], to_schedule)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn rearm_schedule(&self, id: i64, next_at: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE schedules SET next_at = ?2 WHERE id = ?1",
            params![id, next_at],
        )?;
        Ok(())
    }

    /// Records how a run went; a clean run clears the previous error.
    pub fn note_schedule_run(
        &self,
        id: i64,
        now: i64,
        error: Option<&str>,
    ) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE schedules SET last_run_at = ?2, last_error = ?3 WHERE id = ?1",
            params![id, now, error],
        )?;
        Ok(())
    }

    pub fn delete_schedule(&self, id: i64) -> Result<(), StorageError> {
        self.conn
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn to_schedule(row: &Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        prompt: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        repeat: row.get(4)?,
        next_at: row.get(5)?,
        created_at: row.get(6)?,
        last_run_at: row.get(7)?,
        last_error: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::storage::tests::TempDir;
    use crate::storage::Storage;

    fn storage(dir: &TempDir) -> Storage {
        Storage::open(dir.db()).expect("the database opens")
    }

    #[test]
    fn a_schedule_round_trips_rearms_and_records_its_runs() {
        let dir = TempDir::new("schedules");
        let storage = storage(&dir);
        assert_eq!(storage.next_scheduled().expect("empty"), None);

        let brief = storage
            .add_schedule(
                "morning brief",
                "ollama",
                "llama3.2:3b",
                "every day 09:00",
                1_000,
            )
            .expect("added");
        assert_eq!(storage.next_scheduled().expect("pending"), Some(1_000));
        assert!(storage.due_schedules(999).expect("due").is_empty());
        assert_eq!(
            storage.due_schedules(1_000).expect("due"),
            vec![brief.clone()]
        );

        storage.rearm_schedule(brief.id, 87_400).expect("rearmed");
        storage
            .note_schedule_run(brief.id, 1_005, Some("provider down"))
            .expect("noted");
        let listed = storage.list_schedules().expect("listed");
        assert_eq!(listed[0].next_at, 87_400);
        assert_eq!(listed[0].last_run_at, Some(1_005));
        assert_eq!(listed[0].last_error.as_deref(), Some("provider down"));

        storage
            .note_schedule_run(brief.id, 90_000, None)
            .expect("noted");
        assert_eq!(
            storage.list_schedules().expect("listed")[0].last_error,
            None
        );

        storage.delete_schedule(brief.id).expect("deleted");
        assert_eq!(storage.next_scheduled().expect("empty"), None);
    }

    #[test]
    fn a_schedule_without_a_prompt_is_refused() {
        let dir = TempDir::new("schedules-empty");
        let storage = storage(&dir);
        assert!(storage
            .add_schedule("  ", "ollama", "llama3.2:3b", "every 45m", 1_000)
            .is_err());
    }
}
