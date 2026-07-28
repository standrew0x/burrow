pub mod color;
pub mod commands;
pub mod db;
pub mod error;
pub mod image_ops;
pub mod ingest;
pub mod store;

use commands::AppState;
use store::Library;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let root = Library::default_root().expect("no local app data directory");
    let library = Library::open(&root).expect("could not open library");
    let state = AppState::new(library).expect("could not open database");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::import_paths,
            commands::list_assets,
            commands::search_by_color,
            commands::library_root,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
