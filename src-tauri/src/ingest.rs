//! Import orchestration.
//!
//! Four phases, chosen so the expensive work parallelises but SQLite writes
//! stay serial:
//!
//! 1. hash every candidate in parallel (cheap: read + blake3)
//! 2. one query to drop digests already in the library
//! 3. decode / thumbnail / palette the survivors in parallel, writing blobs
//! 4. one transaction to insert the rows
//!
//! Phase 3 re-reads each file rather than carrying phase 1's bytes forward. A
//! drop of 500 photos is several GB; holding all of it to save a re-read that
//! the OS page cache will almost certainly serve is the wrong trade.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::color::{palette_from_pixels, Swatch};
use crate::error::{Error, Result};
use crate::image_ops::{self, THUMB_LONG_EDGE};
use crate::store::{self, Library};
use crate::video;

/// Palette size. Five reads as a palette strip in the UI and is enough to
/// cover an image's structure without splitting near-identical shades.
const PALETTE_SIZE: usize = 5;

/// Files decoded concurrently in phase 3. Bounds peak memory: a decoded 48MP
/// image is ~190MB as RGBA8, so an unbounded rayon fan-out over a large drop
/// can exhaust RAM on a 16GB machine.
const DECODE_CHUNK: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "video" => MediaKind::Video,
            _ => MediaKind::Image,
        }
    }
}

/// Whether the library holds this reference's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetState {
    /// The file is in the blob store.
    Local,
    /// Only a thumbnail and a URL. Plays by streaming from the remote host,
    /// and becomes `Local` when downloaded.
    Linked,
}

impl AssetState {
    fn as_str(self) -> &'static str {
        match self {
            AssetState::Local => "local",
            AssetState::Linked => "linked",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "linked" => AssetState::Linked,
            _ => AssetState::Local,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRow {
    pub id: i64,
    pub hash: String,
    pub kind: MediaKind,
    pub state: AssetState,
    /// Present for video only, and only once the bytes are local -- a link has
    /// no duration until something has actually read the file.
    pub duration_ms: Option<i64>,
    pub ext: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    /// Size on disk. Zero for a link, which is the honest answer: the library
    /// is storing a thumbnail, not a video.
    pub bytes: i64,
    pub original_name: Option<String>,
    /// The page this came from -- a post, a video page -- for "open original".
    pub source_url: Option<String>,
    /// The media file on the remote host. Present only while linked, and what
    /// the player streams from.
    pub remote_url: Option<String>,
    /// The user's own note. `None` when unset; never an empty string.
    pub note: Option<String>,
    pub imported_at: i64,
    pub swatches: Vec<Swatch>,
    /// Absolute path to the thumbnail, included on every row so the grid does
    /// not need one IPC round-trip per tile to render.
    pub thumb_path: String,
    /// Absolute path to the stored original. Meaningless while linked -- the
    /// file is not there -- so the UI branches on `state` before using it.
    pub blob_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedImport {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub imported: Vec<AssetRow>,
    /// Files skipped because their digest was already in the library, including
    /// duplicates within this same batch.
    pub duplicates: usize,
    /// Skipped because they had been deleted from the library before.
    pub dismissed: usize,
    pub failed: Vec<FailedImport>,
}

/// Everything computed off-thread, ready for a DB insert.
struct Prepared {
    hash: String,
    kind: MediaKind,
    ext: String,
    mime: String,
    width: u32,
    height: u32,
    bytes: i64,
    duration_ms: Option<i64>,
    original_name: Option<String>,
    swatches: Vec<Swatch>,
}

/// Bytes read for format sniffing. Every container we care about declares
/// itself well inside this, and it keeps detection off the 2GB read path.
const HEADER_SNIFF_BYTES: usize = 4096;

/// Still-image extensions collected when walking a dropped directory.
///
/// Files named explicitly are always attempted regardless of extension --
/// format is sniffed from content. This list only gates *discovered* files, so
/// dropping a project folder does not turn its .txt, .psd and .ai files into a
/// wall of failures the user has to scroll past.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "avif", "bmp", "tif", "tiff", "ico",
];

/// Container extension to MIME. Only mp4 and webm play in WebView2; the rest
/// are stored and openable but will not render in an inline `<video>`.
fn video_mime(ext: &str) -> &'static str {
    match ext {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "ts" | "m2ts" => "video/mp2t",
        _ => "video/mpeg",
    }
}

/// Recursion cap for directory walks. Windows makes junctions and symlinks
/// easy to create by accident, and `read_dir` follows them happily -- without
/// a cap a cyclic junction is an infinite walk.
const MAX_WALK_DEPTH: usize = 8;

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn is_media_extension(path: &Path) -> bool {
    extension_of(path).is_some_and(|e| {
        IMAGE_EXTENSIONS.contains(&e.as_str()) || video::VIDEO_EXTENSIONS.contains(&e.as_str())
    })
}

fn walk_into(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    // An unreadable directory is skipped rather than failing the batch: a drop
    // of ten folders should not be lost because one of them is permission-denied.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_into(&path, depth + 1, out);
        } else if is_media_extension(&path) {
            out.push(path);
        }
    }
}

/// Expands any directories in `paths` into the image files beneath them.
///
/// The OS hands over exactly what was dragged -- drop a folder and you get one
/// path, the folder itself. Without this, `fs::read` on that path fails and the
/// user sees "Access is denied" instead of an import.
fn expand_inputs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        if path.is_dir() {
            walk_into(path, 0, &mut out);
        } else {
            out.push(path.clone());
        }
    }
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn file_name_of(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Imports `paths` into the library. Unreadable or undecodable files are
/// reported in [`ImportReport::failed`] rather than aborting the batch -- one
/// corrupt file in a 200-file drop should not lose the other 199.
pub fn import_paths(
    lib: &Library,
    conn: &mut Connection,
    paths: &[PathBuf],
) -> Result<ImportReport> {
    let mut report = ImportReport::default();

    // Dropped folders arrive as a single path; expand before anything else.
    let paths = expand_inputs(paths);
    if paths.is_empty() {
        return Ok(report);
    }

    // --- Phase 1: hash in parallel ---
    // Memory-mapped, so a 2GB video costs no heap. The previous `fs::read` here
    // was invisible for images and would have been ruinous for video.
    let hashed: Vec<std::result::Result<(PathBuf, String), FailedImport>> = paths
        .par_iter()
        .map(|path| match store::hash_file(path) {
            Ok(hash) => Ok((path.clone(), hash)),
            Err(e) => Err(FailedImport {
                path: path.display().to_string(),
                reason: e.to_string(),
            }),
        })
        .collect();

    let mut candidates: Vec<(PathBuf, String)> = Vec::with_capacity(hashed.len());
    for outcome in hashed {
        match outcome {
            Ok(pair) => candidates.push(pair),
            Err(f) => report.failed.push(f),
        }
    }

    // --- Phase 2: drop digests we already hold ---
    let mut seen: HashSet<String> = HashSet::new();
    let mut fresh: Vec<(PathBuf, String)> = Vec::with_capacity(candidates.len());
    {
        let mut exists = conn.prepare("SELECT 1 FROM assets WHERE hash = ?1")?;
        for (path, hash) in candidates.drain(..) {
            // `seen` catches the same bytes appearing twice in one drop, which
            // the DB check alone would miss since neither is committed yet.
            let already = !seen.insert(hash.clone()) || exists.exists(rusqlite::params![&hash])?;
            if already {
                report.duplicates += 1;
            } else {
                fresh.push((path, hash));
            }
        }
    }

    // --- Phase 3: decode, thumbnail, palette, write blobs ---
    let mut prepared: Vec<Prepared> = Vec::with_capacity(fresh.len());
    for chunk in fresh.chunks(DECODE_CHUNK) {
        let results: Vec<std::result::Result<Prepared, FailedImport>> = chunk
            .par_iter()
            .map(|(path, hash)| {
                prepare_one(lib, path, hash).map_err(|e| FailedImport {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            })
            .collect();

        for outcome in results {
            match outcome {
                Ok(p) => prepared.push(p),
                Err(f) => report.failed.push(f),
            }
        }
    }

    // --- Phase 4: one transaction for all inserts ---
    let imported_at = now_unix();
    let tx = conn.transaction()?;
    for p in &prepared {
        tx.execute(
            "INSERT INTO assets
                (hash, kind, duration_ms, ext, mime, width, height, bytes,
                 original_name, imported_at, state, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'local', ?1)",
            rusqlite::params![
                p.hash,
                p.kind.as_str(),
                p.duration_ms,
                p.ext,
                p.mime,
                p.width,
                p.height,
                p.bytes,
                p.original_name,
                imported_at,
            ],
        )?;
        let id = tx.last_insert_rowid();

        for (ordinal, s) in p.swatches.iter().enumerate() {
            tx.execute(
                "INSERT INTO swatches (asset_id, ordinal, weight, l, a, b, hex)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, ordinal as i64, s.weight, s.l, s.a, s.b, s.hex],
            )?;
        }

        report.imported.push(AssetRow {
            id,
            hash: p.hash.clone(),
            kind: p.kind,
            state: AssetState::Local,
            duration_ms: p.duration_ms,
            ext: p.ext.clone(),
            mime: p.mime.clone(),
            width: p.width,
            height: p.height,
            bytes: p.bytes,
            original_name: p.original_name.clone(),
            source_url: None,
            remote_url: None,
            note: None,
            imported_at,
            swatches: p.swatches.clone(),
            thumb_path: lib.thumb_path(&p.hash).display().to_string(),
            blob_path: lib.blob_path(&p.hash, &p.ext).display().to_string(),
        });
    }
    tx.commit()?;

    Ok(report)
}

/// Classifies by content, not by filename.
///
/// Image formats declare themselves in their first few bytes, so a header
/// sniff settles those cheaply. Anything else is offered to ffprobe, which is
/// the only reliable way to tell a real .mp4 from something merely named one.
fn prepare_one(lib: &Library, path: &Path, hash: &str) -> Result<Prepared> {
    let header = store::read_header(path, HEADER_SNIFF_BYTES)?;

    if let Some((ext, mime)) = image_ops::format_of(&header) {
        return prepare_image(lib, path, hash, ext, mime);
    }
    if let Some(info) = video::probe(path)? {
        return prepare_video(lib, path, hash, info);
    }
    Err(Error::Unsupported(path.to_path_buf()))
}

fn prepare_image(
    lib: &Library,
    path: &Path,
    hash: &str,
    ext: &str,
    mime: &str,
) -> Result<Prepared> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let image = image_ops::decode(path, &bytes)?;

    let prepared = Prepared {
        hash: hash.to_string(),
        kind: MediaKind::Image,
        ext: ext.to_string(),
        mime: mime.to_string(),
        width: image.width(),
        height: image.height(),
        bytes: bytes.len() as i64,
        duration_ms: None,
        original_name: file_name_of(path),
        swatches: palette_from_pixels(&image_ops::palette_samples(&image), PALETTE_SIZE),
    };

    let thumb = image_ops::encode_webp(&image_ops::thumbnail(&image, THUMB_LONG_EDGE))?;

    // Blobs are written before the DB row exists. That ordering means a crash
    // between the two leaves an orphaned blob -- wasted disk, reclaimable by a
    // GC pass -- rather than a row pointing at a file that was never written,
    // which would render as a broken tile forever.
    lib.write_if_absent(&lib.blob_path(hash, ext), &bytes)?;
    lib.write_if_absent(&lib.thumb_path(hash), &thumb)?;

    Ok(prepared)
}

fn prepare_video(
    lib: &Library,
    path: &Path,
    hash: &str,
    info: video::VideoInfo,
) -> Result<Prepared> {
    // Container comes from the extension because ffprobe reports the codec,
    // not the wrapper, and the wrapper is what decides playability.
    let ext = extension_of(path).unwrap_or_else(|| "mp4".to_string());
    let mime = video_mime(&ext);

    // The poster frame goes through the exact same thumbnail and OkLab palette
    // path as a still, so colour search works across video for free.
    let frame_png = video::extract_poster_frame(path, info.duration_ms)?;
    let frame = image_ops::decode(path, &frame_png)?;

    let thumb = image_ops::encode_webp(&image_ops::thumbnail(&frame, THUMB_LONG_EDGE))?;
    let swatches = palette_from_pixels(&image_ops::palette_samples(&frame), PALETTE_SIZE);

    let size = std::fs::metadata(path)
        .map(|m| m.len() as i64)
        .map_err(|e| Error::io(path, e))?;

    // Streamed, never buffered -- this is the whole reason video forced the
    // hashing and blob paths off `fs::read`.
    lib.copy_if_absent(&lib.blob_path(hash, &ext), path)?;
    lib.write_if_absent(&lib.thumb_path(hash), &thumb)?;

    Ok(Prepared {
        hash: hash.to_string(),
        kind: MediaKind::Video,
        ext,
        mime: mime.to_string(),
        // Dimensions come from the stream, not the decoded frame: ffmpeg may
        // hand back a frame with square pixels where the stream is anamorphic.
        width: if info.width > 0 {
            info.width
        } else {
            frame.width()
        },
        height: if info.height > 0 {
            info.height
        } else {
            frame.height()
        },
        bytes: size,
        duration_ms: Some(info.duration_ms),
        original_name: file_name_of(path),
        swatches,
    })
}

// --- linked references ---

/// A resolved link, ready to become a row.
///
/// Deliberately not [`link::Resolved`]: X sync produces these too, and it has
/// no business going through the URL resolver for media it already parsed out
/// of the timeline API.
pub struct PendingLink {
    pub page_url: String,
    pub media_url: String,
    pub kind: MediaKind,
    pub title: Option<String>,
    /// Encoded still image, in whatever format the source served.
    pub thumbnail: Vec<u8>,
}

/// The storage key for a link.
///
/// Digest of the media URL, not of any bytes: there are none yet. This is what
/// names the thumbnail file, and -- once downloaded -- the blob, which is why
/// it must not change when the real content hash finally becomes known.
pub fn link_hash(media_url: &str) -> String {
    store::hash_bytes(media_url.as_bytes())
}

/// Container extension implied by a media URL.
fn ext_from_url(url: &str, kind: MediaKind) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let candidate = path
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());

    match candidate {
        Some(e)
            if (kind == MediaKind::Video && video::VIDEO_EXTENSIONS.contains(&e.as_str()))
                || (kind == MediaKind::Image && IMAGE_EXTENSIONS.contains(&e.as_str())) =>
        {
            e
        }
        // A URL that names no usable extension is normal -- CDN paths and
        // player pages routinely have none.
        _ => match kind {
            MediaKind::Video => "mp4".to_string(),
            MediaKind::Image => "jpg".to_string(),
        },
    }
}

/// Adds references that live on someone else's server.
///
/// Only the thumbnail is fetched and stored. The palette comes off that
/// thumbnail, so colour search covers linked references exactly as it covers
/// downloaded ones -- which is the point of holding a thumbnail rather than
/// just a URL.
pub fn import_links(
    lib: &Library,
    conn: &mut Connection,
    links: Vec<PendingLink>,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    if links.is_empty() {
        return Ok(report);
    }

    // Prepared outside the transaction: decoding and encoding a thumbnail is
    // slow enough that holding a write lock across it would block the grid.
    struct Ready {
        link: PendingLink,
        hash: String,
        ext: String,
        width: u32,
        height: u32,
        swatches: Vec<Swatch>,
    }

    let mut ready: Vec<Ready> = Vec::with_capacity(links.len());
    let mut seen: HashSet<String> = HashSet::new();
    {
        let mut exists = conn.prepare("SELECT 1 FROM assets WHERE remote_url = ?1")?;
        let mut was_dismissed = conn.prepare("SELECT 1 FROM dismissed WHERE remote_url = ?1")?;
        for link in links {
            // Thrown away on purpose once already. Counted rather than silently
            // dropped: a skip nobody can see is the same as a sync that lost
            // things, which is exactly the complaint this feature answers.
            if was_dismissed.exists(rusqlite::params![&link.media_url])? {
                report.dismissed += 1;
                continue;
            }
            // Same URL twice in one batch, or already in the library.
            let already = !seen.insert(link.media_url.clone())
                || exists.exists(rusqlite::params![&link.media_url])?;
            if already {
                report.duplicates += 1;
                continue;
            }

            let hash = link_hash(&link.media_url);
            match prepare_link_thumbnail(lib, &hash, &link) {
                Ok((width, height, swatches)) => ready.push(Ready {
                    ext: ext_from_url(&link.media_url, link.kind),
                    link,
                    hash,
                    width,
                    height,
                    swatches,
                }),
                Err(e) => report.failed.push(FailedImport {
                    path: link.page_url.clone(),
                    reason: e.to_string(),
                }),
            }
        }
    }

    let imported_at = now_unix();
    let tx = conn.transaction()?;
    for r in &ready {
        let mime = match r.link.kind {
            MediaKind::Video => video_mime(&r.ext).to_string(),
            MediaKind::Image => image_ops::mime_for_extension(&r.ext).to_string(),
        };

        tx.execute(
            "INSERT INTO assets
                (hash, kind, duration_ms, ext, mime, width, height, bytes,
                 original_name, source_url, imported_at, state, remote_url)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                r.hash,
                r.link.kind.as_str(),
                r.ext,
                mime,
                r.width,
                r.height,
                r.link.title,
                r.link.page_url,
                imported_at,
                // Bound rather than written into the SQL so the column can only
                // ever hold a value the enum can read back.
                AssetState::Linked.as_str(),
                r.link.media_url,
            ],
        )?;
        let id = tx.last_insert_rowid();

        for (ordinal, s) in r.swatches.iter().enumerate() {
            tx.execute(
                "INSERT INTO swatches (asset_id, ordinal, weight, l, a, b, hex)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, ordinal as i64, s.weight, s.l, s.a, s.b, s.hex],
            )?;
        }

        report.imported.push(AssetRow {
            id,
            hash: r.hash.clone(),
            kind: r.link.kind,
            state: AssetState::Linked,
            duration_ms: None,
            ext: r.ext.clone(),
            mime,
            width: r.width,
            height: r.height,
            bytes: 0,
            original_name: r.link.title.clone(),
            source_url: Some(r.link.page_url.clone()),
            remote_url: Some(r.link.media_url.clone()),
            note: None,
            imported_at,
            swatches: r.swatches.clone(),
            thumb_path: lib.thumb_path(&r.hash).display().to_string(),
            blob_path: lib.blob_path(&r.hash, &r.ext).display().to_string(),
        });
    }
    tx.commit()?;

    Ok(report)
}

/// Decodes the fetched still, stores it as this reference's thumbnail, and
/// reads its palette. Returns the dimensions the tile should claim.
fn prepare_link_thumbnail(
    lib: &Library,
    hash: &str,
    link: &PendingLink,
) -> Result<(u32, u32, Vec<Swatch>)> {
    let path = Path::new(&link.page_url);
    let image = image_ops::decode(path, &link.thumbnail)?;

    let thumb = image_ops::encode_webp(&image_ops::thumbnail(&image, THUMB_LONG_EDGE))?;
    lib.write_if_absent(&lib.thumb_path(hash), &thumb)?;

    let swatches = palette_from_pixels(&image_ops::palette_samples(&image), PALETTE_SIZE);
    // The poster's dimensions, not the media's -- nothing here has read the
    // media. They share an aspect ratio, which is all the grid needs, and the
    // real numbers land on the row when the file is eventually downloaded.
    Ok((image.width(), image.height(), swatches))
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DownloadReport {
    /// Rows that now hold real bytes.
    pub downloaded: Vec<AssetRow>,
    /// Links whose content turned out to already be in the library. The link
    /// row is gone and its board memberships moved to the asset that has the
    /// bytes; nothing was lost.
    pub deduplicated: usize,
    pub bytes_written: i64,
    pub failed: Vec<FailedImport>,
}

/// Fetches the bytes behind linked references and turns them into local ones.
///
/// `fetch` does the transfer, so the caller decides which HTTP client applies:
/// X media needs the authenticated client, anything else must go through the
/// cookieless one. Keeping that choice out here is what stops session cookies
/// reaching an arbitrary host.
pub fn download_assets(
    lib: &Library,
    conn: &mut Connection,
    asset_ids: &[i64],
    mut fetch: impl FnMut(&str, &Path) -> Result<u64>,
    mut progress: impl FnMut(i64, usize, usize),
) -> Result<DownloadReport> {
    let mut report = DownloadReport::default();
    if asset_ids.is_empty() {
        return Ok(report);
    }

    // Read the whole worklist up front so the connection is free during the
    // transfers, which are the slow part by orders of magnitude.
    let mut pending: Vec<(i64, String, String, String, MediaKind)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT hash, remote_url, ext, kind FROM assets
             WHERE id = ?1 AND state = 'linked' AND remote_url IS NOT NULL",
        )?;
        for id in asset_ids {
            let mut rows = stmt.query([id])?;
            if let Some(r) = rows.next()? {
                pending.push((
                    *id,
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    MediaKind::from_str(&r.get::<_, String>(3)?),
                ));
            }
        }
    }

    let total = pending.len();
    for (n, (id, hash, remote_url, ext, _kind)) in pending.into_iter().enumerate() {
        progress(id, n + 1, total);
        match materialize(lib, conn, id, &hash, &remote_url, &ext, &mut fetch) {
            Ok(Materialized::Downloaded(row, bytes)) => {
                report.bytes_written += bytes;
                report.downloaded.push(*row);
            }
            Ok(Materialized::AlreadyHeld) => report.deduplicated += 1,
            Err(e) => report.failed.push(FailedImport {
                path: remote_url.clone(),
                reason: e.to_string(),
            }),
        }
    }
    Ok(report)
}

enum Materialized {
    Downloaded(Box<AssetRow>, i64),
    AlreadyHeld,
}

#[allow(clippy::too_many_arguments)]
fn materialize(
    lib: &Library,
    conn: &mut Connection,
    id: i64,
    hash: &str,
    remote_url: &str,
    ext: &str,
    fetch: &mut impl FnMut(&str, &Path) -> Result<u64>,
) -> Result<Materialized> {
    // Downloaded beside the destination rather than into %TEMP%: a rename
    // within one volume is atomic and instant, while a cross-volume move is a
    // second full copy of a file that may be hundreds of megabytes.
    let dest = lib.blob_path(hash, ext);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let staging = dest.with_extension("downloading");
    let _guard = CleanupOnDrop(&staging);

    fetch(remote_url, &staging)?;

    let content_hash = store::hash_file(&staging)?;

    // The bytes may already be here under a different reference -- the same
    // video dragged in from disk, or linked twice from different posts.
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM assets WHERE content_hash = ?1 AND id <> ?2 AND state = 'local'",
            rusqlite::params![&content_hash, id],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(keeper) = existing {
        // Hand the link's board memberships to the copy that has the bytes, so
        // deduplicating never silently empties a board.
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO board_items (board_id, asset_id, added_at)
             SELECT board_id, ?1, added_at FROM board_items WHERE asset_id = ?2",
            rusqlite::params![keeper, id],
        )?;
        tx.execute("DELETE FROM assets WHERE id = ?1", [id])?;
        tx.commit()?;
        let _ = std::fs::remove_file(lib.thumb_path(hash));
        return Ok(Materialized::AlreadyHeld);
    }

    // Now that the bytes are known-wanted, read what they actually are. The
    // link's stored width/height came from a poster and the duration was
    // unknown; both get corrected here.
    let found = describe_downloaded(&staging)?;
    let size = std::fs::metadata(&staging)
        .map(|m| m.len() as i64)
        .map_err(|e| Error::io(&staging, e))?;

    // The link's thumbnail came from a publisher's poster; this one comes from
    // the media, so replace rather than skip if one is already there.
    let thumb_path = lib.thumb_path(hash);
    let _ = std::fs::remove_file(&thumb_path);
    lib.write_if_absent(&thumb_path, &found.thumb)?;
    // Rename last: until this succeeds the row still says 'linked' and the
    // reference still streams, so a failure here costs a retry, not a tile.
    std::fs::rename(&staging, &dest).map_err(|e| Error::io(&dest, e))?;

    // `remote_url` is kept, not cleared. `state` already records that the bytes
    // are held locally, and the URL is what tells a later sync "you already
    // have this". Clearing it meant downloading a reference made the next sync
    // add it back as a second, linked copy of something already on disk.
    conn.execute(
        "UPDATE assets
            SET state = ?1, content_hash = ?2, bytes = ?3, width = ?4,
                height = ?5, duration_ms = ?6, kind = ?7
          WHERE id = ?8",
        rusqlite::params![
            AssetState::Local.as_str(),
            content_hash,
            size,
            found.width,
            found.height,
            found.duration_ms,
            found.kind.as_str(),
            id
        ],
    )?;

    let mut row = conn.query_row(&format!("{ASSET_COLUMNS} WHERE id = ?1"), [id], |r| {
        row_to_asset(lib, r)
    })?;
    row.swatches = swatches_for(conn, id)?;
    Ok(Materialized::Downloaded(Box::new(row), size))
}

/// Removes a staging file unless it was renamed away.
///
/// A failed or cancelled download must not leave a half-written file sitting
/// next to the blob it was going to become.
struct CleanupOnDrop<'a>(&'a Path);

impl Drop for CleanupOnDrop<'_> {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = std::fs::remove_file(self.0);
        }
    }
}

/// What a downloaded file turned out to be.
struct Downloaded {
    width: u32,
    height: u32,
    duration_ms: Option<i64>,
    /// Encoded WebP thumbnail, regenerated from the real media.
    thumb: Vec<u8>,
    kind: MediaKind,
}

/// Reads a freshly downloaded file: real dimensions, duration, and a thumbnail
/// generated from the media itself rather than from whatever poster the source
/// published.
///
/// The kind recorded when the link was added is not passed in on purpose. It
/// was a guess made from a URL and a content type; the file now on disk is
/// authoritative, and preferring the guess would be how a video ends up stored
/// as an image that will not play.
fn describe_downloaded(path: &Path) -> Result<Downloaded> {
    let header = store::read_header(path, HEADER_SNIFF_BYTES)?;

    // Trust the file over the expectation: a URL that looked like a video can
    // serve an image, and storing it as the wrong kind breaks playback.
    if image_ops::format_of(&header).is_some() {
        let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        let image = image_ops::decode(path, &bytes)?;
        let thumb = image_ops::encode_webp(&image_ops::thumbnail(&image, THUMB_LONG_EDGE))?;
        return Ok(Downloaded {
            width: image.width(),
            height: image.height(),
            duration_ms: None,
            thumb,
            kind: MediaKind::Image,
        });
    }

    if let Some(info) = video::probe(path)? {
        let frame_png = video::extract_poster_frame(path, info.duration_ms)?;
        let frame = image_ops::decode(path, &frame_png)?;
        let thumb = image_ops::encode_webp(&image_ops::thumbnail(&frame, THUMB_LONG_EDGE))?;
        return Ok(Downloaded {
            // Dimensions come from the stream, not the decoded frame: ffmpeg
            // may hand back square pixels where the stream is anamorphic.
            width: if info.width > 0 {
                info.width
            } else {
                frame.width()
            },
            height: if info.height > 0 {
                info.height
            } else {
                frame.height()
            },
            duration_ms: Some(info.duration_ms),
            thumb,
            kind: MediaKind::Video,
        });
    }

    Err(Error::Link(format!(
        "downloaded {} bytes that are neither an image nor a video -- the link \
         probably served an error page",
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    )))
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReport {
    pub deleted: usize,
    pub bytes_freed: i64,
    /// How many of the deleted references were remembered, so a later sync
    /// will not offer them again.
    pub dismissed: usize,
    /// Rows removed whose blob or thumbnail could not be unlinked -- usually a
    /// file lock. The reference is gone from the library either way; this is
    /// wasted disk, not a broken tile.
    pub orphaned_files: Vec<String>,
}

/// Permanently removes assets: database rows first, then the stored files.
///
/// Row-then-file, deliberately. A crash between the two leaves an orphaned
/// blob -- reclaimable disk. The reverse order would leave a row pointing at a
/// file that no longer exists, which renders as a permanently broken tile.
///
/// `assets.hash` is UNIQUE, so one row owns one blob and there is no risk of
/// unlinking a file another reference still needs.
pub fn delete_assets(
    lib: &Library,
    conn: &mut Connection,
    asset_ids: &[i64],
) -> Result<DeleteReport> {
    let mut report = DeleteReport::default();
    if asset_ids.is_empty() {
        return Ok(report);
    }

    // Collect what to unlink before the rows disappear.
    let mut doomed: Vec<(String, String, i64)> = Vec::with_capacity(asset_ids.len());
    // And what to remember having thrown away.
    let mut tombstones: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT hash, ext, bytes, remote_url, source_url, original_name
               FROM assets WHERE id = ?1",
        )?;
        for id in asset_ids {
            let mut rows = stmt.query([id])?;
            if let Some(r) = rows.next()? {
                doomed.push((r.get(0)?, r.get(1)?, r.get(2)?));
                // Only references with a remote identity get a tombstone. A
                // dropped local import has nothing to be recognised by later,
                // and re-adding a file you dragged in again is a deliberate act
                // that should simply work.
                if let Some(remote) = r.get::<_, Option<String>>(3)? {
                    tombstones.push((remote, r.get(4)?, r.get(5)?));
                }
            }
        }
    }

    let dismissed_at = now_unix();
    let tx = conn.transaction()?;
    {
        // Swatches and board memberships cascade from the asset row.
        let mut stmt = tx.prepare("DELETE FROM assets WHERE id = ?1")?;
        for id in asset_ids {
            report.deleted += stmt.execute([id])?;
        }
    }
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO dismissed (remote_url, page_url, title, dismissed_at)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (remote, page, title) in &tombstones {
            stmt.execute(rusqlite::params![remote, page, title, dismissed_at])?;
            report.dismissed += 1;
        }
    }
    tx.commit()?;

    for (hash, ext, bytes) in doomed {
        let blob = lib.blob_path(&hash, &ext);
        let thumb = lib.thumb_path(&hash);
        let mut clean = true;
        for path in [&blob, &thumb] {
            if path.exists() && std::fs::remove_file(path).is_err() {
                report.orphaned_files.push(path.display().to_string());
                clean = false;
            }
        }
        if clean {
            report.bytes_freed += bytes;
        }
    }

    Ok(report)
}

/// A reference that was deleted and will not be offered again.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dismissed {
    pub remote_url: String,
    pub page_url: Option<String>,
    pub title: Option<String>,
    pub dismissed_at: i64,
}

/// Everything currently being skipped, newest first.
///
/// This list has to be visible somewhere. A rule that silently withholds
/// results is indistinguishable from a broken sync, and an accidental delete
/// would otherwise be permanent with no way to find out why.
pub fn list_dismissed(conn: &Connection, limit: i64) -> Result<Vec<Dismissed>> {
    let mut stmt = conn.prepare(
        "SELECT remote_url, page_url, title, dismissed_at
           FROM dismissed ORDER BY dismissed_at DESC, remote_url LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(Dismissed {
            remote_url: r.get(0)?,
            page_url: r.get(1)?,
            title: r.get(2)?,
            dismissed_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Stops skipping some references, or all of them when `urls` is empty.
///
/// Undoing a tombstone does not restore anything by itself; it makes the next
/// sync offer the reference again, which is the only sense in which a deleted
/// reference can come back.
pub fn undismiss(conn: &mut Connection, urls: &[String]) -> Result<usize> {
    if urls.is_empty() {
        return Ok(conn.execute("DELETE FROM dismissed", [])?);
    }
    let tx = conn.transaction()?;
    let mut removed = 0usize;
    {
        let mut stmt = tx.prepare("DELETE FROM dismissed WHERE remote_url = ?1")?;
        for url in urls {
            removed += stmt.execute([url])?;
        }
    }
    tx.commit()?;
    Ok(removed)
}

/// Most recently imported first.
pub fn list_assets(
    lib: &Library,
    conn: &Connection,
    limit: i64,
    offset: i64,
) -> Result<Vec<AssetRow>> {
    let mut stmt = conn.prepare(&format!(
        "{ASSET_COLUMNS} ORDER BY imported_at DESC, id DESC LIMIT ?1 OFFSET ?2"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![limit, offset], |r| row_to_asset(lib, r))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = rows;
    for asset in &mut out {
        asset.swatches = swatches_for(conn, asset.id)?;
    }
    Ok(out)
}

/// Column list every asset query selects, so `row_to_asset` can read them all
/// positionally. Kept in one place because the indices below depend on it.
pub(crate) const ASSET_COLUMNS: &str =
    "SELECT id, hash, kind, duration_ms, ext, mime, width, height, bytes,
            original_name, source_url, imported_at, state, remote_url, note
     FROM assets";

pub(crate) fn row_to_asset(lib: &Library, r: &rusqlite::Row) -> rusqlite::Result<AssetRow> {
    let hash: String = r.get(1)?;
    let ext: String = r.get(4)?;
    Ok(AssetRow {
        id: r.get(0)?,
        thumb_path: lib.thumb_path(&hash).display().to_string(),
        blob_path: lib.blob_path(&hash, &ext).display().to_string(),
        hash,
        kind: MediaKind::from_str(&r.get::<_, String>(2)?),
        duration_ms: r.get(3)?,
        ext,
        mime: r.get(5)?,
        width: r.get(6)?,
        height: r.get(7)?,
        bytes: r.get(8)?,
        original_name: r.get(9)?,
        source_url: r.get(10)?,
        imported_at: r.get(11)?,
        state: AssetState::from_str(&r.get::<_, String>(12)?),
        remote_url: r.get(13)?,
        note: r.get(14)?,
        swatches: Vec::new(),
    })
}

fn swatches_for(conn: &Connection, asset_id: i64) -> Result<Vec<Swatch>> {
    let mut stmt = conn.prepare(
        "SELECT weight, l, a, b, hex FROM swatches WHERE asset_id = ?1 ORDER BY ordinal",
    )?;
    let rows = stmt
        .query_map([asset_id], |r| {
            Ok(Swatch {
                weight: r.get(0)?,
                l: r.get(1)?,
                a: r.get(2)?,
                b: r.get(3)?,
                hex: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, Serialize)]
pub struct ColorMatch {
    pub asset: AssetRow,
    /// OkLab distance from the query colour to the closest swatch.
    pub distance: f32,
}

/// Finds assets with a swatch within `tolerance` OkLab units of `hex`.
///
/// SQL prefilters on a lightness band -- the only axis with an index -- and the
/// exact distance is computed in Rust. Doing the whole thing in SQL would need
/// a sqrt over three columns per row, which SQLite cannot index anyway.
pub fn search_by_color(
    lib: &Library,
    conn: &Connection,
    hex: &str,
    tolerance: f32,
    limit: usize,
) -> Result<Vec<ColorMatch>> {
    let Some((r, g, b)) = crate::color::parse_hex(hex) else {
        return Ok(Vec::new());
    };
    let target = crate::color::srgb_to_oklab(r, g, b);

    let mut stmt = conn.prepare(
        "SELECT asset_id, l, a, b FROM swatches
         WHERE l BETWEEN ?1 AND ?2",
    )?;
    let candidates = stmt
        .query_map(
            rusqlite::params![target.l - tolerance, target.l + tolerance],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    crate::color::Oklab {
                        l: row.get(1)?,
                        a: row.get(2)?,
                        b: row.get(3)?,
                    },
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Keep the best-matching swatch per asset; an image with three near-misses
    // should rank once, by its closest, not three times.
    let mut best: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
    for (asset_id, swatch) in candidates {
        let dl = swatch.l - target.l;
        let da = swatch.a - target.a;
        let db = swatch.b - target.b;
        let distance = (dl * dl + da * da + db * db).sqrt();
        if distance > tolerance {
            continue;
        }
        best.entry(asset_id)
            .and_modify(|d| {
                if distance < *d {
                    *d = distance;
                }
            })
            .or_insert(distance);
    }

    let mut ranked: Vec<(i64, f32)> = best.into_iter().collect();
    ranked.sort_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
    ranked.truncate(limit);

    let mut out = Vec::with_capacity(ranked.len());
    for (asset_id, distance) in ranked {
        if let Some(asset) = asset_by_id(lib, conn, asset_id)? {
            out.push(ColorMatch { asset, distance });
        }
    }
    Ok(out)
}

// --- notes ---

/// Longest note kept. Generous for a caption and far short of a document; the
/// cap exists so a stray paste cannot put a megabyte into every grid query,
/// since notes ride along on every row the UI reads.
pub const MAX_NOTE_LEN: usize = 4000;

/// Writes (or clears) a reference's note. Returns the stored value.
///
/// Blank input clears rather than storing `""`. Otherwise "cleared the note"
/// and "never wrote one" become two states that render identically but compare
/// differently, and every later `IS NULL` check has to remember both.
pub fn set_note(conn: &Connection, asset_id: i64, note: &str) -> Result<Option<String>> {
    let trimmed = note.trim();
    let stored: Option<String> = if trimmed.is_empty() {
        None
    } else {
        // Truncated on a character boundary; slicing bytes would panic on the
        // first multi-byte character, and notes are free text.
        Some(trimmed.chars().take(MAX_NOTE_LEN).collect())
    };

    let changed = conn.execute(
        "UPDATE assets SET note = ?1 WHERE id = ?2",
        rusqlite::params![stored, asset_id],
    )?;
    if changed == 0 {
        return Err(Error::Link(format!("no reference with id {asset_id}")));
    }
    Ok(stored)
}

/// References whose note contains `query`, newest first.
///
/// Case-insensitive substring rather than full-text: notes are short and the
/// library is one person's, so a scan is immeasurably fast and behaves the way
/// people expect from a search box -- a partial word matches. FTS5 would need
/// token-boundary matching and an index to maintain, for no gain at this size.
pub fn search_notes(
    lib: &Library,
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<AssetRow>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // `%` and `_` are wildcards in LIKE. A user typing them means the literal
    // characters, so they are escaped rather than silently widening the search.
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");

    let mut stmt = conn.prepare(&format!(
        "{ASSET_COLUMNS} WHERE note IS NOT NULL AND note LIKE ?1 ESCAPE '\\'
         ORDER BY imported_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![pattern, limit], |r| row_to_asset(lib, r))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = rows;
    for asset in &mut out {
        asset.swatches = swatches_for(conn, asset.id)?;
    }
    Ok(out)
}

pub(crate) fn asset_by_id(lib: &Library, conn: &Connection, id: i64) -> Result<Option<AssetRow>> {
    let mut stmt = conn.prepare(&format!("{ASSET_COLUMNS} WHERE id = ?1"))?;
    let mut rows = stmt.query([id])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let mut asset = row_to_asset(lib, r)?;
    drop(rows);
    asset.swatches = swatches_for(conn, id)?;
    Ok(Some(asset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba, RgbaImage};

    struct Fixture {
        lib: Library,
        conn: Connection,
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("burrow-ingest-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).expect("create fixture dir");
            let lib = Library::open(dir.join("lib")).expect("open library");
            let conn = crate::db::open(&lib.db_path()).expect("open db");
            Self { lib, conn, dir }
        }

        /// Writes a solid-colour PNG and returns its path.
        fn write_png(&self, name: &str, w: u32, h: u32, rgba: [u8; 4]) -> PathBuf {
            let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(rgba)));
            let path = self.dir.join("src").join(name);
            let mut buf = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut buf, image::ImageFormat::Png)
                .expect("encode");
            std::fs::write(&path, buf.into_inner()).expect("write png");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Encodes a solid-colour PNG in memory, standing in for a fetched poster.
    fn png_bytes(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(w, h, Rgba(rgba)));
        let mut buf = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode");
        buf.into_inner()
    }

    fn pending(media_url: &str, kind: MediaKind, rgba: [u8; 4]) -> PendingLink {
        PendingLink {
            page_url: "https://x.com/someone/status/1".into(),
            media_url: media_url.into(),
            kind,
            title: Some("a reference".into()),
            thumbnail: png_bytes(64, 36, rgba),
        }
    }

    #[test]
    fn a_local_import_records_its_content_hash() {
        // The v4 backfill only reached rows that already existed, so every new
        // insert has to write content_hash itself. Without it, downloading a
        // link can never recognise bytes the library already holds.
        let mut fx = Fixture::new("content-hash");
        let path = fx.write_png("a.png", 8, 8, [1, 2, 3, 255]);
        let report = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");

        let hash = &report.imported[0].hash;
        let stored: Option<String> = fx
            .conn
            .query_row(
                "SELECT content_hash FROM assets WHERE hash = ?1",
                [hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_ref(), Some(hash));
    }

    #[test]
    fn a_link_is_stored_as_a_thumbnail_and_a_url_with_no_blob() {
        let mut fx = Fixture::new("link-basic");
        let url = "https://video.twimg.com/clip.mp4";
        let report = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(url, MediaKind::Video, [10, 200, 90, 255])],
        )
        .expect("import link");

        assert_eq!(report.imported.len(), 1);
        let a = &report.imported[0];
        assert_eq!(a.state, AssetState::Linked);
        assert_eq!(a.remote_url.as_deref(), Some(url));
        assert_eq!(
            a.bytes, 0,
            "a link stores no media, and should not claim to"
        );
        assert_eq!(a.duration_ms, None, "nothing has read the file yet");

        // The thumbnail is real and on disk; the blob deliberately is not.
        assert!(Path::new(&a.thumb_path).is_file(), "no thumbnail written");
        assert!(
            !Path::new(&a.blob_path).exists(),
            "a link must not create a blob"
        );

        // The palette is what makes colour search cover linked references, so
        // its absence would be a silent feature regression.
        assert!(
            !a.swatches.is_empty(),
            "no palette extracted from the poster"
        );
    }

    #[test]
    fn the_same_link_twice_is_a_duplicate_not_a_second_tile() {
        let mut fx = Fixture::new("link-dupe");
        let url = "https://video.twimg.com/same.mp4";

        // Once in a single batch...
        let report = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![
                pending(url, MediaKind::Video, [1, 2, 3, 255]),
                pending(url, MediaKind::Video, [1, 2, 3, 255]),
            ],
        )
        .expect("import");
        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.duplicates, 1);

        // ...and again against what is already committed.
        let again = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(url, MediaKind::Video, [1, 2, 3, 255])],
        )
        .expect("import");
        assert!(again.imported.is_empty());
        assert_eq!(again.duplicates, 1);
    }

    #[test]
    fn a_deleted_reference_does_not_come_back_on_the_next_sync() {
        // Otherwise "delete" means "delete until you sync again", which is not
        // what anybody means by delete.
        let mut fx = Fixture::new("dismiss-resync");
        let url = "https://video.twimg.com/gone.mp4";

        let first = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(url, MediaKind::Video, [9, 9, 9, 255])],
        )
        .expect("import");
        let id = first.imported[0].id;

        let deleted = delete_assets(&fx.lib, &mut fx.conn, &[id]).expect("delete");
        assert_eq!(deleted.deleted, 1);
        assert_eq!(deleted.dismissed, 1, "the URL was not remembered");

        // The same bookmark comes round again on the next sync.
        let second = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(url, MediaKind::Video, [9, 9, 9, 255])],
        )
        .expect("import");
        assert!(second.imported.is_empty(), "a deleted reference came back");
        assert_eq!(
            second.dismissed, 1,
            "the skip must be counted, not silent -- an invisible skip is \
             indistinguishable from a sync that lost things"
        );
        assert_eq!(second.duplicates, 0, "a tombstone is not a duplicate");

        // And it can be taken back, or the first misclick is permanent.
        let listed = list_dismissed(&fx.conn, 10).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].remote_url, url);
        assert_eq!(undismiss(&mut fx.conn, &[url.to_string()]).unwrap(), 1);

        let third = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(url, MediaKind::Video, [9, 9, 9, 255])],
        )
        .expect("import");
        assert_eq!(third.imported.len(), 1, "undismiss did not restore syncing");
    }

    #[test]
    fn deleting_a_local_import_leaves_no_tombstone() {
        // A dragged-in file has no remote identity, and dragging it in again is
        // a deliberate act that must simply work.
        let mut fx = Fixture::new("dismiss-local");
        let path = fx.write_png("local.png", 8, 8, [4, 4, 4, 255]);
        let imported =
            import_paths(&fx.lib, &mut fx.conn, std::slice::from_ref(&path)).expect("import");
        let id = imported.imported[0].id;

        let report = delete_assets(&fx.lib, &mut fx.conn, &[id]).expect("delete");
        assert_eq!(report.deleted, 1);
        assert_eq!(report.dismissed, 0);
        assert!(list_dismissed(&fx.conn, 10).unwrap().is_empty());

        let again = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("re-import");
        assert_eq!(again.imported.len(), 1, "a re-added file was refused");
    }

    #[test]
    fn colour_search_reaches_linked_references() {
        // The reason a link stores a thumbnail at all rather than just a URL.
        let mut fx = Fixture::new("link-colour");
        import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/green.mp4",
                MediaKind::Video,
                [20, 200, 60, 255],
            )],
        )
        .expect("import");

        let hits = search_by_color(&fx.lib, &fx.conn, "#14c83c", 0.12, 10).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "a linked reference did not match its own colour"
        );
        assert_eq!(hits[0].asset.state, AssetState::Linked);
    }

    #[test]
    fn downloading_a_link_makes_it_local_with_real_metadata() {
        let Some(source) = write_video(&Fixture::new("probe").dir.clone(), "src.mp4", 3) else {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        };
        let mut fx = Fixture::new("link-download");
        let real = fx.dir.join("real.mp4");
        std::fs::copy(&source, &real).expect("stage source");
        let _ = std::fs::remove_file(&source);

        let report = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/real.mp4",
                MediaKind::Video,
                [90, 90, 90, 255],
            )],
        )
        .expect("import link");
        let id = report.imported[0].id;

        // Stands in for the network: copies the staged file into place.
        let done = download_assets(
            &fx.lib,
            &mut fx.conn,
            &[id],
            |_url, dest| std::fs::copy(&real, dest).map_err(|e| Error::io(dest, e)),
            |_, _, _| {},
        )
        .expect("download");

        assert_eq!(done.failed.len(), 0, "{:?}", done.failed);
        assert_eq!(done.downloaded.len(), 1);
        let a = &done.downloaded[0];
        assert_eq!(a.state, AssetState::Local);
        assert!(Path::new(&a.blob_path).is_file(), "blob was not written");
        assert!(a.bytes > 0, "size still reads as a link's zero");
        assert!(
            a.duration_ms.unwrap_or(0) > 0,
            "duration should be known once the file is real"
        );
        // The URL is kept. `state` is what says the bytes are held locally;
        // the URL is what lets a later sync recognise this bookmark as already
        // in the library. Clearing it meant downloading a reference caused the
        // next sync to add it back as a second, linked copy of the same thing.
        assert_eq!(
            a.remote_url.as_deref(),
            Some("https://video.twimg.com/real.mp4"),
            "a downloaded reference must still know where it came from"
        );

        // No staging file left beside the blob.
        assert!(!Path::new(&a.blob_path)
            .with_extension("downloading")
            .exists());
    }

    #[test]
    fn downloading_bytes_the_library_already_holds_dedupes_and_keeps_the_board() {
        let fx0 = Fixture::new("dedupe-src");
        let Some(source) = write_video(&fx0.dir.clone(), "src.mp4", 3) else {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        };
        let mut fx = Fixture::new("link-dedupe");
        let real = fx.dir.join("real.mp4");
        std::fs::copy(&source, &real).expect("stage");

        // Already in the library the ordinary way.
        let local =
            import_paths(&fx.lib, &mut fx.conn, std::slice::from_ref(&real)).expect("import");
        let local_id = local.imported[0].id;

        // The same footage arrives again as a link, and gets put on a board.
        let linked = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/same-footage.mp4",
                MediaKind::Video,
                [5, 5, 5, 255],
            )],
        )
        .expect("import link");
        let link_id = linked.imported[0].id;

        let board = crate::boards::create_board(&fx.conn, "Refs").expect("board");
        crate::boards::add_to_board(&mut fx.conn, board.id, &[link_id]).expect("add");

        let done = download_assets(
            &fx.lib,
            &mut fx.conn,
            &[link_id],
            |_url, dest| std::fs::copy(&real, dest).map_err(|e| Error::io(dest, e)),
            |_, _, _| {},
        )
        .expect("download");

        assert_eq!(done.deduplicated, 1, "identical bytes imported twice");
        assert!(done.downloaded.is_empty());

        // The link row is gone...
        let still: i64 = fx
            .conn
            .query_row(
                "SELECT count(*) FROM assets WHERE id = ?1",
                [link_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, 0);

        // ...and the board points at the copy that has the bytes, rather than
        // having quietly lost its item.
        let on_board: Vec<i64> = fx
            .conn
            .prepare("SELECT asset_id FROM board_items WHERE board_id = ?1")
            .unwrap()
            .query_map([board.id], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            on_board,
            vec![local_id],
            "board membership was not transferred"
        );
    }

    #[test]
    fn a_failed_download_leaves_the_reference_linked_and_no_debris() {
        let mut fx = Fixture::new("link-fail");
        let report = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/gone.mp4",
                MediaKind::Video,
                [7, 7, 7, 255],
            )],
        )
        .expect("import link");
        let id = report.imported[0].id;

        let done = download_assets(
            &fx.lib,
            &mut fx.conn,
            &[id],
            |_url, dest| {
                // Write something, then fail: the half-written file must not
                // survive to be mistaken for a blob.
                std::fs::write(dest, b"partial").ok();
                Err(Error::Link("host is unreachable".into()))
            },
            |_, _, _| {},
        )
        .expect("download");

        assert_eq!(done.failed.len(), 1);
        assert_eq!(done.downloaded.len(), 0);

        let state: String = fx
            .conn
            .query_row("SELECT state FROM assets WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "linked", "a failed download must not orphan the row");

        let blob = fx.lib.blob_path(&report.imported[0].hash, "mp4");
        assert!(!blob.exists());
        assert!(
            !blob.with_extension("downloading").exists(),
            "a partial download was left behind"
        );
    }

    #[test]
    fn a_link_that_serves_an_error_page_is_reported_not_stored() {
        let mut fx = Fixture::new("link-html");
        let report = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/nope.mp4",
                MediaKind::Video,
                [9, 9, 9, 255],
            )],
        )
        .expect("import link");
        let id = report.imported[0].id;

        let done = download_assets(
            &fx.lib,
            &mut fx.conn,
            &[id],
            |_url, dest| {
                std::fs::write(dest, b"<html><body>404</body></html>")
                    .map(|_| 29u64)
                    .map_err(|e| Error::io(dest, e))
            },
            |_, _, _| {},
        )
        .expect("download");

        assert_eq!(done.downloaded.len(), 0);
        assert_eq!(done.failed.len(), 1);
        assert!(
            done.failed[0]
                .reason
                .contains("neither an image nor a video"),
            "unhelpful message: {}",
            done.failed[0].reason
        );
    }

    #[test]
    fn a_note_round_trips_and_clears_to_null() {
        let mut fx = Fixture::new("note-basic");
        let path = fx.write_png("a.png", 8, 8, [1, 2, 3, 255]);
        let id = import_paths(&fx.lib, &mut fx.conn, &[path])
            .unwrap()
            .imported[0]
            .id;

        set_note(&fx.conn, id, "  use this grain  ").expect("set");
        let asset = asset_by_id(&fx.lib, &fx.conn, id).unwrap().unwrap();
        assert_eq!(
            asset.note.as_deref(),
            Some("use this grain"),
            "surrounding whitespace should not be stored"
        );

        // Blanking must clear rather than store "", so that "cleared" and
        // "never set" are one state rather than two that look alike.
        set_note(&fx.conn, id, "   ").expect("clear");
        let asset = asset_by_id(&fx.lib, &fx.conn, id).unwrap().unwrap();
        assert_eq!(asset.note, None);
    }

    #[test]
    fn a_note_survives_on_a_linked_reference_and_after_downloading() {
        let fx0 = Fixture::new("note-dl-src");
        let Some(source) = write_video(&fx0.dir.clone(), "src.mp4", 3) else {
            eprintln!("ffmpeg unavailable; skipping");
            return;
        };
        let mut fx = Fixture::new("note-download");
        let real = fx.dir.join("real.mp4");
        std::fs::copy(&source, &real).unwrap();

        let id = import_links(
            &fx.lib,
            &mut fx.conn,
            vec![pending(
                "https://video.twimg.com/noted.mp4",
                MediaKind::Video,
                [3, 3, 3, 255],
            )],
        )
        .unwrap()
        .imported[0]
            .id;

        set_note(&fx.conn, id, "opening shot reference").unwrap();

        let done = download_assets(
            &fx.lib,
            &mut fx.conn,
            &[id],
            |_url, dest| std::fs::copy(&real, dest).map_err(|e| Error::io(dest, e)),
            |_, _, _| {},
        )
        .expect("download");

        assert_eq!(done.downloaded.len(), 1, "{:?}", done.failed);
        // Downloading rewrites most of the row; the note is the user's and must
        // not be collateral damage.
        assert_eq!(
            done.downloaded[0].note.as_deref(),
            Some("opening shot reference")
        );
    }

    #[test]
    fn notes_are_searchable_by_substring() {
        let mut fx = Fixture::new("note-search");
        let a = fx.write_png("a.png", 8, 8, [10, 0, 0, 255]);
        let b = fx.write_png("b.png", 8, 8, [0, 10, 0, 255]);
        let c = fx.write_png("c.png", 8, 8, [0, 0, 10, 255]);
        let r = import_paths(&fx.lib, &mut fx.conn, &[a, b, c]).unwrap();

        set_note(&fx.conn, r.imported[0].id, "Bauhaus stairwell").unwrap();
        set_note(&fx.conn, r.imported[1].id, "brutalist CONCRETE").unwrap();
        // Third gets no note and must never appear.

        assert_eq!(
            search_notes(&fx.lib, &fx.conn, "bauhaus", 50)
                .unwrap()
                .len(),
            1
        );
        // Case-insensitive, and matching mid-word is the point of substring.
        assert_eq!(
            search_notes(&fx.lib, &fx.conn, "concrete", 50)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_notes(&fx.lib, &fx.conn, "stair", 50).unwrap().len(),
            1
        );
        assert_eq!(
            search_notes(&fx.lib, &fx.conn, "zebra", 50).unwrap().len(),
            0
        );
        // An empty query is not "match everything".
        assert_eq!(search_notes(&fx.lib, &fx.conn, "  ", 50).unwrap().len(), 0);
    }

    #[test]
    fn like_wildcards_typed_by_a_user_are_literal() {
        let mut fx = Fixture::new("note-wildcards");
        let a = fx.write_png("a.png", 8, 8, [10, 0, 0, 255]);
        let b = fx.write_png("b.png", 8, 8, [0, 10, 0, 255]);
        let r = import_paths(&fx.lib, &mut fx.conn, &[a, b]).unwrap();
        set_note(&fx.conn, r.imported[0].id, "grade 100% matched").unwrap();
        set_note(&fx.conn, r.imported[1].id, "no percent here").unwrap();

        // Unescaped, "%" is LIKE's match-anything and would return both.
        let hits = search_notes(&fx.lib, &fx.conn, "100%", 50).unwrap();
        assert_eq!(hits.len(), 1, "% was treated as a wildcard");
        assert_eq!(hits[0].note.as_deref(), Some("grade 100% matched"));

        // Same for "_", which matches any single character.
        assert_eq!(search_notes(&fx.lib, &fx.conn, "_", 50).unwrap().len(), 0);
    }

    #[test]
    fn a_note_on_a_missing_reference_is_an_error_not_a_silent_no_op() {
        let fx = Fixture::new("note-missing");
        assert!(set_note(&fx.conn, 9999, "hello").is_err());
    }

    #[test]
    fn an_overlong_note_is_truncated_on_a_character_boundary() {
        let mut fx = Fixture::new("note-long");
        let path = fx.write_png("a.png", 8, 8, [1, 2, 3, 255]);
        let id = import_paths(&fx.lib, &mut fx.conn, &[path])
            .unwrap()
            .imported[0]
            .id;

        // Multi-byte throughout: byte slicing here would panic.
        let huge = "é".repeat(MAX_NOTE_LEN + 500);
        let stored = set_note(&fx.conn, id, &huge).expect("set");
        assert_eq!(stored.unwrap().chars().count(), MAX_NOTE_LEN);
    }

    #[test]
    fn extensions_come_from_the_url_and_fall_back_by_kind() {
        assert_eq!(
            ext_from_url("https://a/b/c.mp4?tag=29", MediaKind::Video),
            "mp4"
        );
        assert_eq!(ext_from_url("https://a/b/c.WEBM", MediaKind::Video), "webm");
        assert_eq!(ext_from_url("https://a/b/c.png", MediaKind::Image), "png");
        // A player page or extensionless CDN path is normal, not an error.
        assert_eq!(
            ext_from_url("https://youtube.com/watch?v=x", MediaKind::Video),
            "mp4"
        );
        assert_eq!(ext_from_url("https://a/image", MediaKind::Image), "jpg");
        // An extension that does not match the kind must not be believed.
        assert_eq!(ext_from_url("https://a/page.html", MediaKind::Video), "mp4");
    }

    #[test]
    fn imports_an_image_with_blob_thumb_and_palette() {
        let mut fx = Fixture::new("basic");
        let path = fx.write_png("red.png", 40, 20, [255, 0, 0, 255]);

        let report = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");

        assert_eq!(report.imported.len(), 1);
        assert_eq!(report.duplicates, 0);
        assert!(report.failed.is_empty(), "{:?}", report.failed);

        let asset = &report.imported[0];
        assert_eq!((asset.width, asset.height), (40, 20));
        assert_eq!(asset.ext, "png");
        assert_eq!(asset.mime, "image/png");
        assert_eq!(asset.original_name.as_deref(), Some("red.png"));

        // Solid red image -> exactly one swatch, and it should be red.
        assert_eq!(asset.swatches.len(), 1, "{:?}", asset.swatches);
        assert_eq!(asset.swatches[0].hex, "#ff0000");

        assert!(
            fx.lib.blob_path(&asset.hash, "png").exists(),
            "blob missing"
        );
        assert!(fx.lib.thumb_path(&asset.hash).exists(), "thumb missing");
    }

    #[test]
    fn reimporting_identical_bytes_is_a_duplicate() {
        let mut fx = Fixture::new("dupe");
        let path = fx.write_png("blue.png", 10, 10, [0, 0, 255, 255]);

        let first =
            import_paths(&fx.lib, &mut fx.conn, std::slice::from_ref(&path)).expect("first");
        assert_eq!(first.imported.len(), 1);

        let second = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("second");
        assert_eq!(second.imported.len(), 0);
        assert_eq!(second.duplicates, 1);

        let count: i64 = fx
            .conn
            .query_row("SELECT count(*) FROM assets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn duplicate_within_one_batch_is_caught() {
        let mut fx = Fixture::new("intra-batch");
        // Same pixels, different filenames -> same digest.
        let a = fx.write_png("a.png", 12, 12, [7, 7, 7, 255]);
        let b = fx.write_png("b.png", 12, 12, [7, 7, 7, 255]);

        let report = import_paths(&fx.lib, &mut fx.conn, &[a, b]).expect("import");
        assert_eq!(
            report.imported.len(),
            1,
            "content-identical files must collapse"
        );
        assert_eq!(report.duplicates, 1);
    }

    #[test]
    fn a_bad_file_does_not_abort_the_batch() {
        let mut fx = Fixture::new("partial-failure");
        let good = fx.write_png("good.png", 8, 8, [1, 2, 3, 255]);

        let junk = fx.dir.join("src").join("junk.png");
        std::fs::write(&junk, b"definitely not an image").unwrap();

        let missing = fx.dir.join("src").join("does-not-exist.png");

        let report = import_paths(&fx.lib, &mut fx.conn, &[good, junk, missing]).expect("import");

        assert_eq!(report.imported.len(), 1, "the good file should still land");
        assert_eq!(report.failed.len(), 2, "{:?}", report.failed);
        assert!(report.failed.iter().any(|f| f.path.contains("junk.png")));
        assert!(report
            .failed
            .iter()
            .any(|f| f.path.contains("does-not-exist.png")));
    }

    #[test]
    fn dropping_a_folder_imports_the_images_inside_it() {
        let mut fx = Fixture::new("folder-drop");
        fx.write_png("a.png", 8, 8, [255, 0, 0, 255]);
        fx.write_png("b.png", 8, 8, [0, 255, 0, 255]);

        // The OS hands over the folder itself, not its contents.
        let folder = fx.dir.join("src");
        let report = import_paths(&fx.lib, &mut fx.conn, &[folder]).expect("import");

        assert_eq!(report.imported.len(), 2, "{:?}", report.failed);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }

    #[test]
    fn folder_walk_recurses_and_ignores_non_images() {
        let mut fx = Fixture::new("folder-walk");
        fx.write_png("top.png", 8, 8, [1, 1, 1, 255]);

        let nested = fx.dir.join("src").join("deep").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([9, 9, 9, 255])));
        let mut buf = std::io::Cursor::new(Vec::new());
        image.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        std::fs::write(nested.join("buried.png"), buf.into_inner()).unwrap();

        // Non-images in a walked folder are skipped silently, not reported as
        // failures -- otherwise dropping a project folder buries the result.
        std::fs::write(fx.dir.join("src").join("notes.txt"), b"not an image").unwrap();
        std::fs::write(fx.dir.join("src").join("layers.psd"), b"nope").unwrap();

        let report = import_paths(&fx.lib, &mut fx.conn, &[fx.dir.join("src")]).expect("import");

        assert_eq!(
            report.imported.len(),
            2,
            "should find top.png and buried.png"
        );
        assert!(
            report.failed.is_empty(),
            "non-images in a walked folder must not surface as failures: {:?}",
            report.failed
        );
    }

    #[test]
    fn an_explicit_non_image_file_still_reports_a_failure() {
        let mut fx = Fixture::new("explicit-non-image");
        let junk = fx.dir.join("src").join("notes.txt");
        std::fs::write(&junk, b"not an image").unwrap();

        // Extension filtering applies only to files *discovered* in a folder.
        // Something the user pointed at directly deserves an explanation.
        let report = import_paths(&fx.lib, &mut fx.conn, &[junk]).expect("import");
        assert_eq!(report.imported.len(), 0);
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    }

    #[test]
    fn an_empty_folder_is_a_no_op() {
        let mut fx = Fixture::new("empty-folder");
        let empty = fx.dir.join("nothing");
        std::fs::create_dir_all(&empty).unwrap();

        let report = import_paths(&fx.lib, &mut fx.conn, &[empty]).expect("import");
        assert_eq!(report.imported.len(), 0);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }

    /// Renders a real clip so video ingest is exercised end to end rather than
    /// against a stub. Returns None when ffmpeg is unavailable.
    fn write_video(dir: &Path, name: &str, seconds: u32) -> Option<PathBuf> {
        if !crate::video::tooling_available() {
            return None;
        }
        let path = dir.join(name);
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=640x360:rate=15:duration={seconds}"),
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .ok()?;
        ok.success().then_some(path)
    }

    #[test]
    fn importing_a_video_stores_it_with_a_poster_thumbnail() {
        let mut fx = Fixture::new("video-basic");
        let Some(path) = write_video(&fx.dir.join("src"), "clip.mp4", 3) else {
            eprintln!("ffmpeg unavailable, skipping");
            return;
        };

        let report = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");
        assert_eq!(report.imported.len(), 1, "{:?}", report.failed);

        let asset = &report.imported[0];
        assert_eq!(asset.kind, MediaKind::Video);
        assert_eq!((asset.width, asset.height), (640, 360));
        assert_eq!(asset.ext, "mp4");
        assert_eq!(asset.mime, "video/mp4");

        let duration = asset.duration_ms.expect("video must carry a duration");
        assert!(
            (duration - 3000).abs() < 400,
            "expected ~3000ms, got {duration}"
        );

        // The original is copied, and a poster thumbnail exists beside it.
        assert!(
            fx.lib.blob_path(&asset.hash, "mp4").exists(),
            "blob missing"
        );
        assert!(fx.lib.thumb_path(&asset.hash).exists(), "thumb missing");

        // Colour search must work on video, which means the poster frame went
        // through the same palette path as a still -- and was not black.
        assert!(!asset.swatches.is_empty(), "video produced no palette");
        assert!(
            asset.swatches.iter().any(|s| s.l > 0.15),
            "palette is all near-black, so the poster frame was probably frame 0: {:?}",
            asset.swatches
        );
    }

    #[test]
    fn images_and_video_import_in_one_batch() {
        let mut fx = Fixture::new("mixed-batch");
        let still = fx.write_png("shot.png", 20, 20, [200, 40, 40, 255]);
        let Some(clip) = write_video(&fx.dir.join("src"), "clip.mp4", 2) else {
            eprintln!("ffmpeg unavailable, skipping");
            return;
        };

        let report = import_paths(&fx.lib, &mut fx.conn, &[still, clip]).expect("import");
        assert_eq!(report.imported.len(), 2, "{:?}", report.failed);

        let kinds: Vec<MediaKind> = report.imported.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&MediaKind::Image));
        assert!(kinds.contains(&MediaKind::Video));
    }

    #[test]
    fn a_video_dropped_twice_is_a_duplicate() {
        let mut fx = Fixture::new("video-dupe");
        let Some(path) = write_video(&fx.dir.join("src"), "clip.mp4", 2) else {
            return;
        };

        assert_eq!(
            import_paths(&fx.lib, &mut fx.conn, std::slice::from_ref(&path))
                .unwrap()
                .imported
                .len(),
            1
        );
        let second = import_paths(&fx.lib, &mut fx.conn, &[path]).unwrap();
        assert_eq!(second.imported.len(), 0);
        assert_eq!(
            second.duplicates, 1,
            "content addressing must cover video too"
        );
    }

    #[test]
    fn kind_and_duration_survive_a_round_trip_through_the_database() {
        let mut fx = Fixture::new("video-roundtrip");
        let Some(path) = write_video(&fx.dir.join("src"), "clip.mp4", 2) else {
            return;
        };
        import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");

        // Re-read through list_assets rather than trusting the insert-time row.
        let listed = list_assets(&fx.lib, &fx.conn, 10, 0).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, MediaKind::Video);
        assert!(listed[0].duration_ms.unwrap_or(0) > 0);
    }

    #[test]
    fn a_folder_of_mixed_media_walks_both_kinds() {
        let mut fx = Fixture::new("video-walk");
        fx.write_png("a.png", 8, 8, [3, 3, 3, 255]);
        if write_video(&fx.dir.join("src"), "b.mov", 2).is_none() {
            return;
        }
        std::fs::write(fx.dir.join("src").join("readme.txt"), b"ignore me").unwrap();

        let report = import_paths(&fx.lib, &mut fx.conn, &[fx.dir.join("src")]).expect("import");
        assert_eq!(report.imported.len(), 2, "{:?}", report.failed);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
    }

    #[test]
    fn deleting_removes_the_row_the_blob_and_the_thumbnail() {
        let mut fx = Fixture::new("delete");
        let path = fx.write_png("doomed.png", 12, 12, [90, 20, 20, 255]);
        let report = import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");
        let asset = report.imported[0].clone();

        let blob = fx.lib.blob_path(&asset.hash, "png");
        let thumb = fx.lib.thumb_path(&asset.hash);
        assert!(blob.exists() && thumb.exists());

        let del = delete_assets(&fx.lib, &mut fx.conn, &[asset.id]).expect("delete");
        assert_eq!(del.deleted, 1);
        assert!(del.orphaned_files.is_empty(), "{:?}", del.orphaned_files);
        assert!(del.bytes_freed > 0);

        assert!(!blob.exists(), "blob survived deletion");
        assert!(!thumb.exists(), "thumbnail survived deletion");
        assert!(list_assets(&fx.lib, &fx.conn, 10, 0).unwrap().is_empty());
    }

    #[test]
    fn deleting_cascades_to_swatches_and_board_membership() {
        let mut fx = Fixture::new("delete-cascade");
        let path = fx.write_png("tracked.png", 10, 10, [10, 200, 10, 255]);
        let asset = import_paths(&fx.lib, &mut fx.conn, &[path])
            .unwrap()
            .imported[0]
            .clone();

        let board = crate::boards::create_board(&fx.conn, "Refs").unwrap();
        crate::boards::add_to_board(&mut fx.conn, board.id, &[asset.id]).unwrap();

        delete_assets(&fx.lib, &mut fx.conn, &[asset.id]).expect("delete");

        let swatches: i64 = fx
            .conn
            .query_row("SELECT count(*) FROM swatches", [], |r| r.get(0))
            .unwrap();
        let items: i64 = fx
            .conn
            .query_row("SELECT count(*) FROM board_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(swatches, 0, "orphaned swatches left behind");
        assert_eq!(items, 0, "orphaned board membership left behind");
    }

    #[test]
    fn deleting_one_asset_leaves_the_others_intact() {
        let mut fx = Fixture::new("delete-partial");
        let a = fx.write_png("keep.png", 8, 8, [1, 2, 3, 255]);
        let b = fx.write_png("drop.png", 8, 8, [200, 100, 50, 255]);
        let imported = import_paths(&fx.lib, &mut fx.conn, &[a, b])
            .unwrap()
            .imported;

        let keep = imported
            .iter()
            .find(|x| x.original_name.as_deref() == Some("keep.png"))
            .unwrap()
            .clone();
        let drop = imported
            .iter()
            .find(|x| x.original_name.as_deref() == Some("drop.png"))
            .unwrap()
            .clone();

        delete_assets(&fx.lib, &mut fx.conn, &[drop.id]).expect("delete");

        let left = list_assets(&fx.lib, &fx.conn, 10, 0).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, keep.id);
        assert!(
            fx.lib.blob_path(&keep.hash, "png").exists(),
            "deleting one asset unlinked another's blob"
        );
    }

    #[test]
    fn deleting_then_reimporting_the_same_file_works() {
        let mut fx = Fixture::new("delete-reimport");
        let path = fx.write_png("again.png", 9, 9, [30, 60, 90, 255]);

        let first = import_paths(&fx.lib, &mut fx.conn, std::slice::from_ref(&path)).unwrap();
        delete_assets(&fx.lib, &mut fx.conn, &[first.imported[0].id]).unwrap();

        // The hash is free again, so this must import rather than dedupe to a
        // row that no longer exists.
        let second = import_paths(&fx.lib, &mut fx.conn, &[path]).unwrap();
        assert_eq!(second.imported.len(), 1, "{:?}", second);
        assert_eq!(second.duplicates, 0);
        assert!(fx.lib.blob_path(&second.imported[0].hash, "png").exists());
    }

    #[test]
    fn deleting_nothing_is_a_no_op() {
        let mut fx = Fixture::new("delete-empty");
        let report = delete_assets(&fx.lib, &mut fx.conn, &[]).expect("delete");
        assert_eq!(report.deleted, 0);

        // Unknown ids must not error either.
        let missing = delete_assets(&fx.lib, &mut fx.conn, &[9999]).expect("delete");
        assert_eq!(missing.deleted, 0);
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut fx = Fixture::new("empty");
        let report = import_paths(&fx.lib, &mut fx.conn, &[]).expect("import");
        assert_eq!(report.imported.len(), 0);
        assert_eq!(report.duplicates, 0);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn list_assets_is_newest_first_and_carries_swatches() {
        let mut fx = Fixture::new("list");
        let a = fx.write_png("one.png", 8, 8, [255, 0, 0, 255]);
        let b = fx.write_png("two.png", 8, 8, [0, 255, 0, 255]);
        import_paths(&fx.lib, &mut fx.conn, &[a, b]).expect("import");

        let listed = list_assets(&fx.lib, &fx.conn, 10, 0).expect("list");
        assert_eq!(listed.len(), 2);
        // Same imported_at within one batch, so id DESC breaks the tie.
        assert!(listed[0].id > listed[1].id);
        assert!(listed.iter().all(|a| !a.swatches.is_empty()));

        let page = list_assets(&fx.lib, &fx.conn, 1, 1).expect("page");
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, listed[1].id);
    }

    #[test]
    fn color_search_finds_near_matches_and_rejects_far_ones() {
        let mut fx = Fixture::new("color");
        let red = fx.write_png("red.png", 16, 16, [255, 0, 0, 255]);
        let green = fx.write_png("green.png", 16, 16, [0, 255, 0, 255]);
        import_paths(&fx.lib, &mut fx.conn, &[red, green]).expect("import");

        // A slightly-off red should match the red image.
        let hits = search_by_color(&fx.lib, &fx.conn, "#f50505", 0.15, 10).expect("search");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].asset.original_name.as_deref(), Some("red.png"));

        // A tolerance of zero matches nothing but an exact hit.
        let none = search_by_color(&fx.lib, &fx.conn, "#f50505", 0.0, 10).expect("search");
        assert!(none.is_empty());

        // Exact green matches green.
        let green_hits = search_by_color(&fx.lib, &fx.conn, "#00ff00", 0.05, 10).expect("search");
        assert_eq!(green_hits.len(), 1);
        assert_eq!(
            green_hits[0].asset.original_name.as_deref(),
            Some("green.png")
        );
    }

    #[test]
    fn color_search_on_unparseable_input_returns_nothing() {
        let mut fx = Fixture::new("bad-hex");
        let red = fx.write_png("red.png", 8, 8, [255, 0, 0, 255]);
        import_paths(&fx.lib, &mut fx.conn, &[red]).expect("import");

        assert!(search_by_color(&fx.lib, &fx.conn, "not-a-colour", 1.0, 10)
            .expect("search")
            .is_empty());
    }

    #[test]
    fn each_asset_appears_once_even_with_several_near_swatches() {
        let mut fx = Fixture::new("dedupe-rank");
        // Two bands of very similar red -> two swatches, both near the query.
        let mut img = RgbaImage::from_pixel(16, 16, Rgba([255, 0, 0, 255]));
        for y in 8..16 {
            for x in 0..16 {
                img.put_pixel(x, y, Rgba([250, 6, 6, 255]));
            }
        }
        let path = fx.dir.join("src").join("bands.png");
        let mut buf = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, buf.into_inner()).unwrap();

        import_paths(&fx.lib, &mut fx.conn, &[path]).expect("import");

        let hits = search_by_color(&fx.lib, &fx.conn, "#ff0000", 0.5, 10).expect("search");
        assert_eq!(hits.len(), 1, "one asset must not rank twice: {hits:?}");
    }
}
