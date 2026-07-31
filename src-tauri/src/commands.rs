//! Tauri command surface.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ingest::{self, AssetRow, ColorMatch, ImportReport};
use crate::store::Library;

/// Default OkLab radius for colour search.
///
/// Tuned against a 24-image library of architectural photos, which is close to
/// a worst case: everything shares sky and glass, so the palettes overlap
/// heavily. Match counts at each radius were 5 / 10 / 19 / 22 for 0.03 / 0.05 /
/// 0.08 / 0.12. Past ~0.08 the filter hands back most of the library and stops
/// being a filter. 0.05 errs tight on purpose -- too few results is a visible,
/// recoverable state (widen the search), while too many just looks broken.
const DEFAULT_COLOR_TOLERANCE: f32 = 0.05;
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
        &state.library,
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
        &state.library,
        &conn,
        &hex,
        tolerance.unwrap_or(DEFAULT_COLOR_TOLERANCE),
        limit.unwrap_or(DEFAULT_PAGE_SIZE as usize),
    )
}

#[tauri::command]
pub fn library_root(state: tauri::State<'_, AppState>) -> String {
    state.library.root().display().to_string()
}

// --- boards ---

#[tauri::command]
pub fn list_boards(state: tauri::State<'_, AppState>) -> Result<Vec<crate::boards::Board>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::list_boards(&state.library, &conn)
}

#[tauri::command]
pub fn create_board(
    state: tauri::State<'_, AppState>,
    name: String,
) -> Result<crate::boards::Board> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::create_board(&conn, &name)
}

#[tauri::command]
pub fn rename_board(
    state: tauri::State<'_, AppState>,
    id: i64,
    name: String,
) -> Result<crate::boards::Board> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::rename_board(&conn, id, &name)
}

#[tauri::command]
pub fn delete_board(state: tauri::State<'_, AppState>, id: i64) -> Result<()> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::delete_board(&conn, id)
}

#[tauri::command]
pub fn add_to_board(
    state: tauri::State<'_, AppState>,
    board_id: i64,
    asset_ids: Vec<i64>,
) -> Result<usize> {
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::add_to_board(&mut conn, board_id, &asset_ids)
}

#[tauri::command]
pub fn remove_from_board(
    state: tauri::State<'_, AppState>,
    board_id: i64,
    asset_ids: Vec<i64>,
) -> Result<usize> {
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::remove_from_board(&mut conn, board_id, &asset_ids)
}

#[tauri::command]
pub fn list_board_assets(
    state: tauri::State<'_, AppState>,
    board_id: i64,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<AssetRow>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::list_board_assets(
        &state.library,
        &conn,
        board_id,
        limit.unwrap_or(DEFAULT_PAGE_SIZE),
        offset.unwrap_or(0),
    )
}
