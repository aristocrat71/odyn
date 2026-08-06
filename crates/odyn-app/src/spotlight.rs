//! Spotlight window: summoned by a global hotkey, hidden by Esc or a second
//! press. Hotkey registration failure is surfaced as status, never a crash.
//! Asks are ephemeral — nothing is stored unless the exchange is promoted.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use odyn_core::chat::{ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, Usage};
use tauri::async_runtime::JoinHandle;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::commands::{describe, title_from, Body, Event, INTERRUPTED};
use crate::state::AppState;

const LABEL: &str = "spotlight";
const MAIN: &str = "main";
const EVENT: &str = "spotlight-event";
/// 640px field plus room for its 80px shadow to bleed inside the window.
const WIDTH: f64 = 720.0;

/// `None` while the hotkey works; the reason when it does not.
pub struct HotkeyStatus(Mutex<Option<String>>);

/// At most one ask lives at a time; a new question replaces the last.
#[derive(Default)]
pub struct AskState {
    next: AtomicU64,
    current: Mutex<Option<Ask>>,
}

struct Ask {
    question: String,
    shared: Arc<Shared>,
    task: Option<JoinHandle<()>>,
}

/// What the streaming task and a promote both need to see.
#[derive(Default)]
struct Shared {
    answer: Mutex<String>,
    usage: Mutex<Option<Usage>>,
    /// The memories injected for this ask, recorded if it is promoted.
    injected: Mutex<Vec<i64>>,
    finished: AtomicBool,
}

impl Shared {
    fn answer(&self) -> String {
        lock(&self.answer).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn setup(app: &AppHandle) {
    let hotkey = match app.state::<AppState>().ready() {
        Ok(ready) => ready.config.spotlight.hotkey.clone(),
        // Config is broken: the main window already explains that; the default
        // hotkey still gives spotlight a chance to work.
        Err(_) => odyn_core::config::SpotlightConfig::default().hotkey,
    };
    let error = register(app, &hotkey).err();
    app.manage(HotkeyStatus(Mutex::new(error)));
    app.manage(AskState::default());
}

fn register(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let shortcut: Shortcut = hotkey
        .parse()
        .map_err(|err| format!("hotkey `{hotkey}`: {err}"))?;
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                toggle(app);
            }
        })
        .map_err(|err| format!("hotkey `{hotkey}`: {err}"))
}

fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL).or_else(|| build(app)) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        present(app, &window);
    }
}

/// The in-app ⌘K, doing exactly what the global hotkey does.
#[tauri::command]
pub fn spotlight_toggle(app: AppHandle) {
    toggle(&app);
}

/// Wayland has no reliable always-on-top/frameless summon; a normal centered
/// window beats shipping hacks. `ODYN_SPOTLIGHT_FALLBACK=1` forces the same
/// path for testing anywhere.
fn fallback_mode() -> bool {
    std::env::var_os("ODYN_SPOTLIGHT_FALLBACK").is_some_and(|v| v == "1")
        || (cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some())
}

fn build(app: &AppHandle) -> Option<WebviewWindow> {
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("spotlight.html".into()))
        .title("odyn")
        .resizable(false)
        .visible(false)
        .accept_first_mouse(true);
    let builder = if fallback_mode() {
        builder.inner_size(WIDTH, 520.0).center()
    } else {
        builder
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .inner_size(WIDTH, 520.0)
    };
    builder.build().ok()
}

fn present(app: &AppHandle, window: &WebviewWindow) {
    if !fallback_mode() {
        place(app, window);
    }
    let _ = window.show();
    let _ = window.set_focus();
    let _ = tauri::Emitter::emit(window, "spotlight-show", ());
}

/// Field top lands at 38% of the height of the monitor holding the cursor.
fn place(app: &AppHandle, window: &WebviewWindow) {
    let monitor = monitor_with_cursor(app);
    let Some(monitor) = monitor else { return };
    let size = monitor.size();
    let position = monitor.position();
    let scale = monitor.scale_factor();
    let width = (WIDTH * scale) as u32;
    let height = (size.height as f64 * 0.52) as u32;
    let x = position.x + ((size.width.saturating_sub(width)) / 2) as i32;
    let y = position.y + (size.height as f64 * 0.38) as i32;
    let _ = window.set_size(PhysicalSize::new(width, height));
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn monitor_with_cursor(app: &AppHandle) -> Option<tauri::Monitor> {
    let monitors = app.available_monitors().ok()?;
    if let Ok(cursor) = app.cursor_position() {
        for monitor in &monitors {
            let p = monitor.position();
            let s = monitor.size();
            let inside_x = cursor.x >= p.x as f64 && cursor.x < (p.x + s.width as i32) as f64;
            let inside_y = cursor.y >= p.y as f64 && cursor.y < (p.y + s.height as i32) as f64;
            if inside_x && inside_y {
                return Some(monitor.clone());
            }
        }
    }
    app.primary_monitor()
        .ok()
        .flatten()
        .or(monitors.into_iter().next())
}

#[tauri::command]
pub fn spotlight_hide(app: AppHandle, asks: State<'_, AskState>) {
    // Dismissal drops the exchange: spotlight keeps nothing it wasn't asked to.
    if let Some(ask) = lock(&asks.current).take() {
        abort(&ask);
    }
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
    }
}

/// Streams an ephemeral answer to the spotlight window; a new question
/// replaces whatever the last one was still doing.
#[tauri::command]
pub fn spotlight_ask(
    app: AppHandle,
    state: State<'_, AppState>,
    asks: State<'_, AskState>,
    text: String,
) -> Result<u64, String> {
    let ready = state.ready()?;
    let request_id = asks.next.fetch_add(1, Ordering::Relaxed);
    if let Some(previous) = lock(&asks.current).take() {
        abort(&previous);
    }

    let shared = Arc::new(Shared::default());
    let mut ask = Ask {
        question: text.clone(),
        shared: Arc::clone(&shared),
        task: None,
    };

    match target(ready) {
        Ok((provider, model)) => match ready.registry.provider(&provider) {
            Ok(provider) => {
                let history = vec![Message::new(Role::User, text)];
                ask.task = Some(tauri::async_runtime::spawn(run(
                    app.clone(),
                    request_id,
                    shared,
                    provider,
                    model,
                    history,
                )));
            }
            Err(err) => emit(
                &app,
                request_id,
                Body::Error {
                    message: err.to_string(),
                },
            ),
        },
        Err(message) => emit(&app, request_id, Body::Error { message }),
    }

    *lock(&asks.current) = Some(ask);
    Ok(request_id)
}

/// Saves the current exchange as a real conversation and hands off to the main
/// window. Mid-stream, the answer is kept as an interrupted partial.
#[tauri::command]
pub async fn spotlight_promote(
    app: AppHandle,
    state: State<'_, AppState>,
    asks: State<'_, AskState>,
) -> Result<i64, String> {
    let ready = state.ready()?;
    let Some(ask) = lock(&asks.current).take() else {
        return Err("nothing to promote".to_string());
    };
    let finished = ask.shared.finished.load(Ordering::Acquire);
    abort(&ask);
    let mut answer = ask.shared.answer();
    if answer.trim().is_empty() {
        return Err("nothing to promote".to_string());
    }
    if !finished {
        answer.push_str(INTERRUPTED);
    }
    let usage = *lock(&ask.shared.usage);

    let (provider, model) = target(ready)?;
    let storage = ready.storage();
    let row = storage
        .create_conversation(&title_from(&ask.question), &provider, &model)
        .map_err(|err| err.to_string())?;
    let question = storage
        .append_message(row.id, Role::User, &ask.question, None, None)
        .map_err(|err| err.to_string())?;
    let injected = lock(&ask.shared.injected).clone();
    if !injected.is_empty() {
        storage
            .record_injections(row.id, Some(question.id), &injected)
            .map_err(|err| err.to_string())?;
    }
    storage
        .append_message(
            row.id,
            Role::Assistant,
            &answer,
            usage.map(|usage| usage.input_tokens),
            usage.map(|usage| usage.output_tokens),
        )
        .map_err(|err| err.to_string())?;
    drop(storage);

    let _ = app.emit_to(MAIN, "open-conversation", row.id);
    if let Some(main) = app.get_webview_window(MAIN) {
        let _ = main.unminimize();
        let _ = main.show();
        let _ = main.set_focus();
    }
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
    }
    Ok(row.id)
}

/// The spotlight target: its own config keys first, the app defaults after.
fn target(ready: &crate::state::Ready) -> Result<(String, String), String> {
    let provider = ready
        .config
        .spotlight
        .provider
        .clone()
        .unwrap_or_else(|| ready.registry.default_provider_name().to_string());
    let model = ready
        .config
        .spotlight
        .model
        .clone()
        .or_else(|| ready.config.default_model(&provider).map(str::to_string))
        .ok_or_else(|| "no spotlight model: set [spotlight] model in odyn.toml".to_string())?;
    Ok((provider, model))
}

fn abort(ask: &Ask) {
    if let Some(task) = &ask.task {
        if !ask.shared.finished.load(Ordering::Acquire) {
            task.abort();
        }
    }
}

async fn run(
    app: AppHandle,
    request_id: u64,
    shared: Arc<Shared>,
    provider: Box<dyn ChatProvider>,
    model: String,
    mut history: Vec<Message>,
) {
    // A spotlight ask has no history: the question alone drives retrieval.
    // Brevity comes from `[spotlight]`, never from any conversation.
    let brevity = app
        .state::<AppState>()
        .ready()
        .map(|ready| ready.config.spotlight.brevity)
        .unwrap_or_default();
    let question = history
        .last()
        .map(|last| last.content.clone())
        .unwrap_or_default();
    if let Some(context) = crate::commands::build_context(&app, Vec::new(), question, brevity).await
    {
        *lock(&shared.injected) = context.memory_ids();
        emit(&app, request_id, crate::commands::context_body(&context));
        if !context.system_message.is_empty() {
            history.insert(0, Message::new(Role::System, context.system_message));
        }
    }
    let mut events = provider.chat_stream(ChatRequest::new(&history, &model));
    while let Some(event) = events.next().await {
        match event {
            Ok(ChatEvent::TextDelta(delta)) => {
                lock(&shared.answer).push_str(&delta);
                emit(&app, request_id, Body::Delta { text: delta });
            }
            Ok(ChatEvent::Done { usage }) => {
                *lock(&shared.usage) = usage;
                shared.finished.store(true, Ordering::Release);
                emit(
                    &app,
                    request_id,
                    Body::Done {
                        usage,
                        interrupted: false,
                    },
                );
                return;
            }
            Err(ChatError::Cancelled) => return,
            Err(err) => {
                emit(
                    &app,
                    request_id,
                    Body::Error {
                        message: format!("stream failed: {}", describe(&err)),
                    },
                );
                return;
            }
        }
    }
    shared.finished.store(true, Ordering::Release);
    emit(
        &app,
        request_id,
        Body::Done {
            usage: None,
            interrupted: false,
        },
    );
}

fn emit(app: &AppHandle, request_id: u64, body: Body) {
    let _ = app.emit(EVENT, Event { request_id, body });
}

#[tauri::command]
pub fn spotlight_status(status: State<'_, HotkeyStatus>) -> Option<String> {
    status
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}
