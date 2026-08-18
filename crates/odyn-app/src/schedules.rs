//! The scheduled-ask runner: a due row becomes a normal conversation —
//! created, asked, answered and recorded like any send, then announced in
//! spotlight. Nothing about the run is hidden.

use odyn_core::brain;
use odyn_core::chat::{Message, Role};
use odyn_core::reminder::now_secs;
use odyn_core::storage::Schedule;
use odyn_core::tools::{self, TurnError};
use tauri::{AppHandle, Manager};

use crate::commands::title_from;
use crate::state::AppState;

const UNATTENDED: &str = "no tools on a scheduled run";

pub(crate) async fn run(app: AppHandle, schedule: Schedule) {
    let outcome = converse(&app, &schedule).await;
    let Ok(ready) = app.state::<AppState>().inner().ready() else {
        return;
    };
    match outcome {
        Ok(conversation_id) => {
            let _ = ready
                .storage()
                .note_schedule_run(schedule.id, now_secs(), None);
            drop(ready);
            crate::reminders::notify_ran(&app, title_from(&schedule.prompt), conversation_id);
        }
        // The failure lands on the row; the next tick of its schedule retries.
        Err(err) => {
            let _ = ready
                .storage()
                .note_schedule_run(schedule.id, now_secs(), Some(&err));
        }
    }
}

/// The turn itself. Storage locks are per statement and never span an await.
async fn converse(app: &AppHandle, schedule: &Schedule) -> Result<i64, String> {
    let ask = brain::parse_ask(&schedule.prompt);
    let (provider, brevity, save_temperature, brain_dir, conversation_id, question_id) = {
        let ready = app.state::<AppState>().inner().ready()?;
        let provider = ready
            .registry
            .provider(&schedule.provider)
            .map_err(|err| err.to_string())?;
        let brain_dir = odyn_core::notes::brain_dir(ready.config.brain.path.as_deref())
            .map_err(|err| err.to_string())?;
        let storage = ready.storage();
        let row = storage
            .create_conversation(
                &title_from(&ask.message),
                &schedule.provider,
                &schedule.model,
            )
            .map_err(|err| err.to_string())?;
        let question = storage
            .append_message(row.id, Role::User, &ask.message, None, None)
            .map_err(|err| err.to_string())?;
        (
            provider,
            ready.config.style.brevity,
            ready.config.brain.save_temperature,
            brain_dir,
            row.id,
            question.id,
        )
    };
    // The same context path as a send: soul always, recall when the prompt
    // mentions /brain, and the injections recorded against the question.
    let context = crate::commands::build_context(app, Vec::new(), ask.clone(), brevity).await;
    let mut history = Vec::new();
    if let Some(context) = context {
        if !context.is_empty() {
            if let Ok(ready) = app.state::<AppState>().inner().ready() {
                let _ = ready.storage().record_injections(
                    Some(conversation_id),
                    Some(question_id),
                    &context.memory_ids(),
                );
            }
        }
        if !context.system_message.is_empty() {
            history.push(Message::new(Role::System, context.system_message));
        }
    }
    history.push(Message::new(Role::User, ask.message));
    // No tools: the run is unattended, and unattended turns write nothing.
    let mut refuse_reminder = |_: &str, _: i64, _: Option<&str>| Err(UNATTENDED.to_string());
    let mut refuse_schedule = |_: &str, _: &str, _: i64| Err(UNATTENDED.to_string());
    let mut refuse_bash = crate::commands::deny_bash();
    let mut effects = tools::Effects {
        brain_dir: &brain_dir,
        workspace: None,
        set_reminder: &mut refuse_reminder,
        set_schedule: &mut refuse_schedule,
        approve: &mut refuse_bash,
    };
    let reply = tools::run_turn(
        provider.as_ref(),
        &schedule.model,
        history,
        &[],
        &mut effects,
        save_temperature,
        |_| Ok(()),
    )
    .await
    .map_err(|err| match err {
        TurnError::Chat(err) => crate::commands::describe(&err),
        TurnError::Write(err) => err.to_string(),
    })?;
    if reply.text.trim().is_empty() {
        return Err("the model streamed no text".to_string());
    }
    let ready = app.state::<AppState>().inner().ready()?;
    ready
        .storage()
        .append_message(
            conversation_id,
            Role::Assistant,
            &reply.text,
            reply.usage.map(|usage| usage.input_tokens),
            reply.usage.map(|usage| usage.output_tokens),
        )
        .map_err(|err| err.to_string())?;
    Ok(conversation_id)
}
