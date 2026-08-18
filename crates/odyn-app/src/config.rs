//! Config commands. The providers view's writes go through `config_edit`, so
//! the file stays hand-editable and the two never disagree.

use odyn_core::catalog;
use odyn_core::chat::ChatError;
use odyn_core::config::{config_path, read_config_file, Config, ConfigError, ProviderConfig};
use odyn_core::config_edit;
use odyn_core::providers::openai_compat::OpenAiCompatProvider;
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

/// Opened from Rust with the backend's own resolved path, never the frontend's.
#[tauri::command]
pub fn open_config(app: AppHandle) -> Result<(), String> {
    let path = config_path().map_err(|err| err.to_string())?;
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|err| err.to_string())
}

/// The key itself never crosses to the frontend — only whether one is there.
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

/// What the form submits. Blank means absence — except `api_key`, whose blank
/// means "unchanged": the form never shows the stored key.
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

#[derive(serde::Serialize)]
pub struct CatalogItem {
    id: &'static str,
    label: &'static str,
    kind: &'static str,
    base_url: &'static str,
    needs_key: bool,
    keys_url: &'static str,
    /// So a pasted key can name its own provider without a round trip.
    key_prefixes: &'static [&'static str],
    /// Already in `odyn.toml`: the tile says so instead of offering it twice.
    configured: bool,
}

/// What a connection came to; `note` says why the model list is empty.
#[derive(serde::Serialize)]
pub struct Connected {
    name: String,
    model: Option<String>,
    models: usize,
    note: Option<String>,
    providers: Vec<ProviderEntry>,
}

#[tauri::command]
pub async fn provider_catalog(state: State<'_, AppState>) -> Result<Vec<CatalogItem>, String> {
    let names: Vec<String> = {
        let ready = state.ready()?;
        ready.config.providers.keys().cloned().collect()
    };
    Ok(catalog::PROVIDERS
        .iter()
        .map(|provider| CatalogItem {
            id: provider.id,
            label: provider.label,
            kind: provider.kind,
            base_url: provider.base_url,
            needs_key: provider.needs_key(),
            keys_url: provider.keys_url,
            key_prefixes: provider.key_prefixes,
            configured: names.iter().any(|name| name == provider.id),
        })
        .collect())
}

/// A catalog entry plus a key is a whole provider. A key the endpoint rejects is
/// the one refusal — nothing is written; every other failure still connects.
#[tauri::command]
pub async fn provider_connect(
    state: State<'_, AppState>,
    id: String,
    api_key: String,
    make_default: bool,
) -> Result<Connected, String> {
    let entry = catalog::find(&id).ok_or_else(|| format!("no provider named `{id}` is known"))?;
    let key = api_key.trim().to_string();
    if entry.needs_key() && key.is_empty() {
        return Err(format!("{} needs an api key", entry.label));
    }

    let (models, note) = if entry.needs_key() {
        probe(entry, &key).await?
    } else {
        (Vec::new(), None)
    };
    // Read fresh: the file may have been edited since load.
    let existing = Config::load()
        .ok()
        .and_then(|mut config| config.providers.remove(entry.id));
    let provider = catalog::connected(entry, &key, &models, existing.as_ref());
    let model = provider.default_model().map(str::to_string);

    let path = config_path().map_err(say)?;
    config_edit::upsert_provider(&path, entry.id, &provider).map_err(say)?;
    if make_default {
        config_edit::set(&path, "default_provider", entry.id).map_err(say)?;
    }
    Ok(Connected {
        name: entry.id.to_string(),
        model,
        models: models.len(),
        note,
        providers: reloaded(&state)?,
    })
}

/// Asks the endpoint what it serves. `Ok` carries the model names and, when the
/// listing failed for reasons other than the key, why. `Err` is a refused key.
async fn probe(
    entry: &catalog::Provider,
    key: &str,
) -> Result<(Vec<String>, Option<String>), String> {
    let built = OpenAiCompatProvider::new(entry.base_url, Some(key.to_string()), Vec::new());
    let provider = match built {
        Ok(provider) => provider,
        // A stray byte in the key: the header cannot carry it, so no request ran.
        Err(err) => return Err(format!("{}: {err}", entry.label)),
    };
    match provider.list_models().await {
        Ok(models) if models.is_empty() => {
            Ok((models, Some(format!("{} listed no models", entry.label))))
        }
        Ok(models) => Ok((models, None)),
        Err(ChatError::Api { status, message }) if status == 401 || status == 403 => {
            Err(format!("{} rejected that key: {message}", entry.label))
        }
        Err(ChatError::Api { status, .. }) => Ok((
            Vec::new(),
            Some(format!(
                "connected — {} answered {status} to a model listing, so pick a model yourself",
                entry.label
            )),
        )),
        Err(err) => Ok((
            Vec::new(),
            Some(format!("key saved — {} did not answer: {err}", entry.label)),
        )),
    }
}

/// A link the model wrote, leaving for the default browser. Only http(s) is
/// ever opened: anything else could name a local scheme handler.
#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    let lowered = url.trim().to_ascii_lowercase();
    if !lowered.starts_with("http://") && !lowered.starts_with("https://") {
        return Err("only http(s) links open".to_string());
    }
    app.opener()
        .open_url(url.trim(), None::<&str>)
        .map_err(|err| err.to_string())
}

/// The url is a build constant, so the frontend cannot redirect the browser.
#[tauri::command]
pub fn open_keys_page(app: AppHandle, id: String) -> Result<(), String> {
    let entry = catalog::find(&id).ok_or_else(|| format!("no provider named `{id}` is known"))?;
    app.opener()
        .open_url(entry.keys_url, None::<&str>)
        .map_err(|err| err.to_string())
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

/// The key already in the file for `name`: a blank key field keeps it.
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

/// Every write answers from the reloaded state the app has actually adopted.
fn reloaded(state: &AppState) -> Result<Vec<ProviderEntry>, String> {
    state.reload()?;
    let ready = state.ready()?;
    Ok(entries(&ready.config))
}

fn say(err: ConfigError) -> String {
    err.to_string()
}
