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
    /// Set when the row is a finished scheduled run; clicking it opens this.
    conversation_id: Option<i64>,
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
pub struct ScheduleRow {
    id: i64,
    prompt: String,
    repeat: String,
    next_at: i64,
    last_run_at: Option<i64>,
    /// What the last run failed with; `None` after a clean one.
    last_error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ReminderList {
    pending: Vec<ReminderRow>,
    past: Vec<ReminderRow>,
    schedules: Vec<ScheduleRow>,
}

impl From<odyn_core::storage::Schedule> for ScheduleRow {
    fn from(row: odyn_core::storage::Schedule) -> Self {
        Self {
            id: row.id,
            prompt: row.prompt,
            repeat: row.repeat,
            next_at: row.next_at,
            last_run_at: row.last_run_at,
            last_error: row.last_error,
        }
    }
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
    let schedules = ready.storage().list_schedules().map_err(say)?;
    Ok(ReminderList {
        pending: pending.into_iter().map(ReminderRow::from).collect(),
        past: past.into_iter().map(ReminderRow::from).collect(),
        schedules: schedules.into_iter().map(ScheduleRow::from).collect(),
    })
}

#[tauri::command]
pub async fn schedule_delete(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    let ready = state.ready()?;
    let deleted = ready.storage().delete_schedule(id);
    deleted.map_err(say)
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
            fire_schedules(&app);
            std::thread::sleep(nap(&app));
        }
    });
}

/// Until the next reminder or schedule, capped at `TICK` so one written
/// meanwhile is never waited out.
fn nap(app: &AppHandle) -> Duration {
    let Ok(ready) = app.state::<AppState>().inner().ready() else {
        return TICK;
    };
    let reminder = ready.storage().next_due().ok().flatten();
    let schedule = ready.storage().next_scheduled().ok().flatten();
    let next = match (reminder, schedule) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (one, other) => one.or(other),
    };
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
            conversation_id: None,
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

/// Runs whatever schedules came due. Each row is re-armed before its run, so
/// a run that crashes or hangs can never tight-loop the clock.
fn fire_schedules(app: &AppHandle) {
    let Ok(ready) = app.state::<AppState>().inner().ready() else {
        return;
    };
    let now = now_secs();
    let due = ready.storage().due_schedules(now).unwrap_or_default();
    for schedule in due {
        let next = reminder::parse_repeat(&schedule.repeat)
            .ok()
            .and_then(|repeat| reminder::next_fire(&repeat, now));
        // A phrase that stopped parsing waits a day rather than spinning.
        let rearmed = ready
            .storage()
            .rearm_schedule(schedule.id, next.unwrap_or(now + 86_400));
        if rearmed.is_ok() {
            tauri::async_runtime::spawn(crate::schedules::run(app.clone(), schedule));
        }
    }
}

/// Shows a finished scheduled run in the panel; the row opens its conversation.
pub(crate) fn notify_ran(app: &AppHandle, text: String, conversation_id: i64) {
    let payload = vec![Due {
        text,
        due_at: now_secs(),
        conversation_id: Some(conversation_id),
    }];
    if app.emit_to(SPOTLIGHT, EVENT, payload).is_ok() {
        crate::spotlight::show_for_reminder(app);
    }
}
