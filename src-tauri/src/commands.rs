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

#[tauri::command]
pub fn move_to_board(
    state: tauri::State<'_, AppState>,
    from_board: i64,
    to_board: i64,
    asset_ids: Vec<i64>,
) -> Result<usize> {
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    crate::boards::move_to_board(&mut conn, from_board, to_board, &asset_ids)
}

/// Permanently deletes references and their stored files.
#[tauri::command]
pub fn delete_assets(
    state: tauri::State<'_, AppState>,
    asset_ids: Vec<i64>,
) -> Result<crate::ingest::DeleteReport> {
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::delete_assets(&state.library, &mut conn, &asset_ids)
}

// --- X bookmark sync ---

const DEFAULT_SYNC_LIMIT: usize = 50;

#[derive(Debug, Clone, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    /// What was synced: "all bookmarks" or a folder name.
    pub source: String,
    /// Media items the window offered, before download.
    pub found: usize,
    pub downloaded: usize,
    pub imported: usize,
    pub duplicates: usize,
    pub images: usize,
    pub videos: usize,
    pub failed: Vec<crate::ingest::FailedImport>,
}

/// Pulls images and videos from X bookmarks into the library.
///
/// `folder` selects a single bookmark folder; omit it for every bookmark.
/// `from`/`to` are inclusive `YYYY-MM-DD` bounds.
///
/// Network work happens off the database mutex; the lock is taken only for the
/// final ingest, so browsing stays responsive while media downloads.
#[tauri::command]
pub async fn sync_from_x(
    state: tauri::State<'_, AppState>,
    limit: Option<usize>,
    folder: Option<String>,
    from: Option<String>,
    to: Option<String>,
    // `kinds` is "all" (default), "images", or "videos".
    kinds: Option<String>,
) -> Result<SyncReport> {
    let library_root = state.library.root().to_path_buf();
    let kinds = kinds.unwrap_or_else(|| "all".to_string());
    let opts = crate::xsync::FetchOptions {
        limit: limit.unwrap_or(DEFAULT_SYNC_LIMIT),
        from: from.filter(|s| !s.is_empty()),
        to: to.filter(|s| !s.is_empty()),
        include_images: kinds != "videos",
        include_videos: kinds != "images",
    };
    let folder = folder.filter(|s| !s.is_empty());

    // Blocking HTTP on the async runtime would stall every other command.
    let fetched = tauri::async_runtime::spawn_blocking(move || -> Result<_> {
        let lib = crate::store::Library::open(&library_root)?;
        let session = crate::xsync::XSession::load(&lib)?;
        let client = crate::xsync::XClient::new(session)?;

        // Discover only what this run needs: the folder-list call is a wasted
        // round trip when syncing everything.
        let (source, spec, label) = match &folder {
            Some(name) => {
                let specs = client.discover(&["BookmarkFoldersSlice", "BookmarkFolderTimeline"])?;
                let folders = client.folders(&specs[0])?;
                let id = folders
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, id)| id.clone())
                    .ok_or_else(|| {
                        let names: Vec<&str> = folders.iter().map(|(n, _)| n.as_str()).collect();
                        Error::X(format!(
                            "bookmark folder {name:?} not found. Available: {names:?}"
                        ))
                    })?;
                (
                    crate::xsync::BookmarkSource::Folder(id),
                    specs[1].clone(),
                    name.clone(),
                )
            }
            None => {
                let specs = client.discover(&["Bookmarks"])?;
                (
                    crate::xsync::BookmarkSource::All,
                    specs[0].clone(),
                    "all bookmarks".to_string(),
                )
            }
        };

        let items = client.fetch_bookmarks(&spec, &source, &opts)?;

        // Staged outside the library so a failed run leaves no half-imported
        // blobs behind; ingest copies what it accepts.
        let staging = std::env::temp_dir().join(format!("burrow-xsync-{}", std::process::id()));
        std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;

        let mut downloaded = Vec::new();
        let mut failed = Vec::new();
        for item in &items {
            match client.download(item, &staging) {
                Ok(p) => downloaded.push((p, item.tweet_url.clone(), item.kind)),
                Err(e) => failed.push(crate::ingest::FailedImport {
                    path: item.tweet_url.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok((label, items.len(), downloaded, failed, staging))
    })
    .await
    .map_err(|e| Error::X(format!("sync task panicked: {e}")))??;

    let (label, found, downloaded, mut failed, staging) = fetched;

    let files: Vec<std::path::PathBuf> = downloaded.iter().map(|(p, _, _)| p.clone()).collect();
    let mut report = {
        let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        ingest::import_paths(&state.library, &mut conn, &files)?
    };

    // Record where each one came from, so a tile can lead back to the post.
    {
        let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        let mut stmt = conn.prepare("UPDATE assets SET source_url = ?1 WHERE id = ?2")?;
        for asset in &report.imported {
            let name = asset.original_name.clone().unwrap_or_default();
            if let Some((_, url, _)) = downloaded.iter().find(|(p, _, _)| {
                p.file_name().map(|f| f.to_string_lossy() == name.as_str()) == Some(true)
            }) {
                stmt.execute(rusqlite::params![url, asset.id])?;
            }
        }
    }

    // Staging is pure scratch once ingest has copied what it wants.
    let _ = std::fs::remove_dir_all(&staging);

    let images = report
        .imported
        .iter()
        .filter(|a| a.kind == crate::ingest::MediaKind::Image)
        .count();
    let videos = report.imported.len() - images;

    failed.append(&mut report.failed);
    Ok(SyncReport {
        source: label,
        found,
        downloaded: files.len(),
        imported: report.imported.len(),
        duplicates: report.duplicates,
        images,
        videos,
        failed,
    })
}

/// Bookmark folder names, so the UI can offer them instead of hardcoding one.
#[tauri::command]
pub async fn x_folders(state: tauri::State<'_, AppState>) -> Result<Vec<String>> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<String>> {
        let lib = crate::store::Library::open(&library_root)?;
        let client = crate::xsync::XClient::new(crate::xsync::XSession::load(&lib)?)?;
        let specs = client.discover(&["BookmarkFoldersSlice"])?;
        Ok(client
            .folders(&specs[0])?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    })
    .await
    .map_err(|e| Error::X(format!("folder lookup panicked: {e}")))?
}
