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
use crate::store::{hash_bytes, Library};

/// Palette size. Five reads as a palette strip in the UI and is enough to
/// cover an image's structure without splitting near-identical shades.
const PALETTE_SIZE: usize = 5;

/// Files decoded concurrently in phase 3. Bounds peak memory: a decoded 48MP
/// image is ~190MB as RGBA8, so an unbounded rayon fan-out over a large drop
/// can exhaust RAM on a 16GB machine.
const DECODE_CHUNK: usize = 16;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRow {
    pub id: i64,
    pub hash: String,
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
    ext: &'static str,
    mime: &'static str,
    width: u32,
    height: u32,
    bytes: i64,
    original_name: Option<String>,
    swatches: Vec<Swatch>,
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
    if paths.is_empty() {
        return Ok(report);
    }

    // --- Phase 1: hash in parallel ---
    let hashed: Vec<std::result::Result<(PathBuf, String), FailedImport>> = paths
        .par_iter()
        .map(|path| match std::fs::read(path) {
            Ok(bytes) => Ok((path.clone(), hash_bytes(&bytes))),
            Err(e) => Err(FailedImport {
                path: path.display().to_string(),
                reason: Error::io(path, e).to_string(),
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
                (hash, ext, mime, width, height, bytes, original_name, imported_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                p.hash,
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
            ext: p.ext.to_string(),
            mime: p.mime.to_string(),
            width: p.width,
            height: p.height,
            bytes: p.bytes,
            original_name: p.original_name.clone(),
            source_url: None,
            imported_at,
            swatches: p.swatches.clone(),
            thumb_path: lib.thumb_path(&p.hash).display().to_string(),
        });
    }
    tx.commit()?;

    Ok(report)
}

fn prepare_one(lib: &Library, path: &Path, hash: &str) -> Result<Prepared> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(path, e))?;

    let (ext, mime) =
        image_ops::format_of(&bytes).ok_or_else(|| Error::Unsupported(path.to_path_buf()))?;

    let image = image_ops::decode(path, &bytes)?;
    let width = image.width();
    let height = image.height();

    let thumb = image_ops::encode_webp(&image_ops::thumbnail(&image, THUMB_LONG_EDGE))?;
    let swatches = palette_from_pixels(&image_ops::palette_samples(&image), PALETTE_SIZE);

    // Blobs are written before the DB row exists. That ordering means a crash
    // between the two leaves an orphaned blob -- wasted disk, reclaimable by a
    // GC pass -- rather than a row pointing at a file that was never written,
    // which would render as a broken tile forever.
    lib.write_if_absent(&lib.blob_path(hash, ext), &bytes)?;
    lib.write_if_absent(&lib.thumb_path(hash), &thumb)?;

    Ok(Prepared {
        hash: hash.to_string(),
        ext,
        mime,
        width,
        height,
        bytes: bytes.len() as i64,
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
        "SELECT id, hash, ext, mime, width, height, bytes, original_name, source_url, imported_at
         FROM assets ORDER BY imported_at DESC, id DESC LIMIT ?1 OFFSET ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![limit, offset], |r| {
            let hash: String = r.get(1)?;
            Ok(AssetRow {
                id: r.get(0)?,
                thumb_path: lib.thumb_path(&hash).display().to_string(),
                hash,
                ext: r.get(2)?,
                mime: r.get(3)?,
                width: r.get(4)?,
                height: r.get(5)?,
                bytes: r.get(6)?,
                original_name: r.get(7)?,
                source_url: r.get(8)?,
                imported_at: r.get(9)?,
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
        "SELECT id, hash, ext, mime, width, height, bytes, original_name, source_url, imported_at
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
        hash,
        ext: r.get(2)?,
        mime: r.get(3)?,
        width: r.get(4)?,
        height: r.get(5)?,
        bytes: r.get(6)?,
        original_name: r.get(7)?,
        source_url: r.get(8)?,
        imported_at: r.get(9)?,
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
