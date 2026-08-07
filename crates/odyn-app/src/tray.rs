//! Menu bar tray. Closing the dashboard leaves odyn running — the spotlight
//! hotkey has to keep answering — so the tray is both the way back to the
//! window and the only way all the way out.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

const MAIN: &str = "main";
const OPEN: &str = "open";
const QUIT: &str = "quit";

/// A tray that will not build is not worth failing startup over: the window is
/// already up and the hotkey still works, so the reason is printed and odyn
/// carries on — but the close button then has to keep quitting, or the app
/// would live on with no way to reach it.
pub fn setup(app: &AppHandle) {
    match build(app) {
        Ok(()) => hide_on_close(app),
        Err(err) => eprintln!("odyn: no tray icon: {err}"),
    }
}

fn build(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, OPEN, "Open Odyn", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::new()
        .tooltip("odyn")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            OPEN => open_dashboard(app),
            // The one total exit: `exit` runs Tauri's cleanup, unlike the
            // predefined quit item, which hands straight to the platform.
            QUIT => app.exit(0),
            _ => {}
        });
    // The app icon itself. `icon_as_template` is left off so macOS draws it in
    // colour instead of flattening it into a monochrome glyph.
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    // The handle is dropped here on purpose: the app holds the tray in its
    // resource table, so the icon outlives this binding.
    tray.build(app)?;
    Ok(())
}

/// The dashboard is only ever hidden, never destroyed, so opening it is a show
/// away — and re-showing keeps whatever conversation was on screen.
pub fn open_dashboard(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// The close button puts odyn in the tray rather than ending it; Quit ends it.
fn hide_on_close(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    let main = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = main.hide();
        }
    });
}
