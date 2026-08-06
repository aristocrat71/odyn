//! Config view commands: the file as it is on disk, and the way out to an
//! editor. DESIGN.md §8 — read-only in v1, so nothing here writes.

use odyn_core::config::{config_path, read_config_file};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[derive(serde::Serialize)]
pub struct ConfigFile {
    path: String,
    text: String,
}

#[tauri::command]
pub fn config_file() -> Result<ConfigFile, String> {
    let (path, text) = read_config_file().map_err(|err| err.to_string())?;
    Ok(ConfigFile {
        path: path.display().to_string(),
        text,
    })
}

/// Opened from Rust: the path is the one the backend resolved, so it never has
/// to be handed to the frontend's opener scope to be trusted.
#[tauri::command]
pub fn open_config(app: AppHandle) -> Result<(), String> {
    let path = config_path().map_err(|err| err.to_string())?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|err| err.to_string())
}
