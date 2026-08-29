pub mod commands;

use commands::{
    cancel_backup, default_settings, exit_app, get_app, get_apps, get_history, get_settings,
    pick_directory, reload_settings, remove_app, resolve_path, run_backup, save_app, save_settings,
    AppState,
};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            get_apps,
            get_settings,
            get_history,
            get_app,
            save_app,
            remove_app,
            resolve_path,
            run_backup,
            cancel_backup,
            save_settings,
            default_settings,
            reload_settings,
            pick_directory,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
