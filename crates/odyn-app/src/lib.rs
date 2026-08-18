mod brain;
mod commands;
mod config;
mod reminders;
mod spotlight;
mod state;
mod tray;
mod update;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build());
    // The spotlight panel type lives behind this plugin; macOS only.
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());
    let app = builder
        .setup(|app| {
            app.manage(state::AppState::load());
            spotlight::setup(app.handle());
            tray::setup(app.handle());
            reminders::setup(app.handle());
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
            brain::brain_memories,
            brain::brain_search,
            brain::brain_add_note,
            brain::brain_update_note,
            brain::brain_delete_note,
            brain::brain_graph,
            brain::embed_catalog,
            brain::brain_set_model,
            brain::brain_set_save_temperature,
            brain::brain_set_top_k,
            brain::brain_set_min_relevance,
            config::config_file,
            config::open_config,
            config::open_url,
            config::providers_config,
            config::provider_catalog,
            config::provider_connect,
            config::open_keys_page,
            config::provider_save,
            config::provider_remove,
            config::set_default_provider,
            config::reload_config,
            reminders::reminders_list,
            reminders::reminder_delete,
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
            spotlight::spotlight_open_view,
        ])
        .build(tauri::generate_context!());
    match app {
        Ok(app) => app.run(|_app, _event| {
            #[cfg(target_os = "macos")]
            if matches!(_event, tauri::RunEvent::Reopen { .. }) {
                tray::open_dashboard(_app);
            }
        }),
        Err(err) => eprintln!("odyn: could not start: {err}"),
    }
}
