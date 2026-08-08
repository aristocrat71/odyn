//! Auto-update against GitHub Releases. One tray item is the whole interface:
//! it relabels itself through checking, downloading and restarting, so there is
//! no window to open and nothing to dismiss. The launch check is silent unless
//! it finds something; a click is never silent, because silence reads as broken.

use std::sync::Mutex;

use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_updater::UpdaterExt;

pub const ID: &str = "update";
pub const IDLE: &str = "Check for updates…";

/// What the next click does. Anything else is a click landing mid-flight.
#[derive(Clone, Copy)]
enum Stage {
    Check,
    Restart,
    Busy,
}

struct Updater {
    stage: Mutex<Stage>,
    item: MenuItem<Wry>,
}

/// Take ownership of the tray item and run the launch check behind it.
pub fn arm(app: &AppHandle, item: MenuItem<Wry>) {
    app.manage(Updater {
        stage: Mutex::new(Stage::Busy),
        item,
    });
    spawn(app.clone(), false);
}

pub fn clicked(app: &AppHandle) {
    let Some(state) = app.try_state::<Updater>() else {
        return;
    };
    let Ok(mut stage) = state.stage.lock() else {
        return;
    };
    match *stage {
        Stage::Busy => {}
        // Diverges: the process is replaced, so the lock goes with it.
        Stage::Restart => app.restart(),
        Stage::Check => {
            *stage = Stage::Busy;
            drop(stage);
            spawn(app.clone(), true);
        }
    }
}

fn spawn(app: AppHandle, manual: bool) {
    tauri::async_runtime::spawn(async move { run(&app, manual).await });
}

async fn run(app: &AppHandle, manual: bool) {
    say(app, "Checking for updates…", false);

    // A dev build has no bundle to replace and offline has nothing to ask, and
    // neither is worth interrupting a launch over.
    let found = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(err) => Err(err),
    };
    let update = match found {
        Ok(Some(update)) => update,
        Ok(None) => return settle(app, manual, "odyn is up to date"),
        Err(_) => return settle(app, manual, "Couldn't check for updates"),
    };

    let version = update.version.clone();
    say(app, &format!("Downloading odyn {version}…"), false);

    let mut total = 0u64;
    let mut got = 0u64;
    let progress = |chunk: usize, length: Option<u64>| {
        got += chunk as u64;
        total = length.unwrap_or(total);
        // No length means no percentage to honestly report.
        if total > 0 {
            let pct = (got * 100 / total).min(100);
            say(app, &format!("Downloading odyn {version} · {pct}%"), false);
        }
    };
    match update.download_and_install(progress, || {}).await {
        // Staged, not live: the running copy is only replaced on relaunch.
        Ok(()) => {
            set_stage(app, Stage::Restart);
            say(app, "Restart to finish updating", true);
        }
        // Always speaks. This is the state that reads as "updates don't work".
        Err(_) => settle(app, true, "Update failed — click to retry"),
    }
}

/// Back to idle. `speak` keeps the outcome on the label for someone who asked
/// for it; a launch check leaves no trace.
fn settle(app: &AppHandle, speak: bool, outcome: &str) {
    set_stage(app, Stage::Check);
    say(app, if speak { outcome } else { IDLE }, true);
}

fn set_stage(app: &AppHandle, stage: Stage) {
    if let Some(state) = app.try_state::<Updater>() {
        if let Ok(mut current) = state.stage.lock() {
            *current = stage;
        }
    }
}

/// Relabel the tray item. Cosmetic, so a failure here is not worth reporting.
fn say(app: &AppHandle, text: &str, enabled: bool) {
    let Some(state) = app.try_state::<Updater>() else {
        return;
    };
    let _ = state.item.set_text(text);
    let _ = state.item.set_enabled(enabled);
}
