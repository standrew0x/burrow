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
        let found = items.len();
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
            });
        }

        // Staged outside the library so a failed run leaves no half-imported
        // blobs behind; ingest copies what it accepts.
        let staging = std::env::temp_dir().join(format!("burrow-xsync-{}", std::process::id()));
        std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;

        let mut downloaded = Vec::new();
        for item in &items {
            match client.download(item, &staging) {
                Ok(p) => downloaded.push((p, item.tweet_url.clone())),
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
                images,
                videos: report.imported.len() - images,
                failed,
            })
        }
        Fetched::Files {
            label,
            found,
            downloaded,
            mut failed,
            staging,
        } => {
            let files: Vec<std::path::PathBuf> =
                downloaded.iter().map(|(p, _)| p.clone()).collect();
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
                    if let Some((_, url)) = downloaded.iter().find(|(p, _)| {
                        p.file_name().map(|f| f.to_string_lossy() == name.as_str()) == Some(true)
                    }) {
                        stmt.execute(rusqlite::params![url, asset.id])?;
                    }
                }
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
                images,
                videos: report.imported.len() - images,
                failed,
            })
        }
    }
}

/// What the blocking half of a sync produced.
enum Fetched {
    Links {
        label: String,
        found: usize,
        links: Vec<crate::ingest::PendingLink>,
        failed: Vec<crate::ingest::FailedImport>,
    },
    Files {
        label: String,
        found: usize,
        downloaded: Vec<(std::path::PathBuf, String)>,
        failed: Vec<crate::ingest::FailedImport>,
        staging: std::path::PathBuf,
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
/// X post links go through the authenticated client, which can read the media
/// out of the timeline API; everything else is resolved from its OpenGraph
/// tags. The two use different HTTP clients on purpose -- see [`crate::link`].
#[tauri::command]
pub async fn add_links(
    state: tauri::State<'_, AppState>,
    urls: Vec<String>,
) -> Result<ImportReport> {
    let library_root = state.library.root().to_path_buf();

    let (links, failed) = tauri::async_runtime::spawn_blocking(
        move || -> (Vec<crate::ingest::PendingLink>, Vec<crate::ingest::FailedImport>) {
            // An X session is optional. Without one, x.com links still resolve
            // through OpenGraph -- worse metadata, but a working tile.
            let x = crate::store::Library::open(&library_root)
                .ok()
                .and_then(|lib| crate::xsync::XSession::load(&lib).ok())
                .and_then(|session| crate::xsync::XClient::new(session).ok());

            let mut links = Vec::new();
            let mut failed = Vec::new();
            for url in urls {
                let url = url.trim().to_string();
                if url.is_empty() {
                    continue;
                }
                match crate::link::resolve(&url, x.as_ref()) {
                    Ok(r) => links.push(crate::ingest::PendingLink {
                        page_url: r.page_url,
                        media_url: r.media_url,
                        kind: r.kind,
                        title: r.title,
                        thumbnail: r.thumbnail,
                    }),
                    Err(e) => failed.push(crate::ingest::FailedImport {
                        path: url,
                        reason: e.to_string(),
                    }),
                }
            }
            (links, failed)
        },
    )
    .await
    .map_err(|e| Error::Link(format!("link resolution panicked: {e}")))?;

    let mut report = {
        let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
        ingest::import_links(&state.library, &mut conn, links)?
    };
    let mut failed = failed;
    failed.append(&mut report.failed);
    report.failed = failed;
    Ok(report)
}

/// Downloads the media behind linked references.
///
/// Emits a `download-progress` event per item so a long run over a selection
/// reports which one it is on rather than freezing the UI.
///
/// Synchronous, and that is load-bearing. `reqwest::blocking::Client` owns an
/// internal tokio runtime, and dropping a runtime inside an async context
/// panics -- so an `async` version that built the X client in `spawn_blocking`
/// and returned it here blew up on drop, at the end of the command, poisoning
/// the database mutex and taking every later command down with it. The client
/// must be created and dropped on the same non-async thread. Tauri runs sync
/// commands off the UI thread, which is what `import_paths` already relies on
/// for equally long work.
#[tauri::command]
pub fn download_assets(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    asset_ids: Vec<i64>,
) -> Result<crate::ingest::DownloadReport> {
    use tauri::Emitter;

    // One client for X media, one for everything else. Handing a non-X URL to
    // the cookie-bearing client would send the session to a stranger's server.
    let x = crate::xsync::XSession::load(&state.library)
        .ok()
        .and_then(|session| crate::xsync::XClient::new(session).ok());

    let mut conn = state.conn.lock().map_err(|_| Error::Poisoned)?;
    ingest::download_assets(
        &state.library,
        &mut conn,
        &asset_ids,
        |url, dest| match (&x, crate::xsync::is_x_media(url)) {
            (Some(client), true) => client.stream_to(url, dest),
            // No session, or not an X URL: the generic path, which validates
            // the address and caps the transfer.
            _ => crate::link::download_to(url, dest),
        },
        |id, done, total| {
            let _ = app.emit("download-progress", (id, done, total));
        },
    )
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
