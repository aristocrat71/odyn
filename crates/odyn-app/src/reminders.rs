//! The reminder clock: one thread that wakes, asks what is due, and shows it in
//! spotlight. Sleeps are capped and every wake re-reads the wall clock, so a
//! machine that slept through a due time fires on its next tick instead.

use std::time::Duration;

use odyn_core::reminder::{self, now_secs};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

const EVENT: &str = "reminder-due";
const SPOTLIGHT: &str = "spotlight";
/// The longest the clock sleeps, and so the most a reminder can run late.
const TICK: Duration = Duration::from_secs(20);
/// Long enough for the spotlight webview to be listening; an event emitted
/// before it loads reaches nobody.
const SETTLE: Duration = Duration::from_secs(5);

/// How much of the shown history the list view keeps.
const PAST_SHOWN: i64 = 50;

#[derive(Clone, serde::Serialize)]
struct Due {
    text: String,
    due_at: i64,
}

#[derive(serde::Serialize)]
pub struct ReminderRow {
    id: i64,
    text: String,
    due_at: i64,
    /// `None` while it is still waiting.
    fired_at: Option<i64>,
    /// The `every`-phrase of a repeating reminder; `None` is one-shot.
    repeat: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ReminderList {
    pending: Vec<ReminderRow>,
    past: Vec<ReminderRow>,
}

impl From<odyn_core::storage::Reminder> for ReminderRow {
    fn from(row: odyn_core::storage::Reminder) -> Self {
        Self {
            id: row.id,
            text: row.text,
            due_at: row.due_at,
            fired_at: row.fired_at,
            repeat: row.repeat,
        }
    }
}

#[tauri::command]
pub async fn reminders_list(state: tauri::State<'_, AppState>) -> Result<ReminderList, String> {
    let ready = state.ready()?;
    // One storage lock per statement — a held guard once froze the whole app.
    let pending = ready.storage().pending_reminders().map_err(say)?;
    let past = ready.storage().fired_reminders(PAST_SHOWN).map_err(say)?;
    Ok(ReminderList {
        pending: pending.into_iter().map(ReminderRow::from).collect(),
        past: past.into_iter().map(ReminderRow::from).collect(),
    })
}

#[tauri::command]
pub async fn reminder_delete(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    let ready = state.ready()?;
    let deleted = ready.storage().delete_reminder(id);
    deleted.map_err(say)
}

fn say(err: odyn_core::storage::StorageError) -> String {
    err.to_string()
}

/// Starts the clock. A plain thread rather than a task: it spends its life
/// asleep, and a blocking sleep is what survives the runtime being busy.
pub(crate) fn setup(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SETTLE);
        loop {
            fire(&app);
            std::thread::sleep(nap(&app));
        }
    });
}

/// Until the next reminder, capped at `TICK` so one written meanwhile is never
/// waited out.
fn nap(app: &AppHandle) -> Duration {
    let Ok(ready) = app.state::<AppState>().inner().ready() else {
        return TICK;
    };
    let next = ready.storage().next_due().ok().flatten();
    let Some(at) = next else {
        return TICK;
    };
    let secs = at
        .saturating_sub(now_secs())
        .clamp(1, TICK.as_secs() as i64);
    Duration::from_secs(secs as u64)
}

/// Shows everything overdue, including whatever came due while Odyn was closed.
/// Marked only after the webview took it: a reminder nobody saw is not spent.
/// A repeating one re-arms from now instead — one catch-up firing, no backlog.
fn fire(app: &AppHandle) {
    let Ok(ready) = app.state::<AppState>().inner().ready() else {
        return;
    };
    let now = now_secs();
    // One storage lock per statement — a held guard once froze the whole app.
    let due = ready.storage().due_reminders(now).unwrap_or_default();
    if due.is_empty() {
        return;
    }
    let payload: Vec<Due> = due
        .iter()
        .map(|reminder| Due {
            text: reminder.text.clone(),
            due_at: reminder.due_at,
        })
        .collect();
    if app.emit_to(SPOTLIGHT, EVENT, payload).is_err() {
        return;
    }
    let mut spent = Vec::new();
    for reminder in due {
        match next_arm(&reminder, now) {
            Some(next) => {
                let _ = ready.storage().rearm(reminder.id, next);
            }
            None => spent.push(reminder.id),
        }
    }
    let _ = ready.storage().mark_fired(&spent, now);
    crate::spotlight::show_for_reminder(app);
}

/// A repeating row's next firing; a phrase that no longer parses burns out
/// like a one-shot rather than firing forever.
fn next_arm(row: &odyn_core::storage::Reminder, now: i64) -> Option<i64> {
    let repeat = reminder::parse_repeat(row.repeat.as_deref()?).ok()?;
    reminder::next_fire(&repeat, now)
}
