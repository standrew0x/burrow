//! Tauri command surface.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::ingest::{self, AssetRow, AssetState, ColorMatch, ImportReport, MediaKind};
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

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSnapshot {
    pub asset: AssetRow,
    pub duplicate: bool,
    pub captured_at_ms: i64,
}

fn safe_snapshot_stem(original_name: Option<&str>) -> String {
    let stem = original_name
        .and_then(|name| {
            PathBuf::from(name)
                .file_stem()
                .map(|value| value.to_owned())
        })
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".to_string());
    let cleaned: String = stem
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .take(80)
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.');
    if cleaned.is_empty() {
        "video".to_string()
    } else {
        cleaned.to_string()
    }
}

fn snapshot_name(original_name: Option<&str>, position_ms: i64) -> String {
    let total_ms = position_ms.max(0);
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms / 60_000) % 60;
    let seconds = (total_ms / 1_000) % 60;
    let millis = total_ms % 1_000;
    format!(
        "{} - {:02}-{:02}-{:02}.{:03}.png",
        safe_snapshot_stem(original_name),
        hours,
        minutes,
        seconds,
        millis
    )
}

fn snapshot_temp_path(name: &str) -> Result<PathBuf> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let directory =
        std::env::temp_dir().join(format!("burrow-snapshot-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&directory).map_err(|error| Error::io(&directory, error))?;
    Ok(directory.join(name))
}

fn finish_video_snapshot(
    library_root: PathBuf,
    source_asset_id: i64,
    board_id: Option<i64>,
    position_ms: i64,
    captured_path: PathBuf,
) -> Result<VideoSnapshot> {
    let cleanup_dir = captured_path.parent().map(PathBuf::from);
    let result = (|| {
        let library = Library::open(library_root)?;
        let digest = crate::store::hash_file(&captured_path)?;
        let mut conn = crate::db::open(&library.db_path())?;
        let report =
            ingest::import_paths(&library, &mut conn, std::slice::from_ref(&captured_path))?;
        if let Some(failure) = report.failed.first() {
            return Err(Error::Media(failure.reason.clone()));
        }

        let asset_id = if let Some(asset) = report.imported.first() {
            asset.id
        } else {
            conn.query_row(
                "SELECT id FROM assets WHERE hash = ?1 OR content_hash = ?1 LIMIT 1",
                [&digest],
                |row| row.get(0),
            )?
        };

        let mut board_ids = {
            let mut statement =
                conn.prepare("SELECT board_id FROM board_items WHERE asset_id = ?1")?;
            let ids = statement
                .query_map([source_asset_id], |row| row.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ids
        };
        if let Some(id) = board_id {
            board_ids.push(id);
        }
        board_ids.sort_unstable();
        board_ids.dedup();
        for id in board_ids {
            crate::boards::add_to_board(&mut conn, id, &[asset_id])?;
        }

        let asset = ingest::asset_by_id(&library, &conn, asset_id)?
            .ok_or_else(|| Error::Media("the captured frame could not be found".to_string()))?;
        Ok(VideoSnapshot {
            asset,
            duplicate: report.duplicates > 0,
            captured_at_ms: position_ms,
        })
    })();

    let _ = std::fs::remove_file(&captured_path);
    if let Some(directory) = cleanup_dir {
        let _ = std::fs::remove_dir(directory);
    }
    result
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
pub fn search_assets(
    state: tauri::State<'_, AppState>,
    query: String,
    tolerance: Option<f32>,
    limit: Option<usize>,
) -> Result<Vec<AssetRow>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::search_assets(
        &state.library,
        &conn,
        &query,
        tolerance.unwrap_or(DEFAULT_COLOR_TOLERANCE),
        limit.unwrap_or(DEFAULT_PAGE_SIZE as usize),
    )
}

#[tauri::command]
pub fn library_root(state: tauri::State<'_, AppState>) -> String {
    state.library.root().display().to_string()
}

/// Explorer-friendly tree containing downloaded X videos.
#[tauri::command]
pub async fn x_downloads_folder(state: tauri::State<'_, AppState>) -> Result<String> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<String> {
        let library = Library::open(library_root)?;
        let conn = crate::db::open(&library.db_path())?;
        crate::x_library::organize_existing(&library, &conn)?;
        Ok(crate::x_library::root(&library).display().to_string())
    })
    .await
    .map_err(|error| Error::Media(format!("X video folder task stopped: {error}")))?
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

// --- notes ---

/// Writes or clears the note on one reference. Returns the stored value, which
/// is `None` when the note was blanked.
#[tauri::command]
pub fn set_note(
    state: tauri::State<'_, AppState>,
    asset_id: i64,
    note: String,
) -> Result<Option<String>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::set_note(&conn, asset_id, &note)
}

/// References whose note contains `query`.
#[tauri::command]
pub fn search_notes(
    state: tauri::State<'_, AppState>,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<AssetRow>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::search_notes(
        &state.library,
        &conn,
        &query,
        limit.unwrap_or(DEFAULT_PAGE_SIZE),
    )
}

/// References that were deleted and are being kept out of future syncs.
#[tauri::command]
pub fn list_dismissed(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
) -> Result<Vec<crate::ingest::Dismissed>> {
    let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::list_dismissed(&conn, limit.unwrap_or(DEFAULT_PAGE_SIZE))
}

/// Lets deleted references be offered again. Empty `urls` clears the whole list.
#[tauri::command]
pub fn undismiss(state: tauri::State<'_, AppState>, urls: Vec<String>) -> Result<usize> {
    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::undismiss(&mut conn, &urls)
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

/// Ceiling on a single sync, whatever the UI asks for.
///
/// "Everything" is a legitimate request, but it still has to terminate: each
/// item costs a poster fetch, so an unbounded run over a large bookmark list is
/// thousands of HTTP requests with no way to tell it has not hung.
const MAX_SYNC_LIMIT: usize = 5000;

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
    /// Skipped because they were deleted from the library before.
    pub dismissed: usize,
    pub images: usize,
    pub videos: usize,
    /// Why the walk ended, in a sentence.
    ///
    /// Present because a count on its own cannot distinguish "that is all your
    /// bookmarks hold" from "there is more, ask for more" -- and mistaking the
    /// second for the first is what makes a working sync feel lossy.
    pub stopped_because: String,
    /// Whether asking for a larger number could return more.
    pub more_available: bool,
    /// Timeline pages read.
    pub pages: usize,
    /// Posts examined, including ones carrying no media at all.
    pub posts_scanned: usize,
    pub failed: Vec<crate::ingest::FailedImport>,
}

/// Pulls images and videos from X bookmarks into the library.
///
/// `folder` selects a single bookmark folder; omit it for every bookmark.
/// `from`/`to` are inclusive `YYYY-MM-DD` bounds.
///
/// `download` decides what "sync" means. Left off, each bookmark becomes a
/// linked reference: its poster image is stored and the video is not. That is
/// the default because the difference is not marginal -- a measured bookmark
/// ran 16KB as a poster against 171MB as a file, so downloading a whole
/// timeline costs gigabytes to get pictures the grid could already show.
/// Anything linked can be downloaded later, one tile or a selection at a time.
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
    download: Option<bool>,
) -> Result<SyncReport> {
    let download = download.unwrap_or(false);
    let library_root = state.library.root().to_path_buf();
    let kinds = kinds.unwrap_or_else(|| "all".to_string());
    let opts = crate::xsync::FetchOptions {
        // 0 is how the UI says "everything"; it still gets a ceiling.
        limit: match limit.unwrap_or(DEFAULT_SYNC_LIMIT) {
            0 => MAX_SYNC_LIMIT,
            n => n.min(MAX_SYNC_LIMIT),
        },
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

        let walk = client.fetch_bookmarks(&spec, &source, &opts)?;
        let items = walk.items;
        let found = items.len();
        let stats = WalkStats {
            stopped_because: walk.stop.explain().to_string(),
            more_available: walk.stop.more_available(),
            pages: walk.pages,
            posts_scanned: walk.posts_scanned,
        };
        let mut failed = Vec::new();

        if !download {
            // Link-only: fetch the poster for each item and nothing else.
            let mut links = Vec::with_capacity(items.len());
            for item in &items {
                match client.fetch_thumbnail(item) {
                    Ok(thumbnail) => links.push(crate::ingest::PendingLink {
                        page_url: item.tweet_url.clone(),
                        media_url: item.media_url.clone(),
                        kind: match item.kind {
                            crate::xsync::BookmarkKind::Video => crate::ingest::MediaKind::Video,
                            crate::xsync::BookmarkKind::Image => crate::ingest::MediaKind::Image,
                        },
                        title: Some(describe_post(item)),
                        posted_at: (!item.date.is_empty()).then(|| item.date.clone()),
                        x_bookmark_sort_index: item.bookmark_sort_index.clone(),
                        video_variants_json: (!item.video_qualities.is_empty())
                            .then(|| serde_json::to_string(&item.video_qualities).ok())
                            .flatten(),
                        thumbnail,
                    }),
                    Err(e) => failed.push(crate::ingest::FailedImport {
                        path: item.tweet_url.clone(),
                        reason: e.to_string(),
                    }),
                }
            }
            return Ok(Fetched::Links {
                label,
                found,
                links,
                failed,
                stats,
            });
        }

        // Staged outside the library so a failed run leaves no half-imported
        // blobs behind; ingest copies what it accepts.
        let staging = std::env::temp_dir().join(format!("burrow-xsync-{}", std::process::id()));
        std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;

        let mut downloaded = Vec::new();
        for item in &items {
            match client.download(item, &staging) {
                Ok(path) => downloaded.push(FromX {
                    path,
                    page_url: item.tweet_url.clone(),
                    media_url: item.media_url.clone(),
                    posted_at: (!item.date.is_empty()).then(|| item.date.clone()),
                    x_bookmark_sort_index: item.bookmark_sort_index.clone(),
                }),
                Err(e) => failed.push(crate::ingest::FailedImport {
                    path: item.tweet_url.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(Fetched::Files {
            label,
            found,
            downloaded,
            failed,
            staging,
            stats,
        })
    })
    .await
    .map_err(|e| Error::X(format!("sync task panicked: {e}")))??;

    match fetched {
        Fetched::Links {
            label,
            found,
            links,
            mut failed,
            stats,
        } => {
            let offered = links.len();
            let mut report = {
                let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
                ingest::import_links(&state.library, &mut conn, links)?
            };
            let images = count_images(&report.imported);
            failed.append(&mut report.failed);
            Ok(SyncReport {
                source: label,
                found,
                // Posters, not media. Naming it "downloaded" would overstate
                // what just landed on disk by three orders of magnitude.
                downloaded: offered,
                imported: report.imported.len(),
                duplicates: report.duplicates,
                dismissed: report.dismissed,
                images,
                videos: report.imported.len() - images,
                stopped_because: stats.stopped_because,
                more_available: stats.more_available,
                pages: stats.pages,
                posts_scanned: stats.posts_scanned,
                failed,
            })
        }
        Fetched::Files {
            label,
            found,
            downloaded,
            mut failed,
            staging,
            stats,
        } => {
            let files: Vec<std::path::PathBuf> =
                downloaded.iter().map(|d| d.path.clone()).collect();
            let mut report = {
                let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
                ingest::import_paths(&state.library, &mut conn, &files)?
            };

            // Record where each one came from, so a tile can lead back to the
            // post -- and so a later link-mode sync recognises it as already
            // held. Without `remote_url` the same bookmark comes back as a
            // second, linked copy of a file that is already on disk.
            {
                let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
                let mut stmt = conn.prepare(
                    "UPDATE assets
                         SET source_url = ?1, remote_url = ?2, posted_at = ?3,
                             x_bookmark_sort_index = ?4
                         WHERE id = ?5",
                )?;
                for asset in &report.imported {
                    let name = asset.original_name.clone().unwrap_or_default();
                    if let Some(d) = downloaded.iter().find(|d| {
                        d.path
                            .file_name()
                            .map(|f| f.to_string_lossy() == name.as_str())
                            == Some(true)
                    }) {
                        stmt.execute(rusqlite::params![
                            d.page_url,
                            d.media_url,
                            d.posted_at,
                            d.x_bookmark_sort_index,
                            asset.id
                        ])?;
                    }
                }
                // Metadata above is what gives the Explorer tree its date and
                // account folders. Existing and newly downloaded X videos are
                // linked into that tree without copying their bytes.
                crate::x_library::organize_existing(&state.library, &conn)?;
            }

            // Staging is pure scratch once ingest has copied what it wants.
            let _ = std::fs::remove_dir_all(&staging);

            let images = count_images(&report.imported);
            failed.append(&mut report.failed);
            Ok(SyncReport {
                source: label,
                found,
                downloaded: files.len(),
                imported: report.imported.len(),
                duplicates: report.duplicates,
                dismissed: report.dismissed,
                images,
                videos: report.imported.len() - images,
                stopped_because: stats.stopped_because,
                more_available: stats.more_available,
                pages: stats.pages,
                posts_scanned: stats.posts_scanned,
                failed,
            })
        }
    }
}

/// One downloaded bookmark and the two URLs that identify it.
struct FromX {
    path: std::path::PathBuf,
    /// The post, for leading a tile back to where it came from.
    page_url: String,
    /// The media file, which is what dedup and tombstones key on.
    media_url: String,
    posted_at: Option<String>,
    x_bookmark_sort_index: Option<String>,
}

/// How far the bookmark walk got, carried through to the report.
struct WalkStats {
    stopped_because: String,
    more_available: bool,
    pages: usize,
    posts_scanned: usize,
}

/// What the blocking half of a sync produced.
enum Fetched {
    Links {
        label: String,
        found: usize,
        links: Vec<crate::ingest::PendingLink>,
        failed: Vec<crate::ingest::FailedImport>,
        stats: WalkStats,
    },
    Files {
        label: String,
        found: usize,
        downloaded: Vec<FromX>,
        failed: Vec<crate::ingest::FailedImport>,
        staging: std::path::PathBuf,
        stats: WalkStats,
    },
}

fn count_images(assets: &[AssetRow]) -> usize {
    assets
        .iter()
        .filter(|a| a.kind == crate::ingest::MediaKind::Image)
        .count()
}

/// A one-line label for a post, used as the reference's name.
fn describe_post(item: &crate::xsync::BookmarkMedia) -> String {
    let text = item.text.replace(['\n', '\r'], " ");
    let text = text.trim();
    if text.is_empty() {
        return format!("@{}", item.author);
    }
    // Truncated on a character boundary; a byte slice would panic on the first
    // emoji, which X posts are not short of.
    let short: String = text.chars().take(70).collect();
    if short.chars().count() < text.chars().count() {
        format!("@{} — {}…", item.author, short.trim_end())
    } else {
        format!("@{} — {short}", item.author)
    }
}

/// Adds references from pasted URLs, fetching only a preview image.
///
/// X post links read the public embed endpoint; everything else is resolved
/// from its OpenGraph tags. Neither needs the X session, so this works before
/// the account is connected -- see [`crate::link`].
#[tauri::command]
pub async fn add_links(
    state: tauri::State<'_, AppState>,
    urls: Vec<String>,
) -> Result<ImportReport> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<ImportReport> {
        let mut links = Vec::new();
        let mut failed = Vec::new();
        for url in urls {
            let url = url.trim().to_string();
            if url.is_empty() {
                continue;
            }
            match crate::link::resolve(&url) {
                Ok(r) => links.push(crate::ingest::PendingLink {
                    page_url: r.page_url,
                    media_url: r.media_url,
                    kind: r.kind,
                    title: r.title,
                    posted_at: None,
                    x_bookmark_sort_index: None,
                    video_variants_json: None,
                    thumbnail: r.thumbnail,
                }),
                Err(e) => failed.push(crate::ingest::FailedImport {
                    path: url,
                    reason: e.to_string(),
                }),
            }
        }
        // Thumbnail decode, WebP encoding, palette extraction, filesystem
        // writes and SQLite work are all blocking. Keeping them inside this
        // closure prevents a large artwork image from occupying Tauri's
        // async command worker and making the rest of the app feel frozen.
        let library = Library::open(library_root)?;
        let mut conn = crate::db::open(&library.db_path())?;
        let mut report = ingest::import_links(&library, &mut conn, links)?;
        failed.append(&mut report.failed);
        report.failed = failed;
        Ok(report)
    })
    .await
    .map_err(|e| Error::Link(format!("link import panicked: {e}")))?
}

/// Downloads the media behind linked references.
///
/// Emits a `download-progress` event per item so a long run over a selection
/// reports which one it is on rather than freezing the UI.
///
/// The complete blocking lifetime—including the HTTP clients—is kept inside a
/// dedicated worker. This avoids dropping reqwest's internal runtime from an
/// async context while also keeping downloads, probing and thumbnail work off
/// Tauri's IPC/UI path.
#[tauri::command]
pub async fn download_assets(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    asset_ids: Vec<i64>,
) -> Result<crate::ingest::DownloadReport> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Emitter;

        let library = Library::open(library_root)?;
        let mut conn = crate::db::open(&library.db_path())?;
        // One client for X media, one for everything else. Handing a non-X URL
        // to the cookie-bearing client would send the session to a stranger.
        let x = crate::xsync::XSession::load(&library)
            .ok()
            .and_then(|session| crate::xsync::XClient::new(session).ok());
        let progress_app = app.clone();
        let report = ingest::download_assets(
            &library,
            &mut conn,
            &asset_ids,
            |url, dest| match (&x, crate::xsync::is_x_media(url)) {
                (Some(client), true) => client.stream_to(url, dest),
                _ => crate::link::download_to(url, dest),
            },
            |id, done, total| {
                let _ = progress_app.emit("download-progress", (id, done, total));
            },
        )?;
        for asset in &report.downloaded {
            crate::x_library::organize_asset(&library, &conn, asset.id)?;
        }
        Ok(report)
    })
    .await
    .map_err(|e| Error::Link(format!("download worker stopped unexpectedly: {e}")))?
}

/// Captures the frame currently under the playhead and imports it as a PNG.
///
/// The frame is decoded from the video itself rather than from the screen, so
/// player controls, the cursor, and other windows can never appear in it. The
/// resulting image inherits every board the source video belongs to, plus the
/// board currently being viewed when one was supplied.
#[tauri::command]
pub async fn capture_video_frame(
    state: tauri::State<'_, AppState>,
    asset_id: i64,
    position_ms: i64,
    board_id: Option<i64>,
) -> Result<VideoSnapshot> {
    let library_root = state.library.root().to_path_buf();
    let source = {
        let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        ingest::asset_by_id(&state.library, &conn, asset_id)?
            .ok_or_else(|| Error::Media("that video is no longer in the library".to_string()))?
    };
    if source.kind != MediaKind::Video {
        return Err(Error::Media(
            "only videos can produce a snapshot".to_string(),
        ));
    }
    if source.state != AssetState::Local {
        return Err(Error::Media(
            "download this video before taking a snapshot".to_string(),
        ));
    }

    // Seeking exactly to duration commonly lands after the final decodable
    // frame. Keep the request inside the clip, but preserve zero for streams
    // whose duration was unavailable.
    let captured_at_ms = match source.duration_ms {
        Some(duration) if duration > 0 => position_ms.clamp(0, duration.saturating_sub(1)),
        _ => position_ms.max(0),
    };
    let capture_name = snapshot_name(source.original_name.as_deref(), captured_at_ms);
    let video_path = PathBuf::from(&source.blob_path);

    let capture_path = {
        tauri::async_runtime::spawn_blocking(move || -> Result<PathBuf> {
            let png = crate::video::extract_frame_at(&video_path, captured_at_ms)?;
            let output_path = snapshot_temp_path(&capture_name)?;
            if let Err(error) = std::fs::write(&output_path, png) {
                if let Some(directory) = output_path.parent() {
                    let _ = std::fs::remove_dir(directory);
                }
                return Err(Error::io(&output_path, error));
            }
            Ok(output_path)
        })
        .await
        .map_err(|error| Error::Media(format!("snapshot worker stopped unexpectedly: {error}")))??
    };

    tauri::async_runtime::spawn_blocking(move || {
        finish_video_snapshot(
            library_root,
            asset_id,
            board_id,
            captured_at_ms,
            capture_path,
        )
    })
    .await
    .map_err(|error| Error::Media(format!("snapshot import stopped unexpectedly: {error}")))?
}

/// Imports a frame already decoded by the in-app video element.
///
/// This is the linked-video path: the webview has the current X frame in
/// memory, so saving that one PNG avoids downloading a potentially huge video
/// merely to ask FFmpeg for bytes the player has already decoded.
#[tauri::command]
pub async fn capture_rendered_video_frame(
    state: tauri::State<'_, AppState>,
    asset_id: i64,
    position_ms: i64,
    board_id: Option<i64>,
    png_bytes: Vec<u8>,
) -> Result<VideoSnapshot> {
    const MAX_RENDERED_FRAME_BYTES: usize = 64 * 1024 * 1024;
    if png_bytes.is_empty() || png_bytes.len() > MAX_RENDERED_FRAME_BYTES {
        return Err(Error::Media(
            "the decoded snapshot was empty or unexpectedly large".to_string(),
        ));
    }

    let library_root = state.library.root().to_path_buf();
    let source = {
        let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        ingest::asset_by_id(&state.library, &conn, asset_id)?
            .ok_or_else(|| Error::Media("that video is no longer in the library".to_string()))?
    };
    if source.kind != MediaKind::Video {
        return Err(Error::Media(
            "only videos can produce a snapshot".to_string(),
        ));
    }
    let captured_at_ms = position_ms.max(0);
    let capture_name = snapshot_name(source.original_name.as_deref(), captured_at_ms);

    tauri::async_runtime::spawn_blocking(move || {
        let capture_path = snapshot_temp_path(&capture_name)?;
        if let Err(error) = std::fs::write(&capture_path, png_bytes) {
            if let Some(directory) = capture_path.parent() {
                let _ = std::fs::remove_dir(directory);
            }
            return Err(Error::io(&capture_path, error));
        }
        finish_video_snapshot(
            library_root,
            asset_id,
            board_id,
            captured_at_ms,
            capture_path,
        )
    })
    .await
    .map_err(|error| Error::Media(format!("snapshot import stopped unexpectedly: {error}")))?
}

/// Available MP4 encodes for a linked X video, best quality first.
#[tauri::command]
pub async fn x_video_qualities(
    state: tauri::State<'_, AppState>,
    asset_id: i64,
) -> Result<Vec<crate::xsync::XVideoQuality>> {
    let (source_url, remote_url, stored_variants) = {
        let conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        conn.query_row(
            "SELECT source_url, remote_url, video_variants_json FROM assets
              WHERE id = ?1 AND state = 'linked' AND kind = 'video'",
            [asset_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| Error::Media("that linked X video is no longer available".to_string()))?
    };
    if let Some(json) = stored_variants {
        if let Ok(variants) = serde_json::from_str::<Vec<crate::xsync::XVideoQuality>>(&json) {
            if !variants.is_empty() {
                return Ok(variants);
            }
        }
    }
    let status_id = source_url
        .split("/status/")
        .nth(1)
        .and_then(|tail| tail.split(['/', '?', '#']).next())
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| Error::Media("this reference has no readable X post id".to_string()))?
        .to_string();
    tauri::async_runtime::spawn_blocking(move || {
        crate::xsync::public_video_qualities(&status_id, &remote_url)
    })
    .await
    .map_err(|error| {
        Error::X(format!(
            "video quality lookup stopped unexpectedly: {error}"
        ))
    })?
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

/// Whether a stored X session exists and still works.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XStatus {
    pub connected: bool,
    pub has_session: bool,
    /// Human-readable reason when `connected` is false.
    pub detail: String,
}

/// Saves pasted cookies, then immediately proves they work.
///
/// Verifying here rather than on the next sync means a bad paste is caught
/// while the user is still looking at the field they pasted into.
#[tauri::command]
pub async fn save_x_session(
    state: tauri::State<'_, AppState>,
    auth_token: String,
    ct0: String,
) -> Result<XStatus> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<XStatus> {
        let lib = crate::store::Library::open(&library_root)?;
        crate::xsync::XSession::save(&lib, &auth_token, &ct0)?;
        Ok(check_session(&lib))
    })
    .await
    .map_err(|e| Error::X(format!("save task panicked: {e}")))?
}

#[tauri::command]
pub async fn x_status(state: tauri::State<'_, AppState>) -> Result<XStatus> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<XStatus> {
        let lib = crate::store::Library::open(&library_root)?;
        Ok(check_session(&lib))
    })
    .await
    .map_err(|e| Error::X(format!("status task panicked: {e}")))?
}

#[tauri::command]
pub async fn clear_x_session(state: tauri::State<'_, AppState>) -> Result<XStatus> {
    let library_root = state.library.root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || -> Result<XStatus> {
        let lib = crate::store::Library::open(&library_root)?;
        crate::xsync::XSession::clear(&lib)?;
        Ok(check_session(&lib))
    })
    .await
    .map_err(|e| Error::X(format!("clear task panicked: {e}")))?
}

/// One authenticated call, so "connected" means it actually works rather than
/// "a file exists".
fn check_session(lib: &crate::store::Library) -> XStatus {
    let session = match crate::xsync::XSession::load(lib) {
        Ok(s) => s,
        Err(e) => {
            return XStatus {
                connected: false,
                has_session: false,
                detail: e.to_string(),
            }
        }
    };
    let client = match crate::xsync::XClient::new(session) {
        Ok(c) => c,
        Err(e) => {
            return XStatus {
                connected: false,
                has_session: true,
                detail: e.to_string(),
            }
        }
    };
    // Probe with the same operation a default sync uses, NOT the folder list.
    // Bookmark folders are a Premium feature and can return "User is not
    // authorized to use bookmark collections" on an account whose plain
    // bookmarks read perfectly well -- reporting that as "not connected" would
    // disable syncing over a capability syncing does not need.
    let probe = client.discover(&["Bookmarks"]).and_then(|specs| {
        client.fetch_bookmarks(
            &specs[0],
            &crate::xsync::BookmarkSource::All,
            &crate::xsync::FetchOptions {
                limit: 1,
                ..Default::default()
            },
        )
    });

    match probe {
        Ok(_) => XStatus {
            connected: true,
            has_session: true,
            detail: "signed in, bookmarks readable".to_string(),
        },
        Err(e) => XStatus {
            connected: false,
            has_session: true,
            detail: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_imported_and_inherits_the_video_board() {
        let root = std::env::temp_dir().join(format!(
            "burrow-snapshot-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(&root).unwrap();
        let mut conn = crate::db::open(&library.db_path()).unwrap();
        conn.execute(
            "INSERT INTO assets
                (hash, kind, duration_ms, ext, mime, width, height, bytes,
                 original_name, imported_at, state, content_hash)
             VALUES (?1, 'video', 5000, 'mp4', 'video/mp4', 320, 240, 10,
                     'clip.mp4', 1, 'local', ?1)",
            ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();
        let board = crate::boards::create_board(&conn, "Frames").unwrap();
        crate::boards::add_to_board(&mut conn, board.id, &[source_id]).unwrap();
        drop(conn);

        let capture_dir = root.join("temporary-capture");
        std::fs::create_dir(&capture_dir).unwrap();
        let capture_path = capture_dir.join(snapshot_name(Some("clip.mp4"), 1_234));
        image::RgbaImage::from_pixel(24, 16, image::Rgba([12, 34, 56, 255]))
            .save(&capture_path)
            .unwrap();

        let saved = finish_video_snapshot(root.clone(), source_id, None, 1_234, capture_path)
            .expect("save snapshot");
        assert!(!saved.duplicate);
        assert_eq!(saved.asset.kind, MediaKind::Image);
        assert_eq!(
            saved.asset.original_name.as_deref(),
            Some("clip - 00-00-01.234.png")
        );

        let conn = crate::db::open(&library.db_path()).unwrap();
        let board_assets =
            crate::boards::list_board_assets(&library, &conn, board.id, 10, 0).unwrap();
        assert!(board_assets.iter().any(|asset| asset.id == saved.asset.id));
        drop(conn);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_rendered_snapshot_does_not_require_downloaded_video_bytes() {
        let root = std::env::temp_dir().join(format!(
            "burrow-linked-snapshot-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let library = Library::open(&root).unwrap();
        let conn = crate::db::open(&library.db_path()).unwrap();
        conn.execute(
            "INSERT INTO assets
                (hash, kind, duration_ms, ext, mime, width, height, bytes,
                 original_name, imported_at, state, remote_url, source_url)
             VALUES (?1, 'video', 5000, 'mp4', 'video/mp4', 320, 240, 0,
                     'x-stream.mp4', 1, 'linked', ?2, ?3)",
            rusqlite::params![
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "https://video.twimg.com/example.mp4",
                "https://x.com/example/status/123"
            ],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();
        drop(conn);

        let capture_dir = root.join("temporary-linked-capture");
        std::fs::create_dir(&capture_dir).unwrap();
        let capture_path = capture_dir.join(snapshot_name(Some("x-stream.mp4"), 2_000));
        image::RgbaImage::from_pixel(24, 16, image::Rgba([90, 80, 70, 255]))
            .save(&capture_path)
            .unwrap();

        let saved = finish_video_snapshot(root.clone(), source_id, None, 2_000, capture_path)
            .expect("save rendered snapshot");
        assert_eq!(saved.asset.kind, MediaKind::Image);
        assert_eq!(saved.asset.state, AssetState::Local);
        assert!(PathBuf::from(&saved.asset.blob_path).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_names_are_windows_safe() {
        assert_eq!(
            snapshot_name(Some("a<b>:c?.mp4"), 3_661_007),
            "a_b__c_ - 01-01-01.007.png"
        );
    }
}
