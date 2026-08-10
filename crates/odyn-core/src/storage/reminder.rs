//! Reminders: state with a deadline, so rows rather than notes. Firing is two
//! steps — read, then mark — so one that was never shown survives a restart.

use rusqlite::{params, Row};

use super::{now_secs, Storage, StorageError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    pub id: i64,
    pub text: String,
    /// Unix epoch seconds.
    pub due_at: i64,
    pub created_at: i64,
    /// When it was shown, which is not when it came due if Odyn was closed.
    pub fired_at: Option<i64>,
}

impl Storage {
    pub fn add_reminder(&self, text: &str, due_at: i64) -> Result<Reminder, StorageError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(StorageError::EmptyReminder);
        }
        let created_at = now_secs();
        self.conn.execute(
            "INSERT INTO reminders (text, due_at, created_at) VALUES (?1, ?2, ?3)",
            params![text, due_at, created_at],
        )?;
        Ok(Reminder {
            id: self.conn.last_insert_rowid(),
            text: text.to_string(),
            due_at,
            created_at,
            fired_at: None,
        })
    }

    /// Due and unshown, in the order they would have arrived.
    pub fn due_reminders(&self, now: i64) -> Result<Vec<Reminder>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, due_at, created_at, fired_at FROM reminders
             WHERE fired_at IS NULL AND due_at <= ?1 ORDER BY due_at, id",
        )?;
        let rows = stmt.query_map(params![now], to_reminder)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// When the scheduler should next wake, or `None` while nothing is pending.
    pub fn next_due(&self) -> Result<Option<i64>, StorageError> {
        Ok(self.conn.query_row(
            "SELECT min(due_at) FROM reminders WHERE fired_at IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Still waiting, soonest first.
    pub fn pending_reminders(&self) -> Result<Vec<Reminder>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, due_at, created_at, fired_at FROM reminders
             WHERE fired_at IS NULL ORDER BY due_at, id",
        )?;
        let rows = stmt.query_map([], to_reminder)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Already shown, most recent first.
    pub fn fired_reminders(&self, limit: i64) -> Result<Vec<Reminder>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, text, due_at, created_at, fired_at FROM reminders
             WHERE fired_at IS NOT NULL ORDER BY due_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], to_reminder)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Cancels one outright. A reminder is cheap to set again, so unlike a note
    /// it needs no trash.
    pub fn delete_reminder(&self, id: i64) -> Result<(), StorageError> {
        self.conn
            .execute("DELETE FROM reminders WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// One transaction: a partly-marked batch would show its tail twice.
    pub fn mark_fired(&self, ids: &[i64], now: i64) -> Result<(), StorageError> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.conn.unchecked_transaction()?;
        {
            let mut stmt = txn
                .prepare("UPDATE reminders SET fired_at = ?2 WHERE id = ?1 AND fired_at IS NULL")?;
            for id in ids {
                stmt.execute(params![id, now])?;
            }
        }
        txn.commit()?;
        Ok(())
    }
}

fn to_reminder(row: &Row<'_>) -> rusqlite::Result<Reminder> {
    Ok(Reminder {
        id: row.get(0)?,
        text: row.get(1)?,
        due_at: row.get(2)?,
        created_at: row.get(3)?,
        fired_at: row.get(4)?,
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
    fn a_reminder_round_trips_and_schedules_the_next_wake() {
        let dir = TempDir::new("reminders-round-trip");
        let storage = storage(&dir);
        assert_eq!(storage.next_due().expect("no reminders yet"), None);

        let later = storage.add_reminder("stand up", 2_000).expect("added");
        let sooner = storage
            .add_reminder("  drink water  ", 1_000)
            .expect("added");
        assert_eq!(sooner.text, "drink water");
        assert_eq!(storage.next_due().expect("pending"), Some(1_000));

        let pending = storage.pending_reminders().expect("pending");
        assert_eq!(
            pending.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![sooner.id, later.id]
        );
    }

    #[test]
    fn only_reminders_that_came_due_fire_and_they_fire_once() {
        let dir = TempDir::new("reminders-fire-once");
        let storage = storage(&dir);
        let first = storage.add_reminder("call mum", 1_000).expect("added");
        let second = storage.add_reminder("stretch", 1_500).expect("added");
        storage.add_reminder("much later", 9_000).expect("added");

        let due = storage.due_reminders(2_000).expect("due");
        assert_eq!(
            due.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![first.id, second.id]
        );

        storage
            .mark_fired(&[first.id, second.id], 2_001)
            .expect("marked");
        assert!(storage.due_reminders(2_000).expect("due").is_empty());
        assert_eq!(storage.next_due().expect("pending"), Some(9_000));
    }

    #[test]
    fn a_reminder_missed_while_closed_still_fires_on_the_next_look() {
        let dir = TempDir::new("reminders-catch-up");
        let storage = storage(&dir);
        let missed = storage.add_reminder("bin day", 1_000).expect("added");
        let due = storage.due_reminders(1_000 + 7 * 86_400).expect("due");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, missed.id);
    }

    #[test]
    fn the_list_splits_pending_from_shown_and_cancelling_removes_one() {
        let dir = TempDir::new("reminders-list");
        let storage = storage(&dir);
        let shown = storage.add_reminder("bin day", 1_000).expect("added");
        let soon = storage.add_reminder("call mum", 5_000).expect("added");
        let later = storage.add_reminder("dentist", 9_000).expect("added");
        storage.mark_fired(&[shown.id], 1_100).expect("marked");

        let pending = storage.pending_reminders().expect("pending");
        assert_eq!(
            pending.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![soon.id, later.id]
        );
        let past = storage.fired_reminders(10).expect("fired");
        assert_eq!(past.len(), 1);
        assert_eq!(past[0].id, shown.id);

        storage.delete_reminder(soon.id).expect("cancelled");
        let pending = storage.pending_reminders().expect("pending");
        assert_eq!(
            pending.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![later.id]
        );
        assert_eq!(storage.next_due().expect("pending"), Some(9_000));
    }

    #[test]
    fn a_reminder_with_nothing_to_say_is_refused() {
        let dir = TempDir::new("reminders-empty");
        let storage = storage(&dir);
        assert!(storage.add_reminder("   ", 1_000).is_err());
    }
}
