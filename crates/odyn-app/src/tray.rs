use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

const MAIN: &str = "main";
const OPEN: &str = "open";
const QUIT: &str = "quit";

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
            QUIT => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    tray.build(app)?;
    Ok(())
}

pub fn open_dashboard(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    dock(app, true);
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

fn hide_on_close(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    let main = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = main.hide();
            dock(main.app_handle(), false);
        }
    });
}

fn dock(app: &AppHandle, visible: bool) {
    #[cfg(target_os = "macos")]
    let _ = app.set_dock_visibility(visible);
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
}
