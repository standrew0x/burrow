pub mod boards;
pub mod color;
pub mod commands;
pub mod db;
pub mod error;
pub mod image_ops;
pub mod ingest;
pub mod link;
pub mod store;
pub mod video;
pub mod xsync;

use commands::AppState;
use store::Library;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let root = Library::default_root().expect("no local app data directory");
    let library = Library::open(&root).expect("could not open library");
    let state = AppState::new(library).expect("could not open database");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Prefer the ffmpeg shipped in the bundle so video import works on
            // a machine that has never heard of ffmpeg. Falls back to PATH,
            // which is what `tauri dev` and the CLI examples use.
            use tauri::Manager;
            if let Ok(dir) = app
                .path()
                .resolve("ffmpeg", tauri::path::BaseDirectory::Resource)
            {
                crate::video::use_bundled_dir(dir);
            }
            Ok(())
        })
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::import_paths,
            commands::add_links,
            commands::download_assets,
            commands::list_assets,
            commands::search_by_color,
            commands::library_root,
            commands::list_boards,
            commands::create_board,
            commands::rename_board,
            commands::delete_board,
            commands::add_to_board,
            commands::remove_from_board,
            commands::list_board_assets,
            commands::move_to_board,
            commands::delete_assets,
            commands::set_note,
            commands::search_notes,
            commands::sync_from_x,
            commands::x_folders,
            commands::x_status,
            commands::save_x_session,
            commands::clear_x_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
