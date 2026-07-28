//! Tauri command surface.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ingest::{self, AssetRow, ColorMatch, ImportReport};
use crate::store::Library;

/// Default OkLab radius for colour search. OkLab L spans 0..1, so 0.12 is a
/// "same colour family" band rather than "same exact shade".
const DEFAULT_COLOR_TOLERANCE: f32 = 0.12;
const DEFAULT_PAGE_SIZE: i64 = 200;

pub struct AppState {
    pub library: Library,
    /// Single writer connection behind a mutex. SQLite in WAL mode allows
    /// concurrent readers, but every command here goes through one handle;
    /// splitting reads onto a pool is a later optimisation, not a correctness
    /// requirement.
    pub conn: Mutex<Connection>,
}

impl AppState {
    pub fn new(library: Library) -> Result<Self> {
        let conn = crate::db::open(&library.db_path())?;
        Ok(Self {
            library,
            conn: Mutex::new(conn),
        })
    }
}

#[tauri::command]
pub fn import_paths(state: tauri::State<'_, AppState>, paths: Vec<String>) -> Result<ImportReport> {
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::import_paths(&state.library, &mut conn, &paths)
}

#[tauri::command]
pub fn list_assets(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<AssetRow>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::list_assets(
        &conn,
        limit.unwrap_or(DEFAULT_PAGE_SIZE),
        offset.unwrap_or(0),
    )
}

#[tauri::command]
pub fn search_by_color(
    state: tauri::State<'_, AppState>,
    hex: String,
    tolerance: Option<f32>,
    limit: Option<usize>,
) -> Result<Vec<ColorMatch>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::search_by_color(
        &conn,
        &hex,
        tolerance.unwrap_or(DEFAULT_COLOR_TOLERANCE),
        limit.unwrap_or(DEFAULT_PAGE_SIZE as usize),
    )
}

/// Absolute path to the thumbnail for `hash`. The frontend turns this into a
/// loadable URL with `convertFileSrc`, which needs the asset protocol enabled
/// and this directory in its scope.
#[tauri::command]
pub fn thumb_path(state: tauri::State<'_, AppState>, hash: String) -> String {
    state.library.thumb_path(&hash).display().to_string()
}

#[tauri::command]
pub fn library_root(state: tauri::State<'_, AppState>) -> String {
    state.library.root().display().to_string()
}
