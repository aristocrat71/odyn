//! Spotlight window: summoned by a global hotkey, hidden by Esc or a second
//! press. Hotkey registration failure is surfaced as status, never a crash.
//! Asks are ephemeral — nothing is stored unless the exchange is promoted.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use odyn_core::chat::{ChatError, ChatEvent, ChatProvider, ChatRequest, Message, Role, Usage};
use odyn_core::config::{config_path, ConfigError, ProviderConfig};
use odyn_core::config_edit;
use tauri::async_runtime::JoinHandle;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};

use crate::commands::{describe, title_from, Body, Event, INTERRUPTED};
use crate::state::AppState;

const LABEL: &str = "spotlight";
const MAIN: &str = "main";
const EVENT: &str = "spotlight-event";
/// A rate limit, a bad request, a dropped connection and a wordless reply are
/// one thing from here: this model cannot answer. The raw text rides `detail`.
const UNAVAILABLE: &str = "model unavailable";
/// 640px field plus room for its 80px shadow to bleed inside the window.
const WIDTH: f64 = 720.0;

/// Registered only while spotlight shows, so Esc dismisses it even when the
/// webview never sees the key. Released again on hide — the asklight pattern.
fn esc() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

/// The real Spotlight behavior is an NSPanel, not a window: non-activating —
/// the app in front keeps focus while the panel takes keys — floating at
/// status level on every space. Ported from asklight.
#[cfg(target_os = "macos")]
mod panel {
    use tauri::{AppHandle, Manager, WebviewWindow};
    use tauri_nspanel::{
        tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt,
    };

    tauri_panel! {
        panel!(SpotPanel {
            config: {
                can_become_key_window: true,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }

    /// No delegate of our own: tao's stays on the window, so losing key
    /// status reaches us as Tauri's ordinary `Focused(false)` — the one
    /// dismissal path shared with the fallback window on other platforms.
    pub fn convert(window: &WebviewWindow) -> bool {
        let Ok(panel) = window.to_panel::<SpotPanel>() else {
            return false;
        };
        panel.set_style_mask(StyleMask::empty().nonactivating_panel().value());
        panel.set_level(PanelLevel::Status.value());
        panel.set_collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .stationary()
                .full_screen_auxiliary()
                .value(),
        );
        true
    }

    /// Show via the panel — `show_and_make_key` — never by converting it back
    /// to a window, which would silently drop the resign-key handler.
    pub fn show(app: &AppHandle) -> bool {
        match app.get_webview_panel(super::LABEL) {
            Ok(panel) => {
                panel.show_and_make_key();
                true
            }
            Err(_) => false,
        }
    }

    pub fn hide(app: &AppHandle) -> bool {
        match app.get_webview_panel(super::LABEL) {
            Ok(panel) => {
                if panel.is_visible() {
                    panel.hide();
                }
                true
            }
            Err(_) => false,
        }
    }
}

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
    // Built now, on the main thread, where AppKit wants it — never inside the
    // hotkey callback. On macOS the window immediately becomes an NSPanel.
    let _ = build(app);
}

fn register(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let shortcut: Shortcut = hotkey
        .parse()
        .map_err(|err| format!("hotkey `{hotkey}`: {err}"))?;
    app.global_shortcut()
        .on_shortcut(shortcut, |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                defer(app, toggle);
            }
        })
        .map_err(|err| format!("hotkey `{hotkey}`: {err}"))
}

/// AppKit window operations silently no-op when run synchronously inside the
/// hotkey callback (asklight's hard-won lesson). A thread hop makes
/// `run_on_main_thread` QUEUE onto the run loop instead of executing inline —
/// this is the difference between spotlight appearing and nothing happening.
fn defer(app: &AppHandle, act: fn(&AppHandle)) {
    let app = app.clone();
    std::thread::spawn(move || {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || act(&handle));
    });
}

fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LABEL).or_else(|| build(app)) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        conceal(app);
    } else {
        present(app, &window);
    }
}

/// Hides without dropping the ask: a stray click or a re-summon later still
/// finds the answer. Only Esc and promotion end an exchange.
fn conceal(app: &AppHandle) {
    let _ = app.global_shortcut().unregister(esc());
    #[cfg(target_os = "macos")]
    if !fallback_mode() && panel::hide(app) {
        return;
    }
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
    }
}

/// Esc semantics: the exchange is dropped, spotlight keeps nothing.
fn dismiss(app: &AppHandle) {
    if let Some(ask) = lock(&app.state::<AskState>().current).take() {
        abort(&ask);
    }
    conceal(app);
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
    let window = builder.build().ok()?;
    #[cfg(target_os = "macos")]
    if !fallback_mode() {
        panel::convert(&window);
    }
    // Standard Spotlight dismissal: focus gone — a click into another app, a
    // ⌘-tab away — hides it. AppKit's resign-key arrives as Tauri's own
    // focus event, so every platform shares this one path.
    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Focused(false)) {
            conceal(&handle);
        }
    });
    Some(window)
}

fn present(app: &AppHandle, window: &WebviewWindow) {
    if !fallback_mode() {
        place(app, window);
    }
    // While it shows, Esc reaches it from anywhere.
    let _ = app
        .global_shortcut()
        .on_shortcut(esc(), |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                defer(app, dismiss);
            }
        });
    #[cfg(target_os = "macos")]
    let shown = !fallback_mode() && panel::show(app);
    #[cfg(not(target_os = "macos"))]
    let shown = false;
    if !shown {
        let _ = window.show();
        let _ = window.set_focus();
    }
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
pub fn spotlight_hide(app: AppHandle) {
    dismiss(&app);
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

    match target(&ready) {
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
            // A misconfigured provider is the user's to fix, so it says so.
            Err(err) => emit(&app, request_id, Body::error(err.to_string())),
        },
        Err(message) => emit(&app, request_id, Body::error(message)),
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

    let (provider, model) = target(&ready)?;
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
    crate::tray::open_dashboard(&app);
    conceal(&app);
    Ok(row.id)
}

#[tauri::command]
pub fn spotlight_open_view(app: AppHandle, view: String) {
    let _ = app.emit_to(MAIN, "open-view", view);
    crate::tray::open_dashboard(&app);
    dismiss(&app);
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
    // A model that says nothing — filtered, or all reasoning — is as unusable
    // as one that errored, so it reads the same way.
    let mut spoke = false;
    while let Some(event) = events.next().await {
        match event {
            Ok(ChatEvent::TextDelta(delta)) => {
                spoke = spoke || !delta.trim().is_empty();
                lock(&shared.answer).push_str(&delta);
                emit(&app, request_id, Body::Delta { text: delta });
            }
            Ok(ChatEvent::Done { usage }) => {
                *lock(&shared.usage) = usage;
                shared.finished.store(true, Ordering::Release);
                emit(&app, request_id, finished(spoke, usage));
                return;
            }
            Err(ChatError::Cancelled) => return,
            Err(err) => {
                emit(&app, request_id, unavailable(describe(&err)));
                return;
            }
        }
    }
    shared.finished.store(true, Ordering::Release);
    emit(&app, request_id, finished(spoke, None));
}

/// The end of a stream: an answer, or the report that there wasn't one.
fn finished(spoke: bool, usage: Option<Usage>) -> Body {
    if spoke {
        Body::Done {
            usage,
            interrupted: false,
        }
    } else {
        unavailable("the model streamed no text")
    }
}

fn unavailable(detail: impl Into<String>) -> Body {
    Body::Error {
        message: UNAVAILABLE.to_string(),
        detail: Some(detail.into()),
    }
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

/// What the spotlight footer renders: the current target, whether its key is
/// missing, and everything pickable.
#[derive(serde::Serialize)]
pub struct SpotTarget {
    provider: String,
    model: String,
    needs_key: bool,
    providers: Vec<SpotProvider>,
}

#[derive(serde::Serialize)]
pub struct SpotProvider {
    name: String,
    kind: &'static str,
    models: Vec<String>,
}

#[tauri::command]
pub async fn spotlight_target(state: State<'_, AppState>) -> Result<SpotTarget, String> {
    let (provider, model, needs_key, configured) = {
        let ready = state.ready()?;
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
            .unwrap_or_default();
        // A key problem is an intake prompt, not an error: the ask field turns
        // into the place the key is pasted — asklight's one-time setup.
        let needs_key = matches!(
            ready.registry.provider(&provider),
            Err(ConfigError::MissingApiKey { .. } | ConfigError::BadKeyEnvName { .. })
        );
        let configured: Vec<(String, ProviderConfig)> = ready
            .config
            .providers
            .iter()
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect();
        (provider, model, needs_key, configured)
    };
    let mut providers = Vec::with_capacity(configured.len());
    for (name, config) in configured {
        let kind = config.kind();
        let models = match &config {
            // What the endpoint itself lists, not just the one model the file
            // happens to name.
            ProviderConfig::OpenAiCompat {
                base_url,
                default_model,
                ..
            } => crate::commands::served(
                base_url,
                config.api_key(&name).ok().flatten(),
                default_model.as_deref(),
            )
            .await
            .1
            .into_iter()
            .map(|model| model.name)
            .collect(),
            // The models Ollama actually has installed, not a guess.
            ProviderConfig::Ollama {
                base_url,
                keep_alive,
            } => crate::commands::installed(base_url, keep_alive.clone())
                .await
                .1
                .into_iter()
                .map(|model| model.name)
                .collect(),
        };
        providers.push(SpotProvider { name, kind, models });
    }
    Ok(SpotTarget {
        provider,
        model,
        needs_key,
        providers,
    })
}

/// The pick lands in `[spotlight]` in the file, so the CLI, the next launch
/// and this panel all agree on what spotlight talks to.
#[tauri::command]
pub async fn spotlight_set_target(
    state: State<'_, AppState>,
    provider: String,
    model: String,
) -> Result<(), String> {
    let path = config_path().map_err(|err| err.to_string())?;
    config_edit::set(&path, "spotlight.provider", &provider).map_err(|err| err.to_string())?;
    if !model.trim().is_empty() {
        config_edit::set(&path, "spotlight.model", model.trim()).map_err(|err| err.to_string())?;
    }
    state.reload()
}

/// asklight's one-time setup with odyn's storage: the key pasted into the ask
/// field becomes the target provider's `api_key` in `odyn.toml`.
#[tauri::command]
pub async fn spotlight_save_key(state: State<'_, AppState>, key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("nothing pasted".to_string());
    }
    let (name, config) = {
        let ready = state.ready()?;
        let name = ready
            .config
            .spotlight
            .provider
            .clone()
            .unwrap_or_else(|| ready.registry.default_provider_name().to_string());
        let config = ready
            .config
            .providers
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("no provider named `{name}` is configured"))?;
        (name, config)
    };
    let ProviderConfig::OpenAiCompat {
        base_url,
        api_key_env,
        default_model,
        ..
    } = config
    else {
        return Err("ollama needs no key".to_string());
    };
    let provider = ProviderConfig::OpenAiCompat {
        base_url,
        api_key: Some(key),
        api_key_env,
        default_model,
    };
    let path = config_path().map_err(|err| err.to_string())?;
    config_edit::upsert_provider(&path, &name, &provider).map_err(|err| err.to_string())?;
    state.reload()
}
