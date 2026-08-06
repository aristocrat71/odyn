mod brain;
mod commands;
mod config;
mod spotlight;
mod state;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build());
    // The spotlight panel type lives behind this plugin; macOS only.
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());
    let app = builder
        .setup(|app| {
            app.manage(state::AppState::load());
            spotlight::setup(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_conversations,
            commands::create_conversation,
            commands::rename_conversation,
            commands::delete_conversation,
            commands::set_conversation_model,
            commands::set_conversation_brevity,
            commands::get_conversation,
            commands::messages,
            commands::send_message,
            commands::context_preview,
            brain::brain_overview,
            brain::brain_episodic,
            brain::brain_search,
            brain::brain_add_core,
            brain::brain_update_core,
            brain::brain_delete_memory,
            brain::brain_graph,
            config::config_file,
            config::open_config,
            config::providers_config,
            config::provider_save,
            config::provider_remove,
            config::set_default_provider,
            config::reload_config,
            commands::cancel_message,
            commands::status,
            commands::providers_overview,
            spotlight::spotlight_hide,
            spotlight::spotlight_toggle,
            spotlight::spotlight_status,
            spotlight::spotlight_ask,
            spotlight::spotlight_promote,
            spotlight::spotlight_target,
            spotlight::spotlight_set_target,
            spotlight::spotlight_save_key,
        ])
        .run(tauri::generate_context!());
    if let Err(err) = app {
        eprintln!("odyn: could not start: {err}");
    }
}
