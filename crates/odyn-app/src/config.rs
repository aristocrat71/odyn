//! Config commands: the file as it is on disk, the way out to an editor, and
//! the providers view's writes — which go through `config_edit`, so the file
//! stays hand-editable and the two never disagree.

use odyn_core::config::{config_path, read_config_file, Config, ConfigError, ProviderConfig};
use odyn_core::config_edit;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::state::AppState;

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

/// One provider as the providers view shows it. The key itself never crosses
/// to the frontend — only whether one is there.
#[derive(serde::Serialize)]
pub struct ProviderEntry {
    name: String,
    kind: &'static str,
    base_url: String,
    default: bool,
    default_model: Option<String>,
    keep_alive: Option<String>,
    /// A literal key sits in the file.
    key_stored: bool,
    key_env: Option<String>,
    /// Whether the named variable is set and non-empty right now.
    key_env_set: bool,
}

/// What the form submits. Blank strings mean absence — except `api_key`,
/// whose blank means "unchanged": the form never shows the stored key, so
/// blank is all an untouched field can send.
#[derive(serde::Deserialize)]
pub struct ProviderDraft {
    name: String,
    kind: String,
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    default_model: Option<String>,
    keep_alive: Option<String>,
    make_default: bool,
}

#[tauri::command]
pub async fn providers_config(state: State<'_, AppState>) -> Result<Vec<ProviderEntry>, String> {
    let ready = state.ready()?;
    Ok(entries(&ready.config))
}

#[tauri::command]
pub async fn provider_save(
    state: State<'_, AppState>,
    draft: ProviderDraft,
) -> Result<Vec<ProviderEntry>, String> {
    let path = config_path().map_err(say)?;
    let provider = resolve(&draft)?;
    config_edit::upsert_provider(&path, &draft.name, &provider).map_err(say)?;
    if draft.make_default {
        config_edit::set(&path, "default_provider", &draft.name).map_err(say)?;
    }
    reloaded(&state)
}

#[tauri::command]
pub async fn provider_remove(
    state: State<'_, AppState>,
    name: String,
) -> Result<Vec<ProviderEntry>, String> {
    let path = config_path().map_err(say)?;
    config_edit::remove_provider(&path, &name).map_err(say)?;
    reloaded(&state)
}

#[tauri::command]
pub async fn set_default_provider(
    state: State<'_, AppState>,
    name: String,
) -> Result<Vec<ProviderEntry>, String> {
    let path = config_path().map_err(say)?;
    config_edit::set(&path, "default_provider", &name).map_err(say)?;
    reloaded(&state)
}

/// For edits made outside the app: reread the file, reopen the database.
#[tauri::command]
pub async fn reload_config(state: State<'_, AppState>) -> Result<(), String> {
    state.reload()
}

fn resolve(draft: &ProviderDraft) -> Result<ProviderConfig, String> {
    let base_url = draft.base_url.trim().to_string();
    match draft.kind.as_str() {
        "openai_compat" => {
            let api_key_env = clean(&draft.api_key_env);
            let mut api_key = clean(&draft.api_key);
            if api_key.is_none() && api_key_env.is_none() {
                api_key = stored_key(&draft.name);
            }
            Ok(ProviderConfig::OpenAiCompat {
                base_url,
                api_key,
                api_key_env,
                default_model: clean(&draft.default_model),
            })
        }
        "ollama" => Ok(ProviderConfig::Ollama {
            base_url,
            keep_alive: clean(&draft.keep_alive),
        }),
        other => Err(format!("unknown provider kind `{other}`")),
    }
}

/// Blank form fields are absence, not empty values.
fn clean(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The key already in the file for `name`, read fresh: an edit that leaves
/// the key field blank keeps the key the file has.
fn stored_key(name: &str) -> Option<String> {
    let mut config = Config::load().ok()?;
    match config.providers.remove(name)? {
        ProviderConfig::OpenAiCompat { api_key, .. } => api_key,
        ProviderConfig::Ollama { .. } => None,
    }
}

fn entries(config: &Config) -> Vec<ProviderEntry> {
    config
        .providers
        .iter()
        .map(|(name, provider)| {
            let (default_model, keep_alive, key_stored, key_env) = match provider {
                ProviderConfig::OpenAiCompat {
                    api_key,
                    api_key_env,
                    default_model,
                    ..
                } => (
                    default_model.clone(),
                    None,
                    api_key.as_deref().is_some_and(|key| !key.trim().is_empty()),
                    api_key_env.clone(),
                ),
                ProviderConfig::Ollama { keep_alive, .. } => {
                    (None, keep_alive.clone(), false, None)
                }
            };
            let key_env_set = key_env.as_deref().is_some_and(|var| {
                std::env::var(var)
                    .ok()
                    .is_some_and(|value| !value.trim().is_empty())
            });
            ProviderEntry {
                name: name.clone(),
                kind: provider.kind(),
                base_url: provider.base_url().to_string(),
                default: *name == config.default_provider,
                default_model,
                keep_alive,
                key_stored,
                key_env,
                key_env_set,
            }
        })
        .collect()
}

/// Every write answers with the list as the reloaded state sees it, so the
/// view never renders a file the running app has not adopted.
fn reloaded(state: &AppState) -> Result<Vec<ProviderEntry>, String> {
    state.reload()?;
    let ready = state.ready()?;
    Ok(entries(&ready.config))
}

fn say(err: ConfigError) -> String {
    err.to_string()
}
