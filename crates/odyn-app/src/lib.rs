mod brain;
mod commands;
mod config;
mod spotlight;
mod state;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
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
            commands::cancel_message,
            commands::status,
            commands::providers_overview,
            spotlight::spotlight_hide,
            spotlight::spotlight_toggle,
            spotlight::spotlight_status,
            spotlight::spotlight_ask,
            spotlight::spotlight_promote,
        ])
        .run(tauri::generate_context!());
    if let Err(err) = app {
        eprintln!("odyn: could not start: {err}");
    }
}
