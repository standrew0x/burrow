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
use rusqlite::Connection;
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRow {
    pub id: i64,
    pub hash: String,
    pub kind: MediaKind,
    /// Present for video only.
    pub duration_ms: Option<i64>,
    pub ext: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: i64,
    pub original_name: Option<String>,
    pub source_url: Option<String>,
    pub imported_at: i64,
    pub swatches: Vec<Swatch>,
    /// Absolute path to the thumbnail, included on every row so the grid does
    /// not need one IPC round-trip per tile to render.
    pub thumb_path: String,
    /// Absolute path to the stored original. Used for video playback; images
    /// render from the thumbnail.
    pub blob_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedImport {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ImportReport {
    pub imported: Vec<AssetRow>,
    /// Files skipped because their digest was already in the library, including
    /// duplicates within this same batch.
    pub duplicates: usize,
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
                 original_name, imported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            duration_ms: p.duration_ms,
            ext: p.ext.clone(),
            mime: p.mime.clone(),
            width: p.width,
            height: p.height,
            bytes: p.bytes,
            original_name: p.original_name.clone(),
            source_url: None,
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

/// Most recently imported first.
pub fn list_assets(
    lib: &Library,
    conn: &Connection,
    limit: i64,
    offset: i64,
) -> Result<Vec<AssetRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, hash, kind, duration_ms, ext, mime, width, height, bytes,
                original_name, source_url, imported_at
         FROM assets ORDER BY imported_at DESC, id DESC LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![limit, offset], |r| {
            let hash: String = r.get(1)?;
            Ok(AssetRow {
                id: r.get(0)?,
                thumb_path: lib.thumb_path(&hash).display().to_string(),
                blob_path: lib
                    .blob_path(&hash, &r.get::<_, String>(4)?)
                    .display()
                    .to_string(),
                hash,
                kind: MediaKind::from_str(&r.get::<_, String>(2)?),
                duration_ms: r.get(3)?,
                ext: r.get(4)?,
                mime: r.get(5)?,
                width: r.get(6)?,
                height: r.get(7)?,
                bytes: r.get(8)?,
                original_name: r.get(9)?,
                source_url: r.get(10)?,
                imported_at: r.get(11)?,
                swatches: Vec::new(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut out = rows;
    for asset in &mut out {
        asset.swatches = swatches_for(conn, asset.id)?;
    }
    Ok(out)
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

fn asset_by_id(lib: &Library, conn: &Connection, id: i64) -> Result<Option<AssetRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, hash, kind, duration_ms, ext, mime, width, height, bytes,
                original_name, source_url, imported_at
         FROM assets WHERE id = ?1",
    )?;
    let mut rows = stmt.query([id])?;
    let Some(r) = rows.next()? else {
        return Ok(None);
    };
    let hash: String = r.get(1)?;
    let mut asset = AssetRow {
        id: r.get(0)?,
        thumb_path: lib.thumb_path(&hash).display().to_string(),
        blob_path: lib
            .blob_path(&hash, &r.get::<_, String>(4)?)
            .display()
            .to_string(),
        hash,
        kind: MediaKind::from_str(&r.get::<_, String>(2)?),
        duration_ms: r.get(3)?,
        ext: r.get(4)?,
        mime: r.get(5)?,
        width: r.get(6)?,
        height: r.get(7)?,
        bytes: r.get(8)?,
        original_name: r.get(9)?,
        source_url: r.get(10)?,
        imported_at: r.get(11)?,
        swatches: Vec::new(),
    };
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
