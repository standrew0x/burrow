//! Exercises the link path against the real internet: resolve a URL, store it
//! as a reference, then download it and check it became a local file.
//!
//!   cargo run --example link_probe -- <library-root> <url>...
//!
//! Uses a scratch library rather than the real one, so it never adds tiles to
//! an actual collection.

use burrow_lib::ingest::{self, AssetState};
use burrow_lib::store::Library;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .expect("usage: link_probe <library-root> <url>...");
    let urls: Vec<String> = args.collect();
    if urls.is_empty() {
        eprintln!("give at least one URL");
        std::process::exit(2);
    }

    let scratch = std::path::PathBuf::from(&root);
    let lib = Library::open(&scratch).expect("open library");
    let mut conn = burrow_lib::db::open(&lib.db_path()).expect("open db");

    // Optional: only X post links need it.
    let x = burrow_lib::xsync::XSession::load(&lib)
        .ok()
        .and_then(|s| burrow_lib::xsync::XClient::new(s).ok());
    println!(
        "X session: {}\n",
        if x.is_some() { "loaded" } else { "none" }
    );

    let mut pending = Vec::new();
    for url in &urls {
        print!("resolving {url}\n  ");
        match burrow_lib::link::resolve(url, x.as_ref()) {
            Ok(r) => {
                println!(
                    "{:?}  thumb={}B  title={:?}",
                    r.kind,
                    r.thumbnail.len(),
                    r.title
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(50)
                        .collect::<String>()
                );
                println!("  media  {}", r.media_url);
                pending.push(ingest::PendingLink {
                    page_url: r.page_url,
                    media_url: r.media_url,
                    kind: r.kind,
                    title: r.title,
                    thumbnail: r.thumbnail,
                });
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }

    if pending.is_empty() {
        eprintln!("\nnothing resolved");
        std::process::exit(1);
    }

    println!("\nimporting {} link(s)...", pending.len());
    let report = ingest::import_links(&lib, &mut conn, pending).expect("import");
    println!(
        "  imported {}  duplicates {}  failed {}",
        report.imported.len(),
        report.duplicates,
        report.failed.len()
    );
    for f in &report.failed {
        println!("  FAILED {}: {}", f.path, f.reason);
    }

    for a in &report.imported {
        let thumb_exists = std::path::Path::new(&a.thumb_path).is_file();
        let blob_exists = std::path::Path::new(&a.blob_path).exists();
        println!(
            "  #{} {:?} {:?} {}x{} swatches={} thumb={} blob={}",
            a.id,
            a.state,
            a.kind,
            a.width,
            a.height,
            a.swatches.len(),
            if thumb_exists { "yes" } else { "MISSING" },
            if blob_exists {
                "PRESENT (wrong)"
            } else {
                "none"
            }
        );
        // A link that stored no thumbnail is a grey box in the grid, and one
        // with no palette silently drops out of colour search.
        assert!(thumb_exists, "linked reference has no thumbnail");
        assert!(!blob_exists, "linked reference wrote a blob");
        assert!(!a.swatches.is_empty(), "linked reference has no palette");
    }

    // Download the first one and confirm it turns into a real local file.
    let Some(first) = report.imported.first() else {
        return;
    };
    println!("\ndownloading #{} ...", first.id);
    let started = std::time::Instant::now();
    let done = ingest::download_assets(
        &lib,
        &mut conn,
        &[first.id],
        |url, dest| match burrow_lib::xsync::is_x_media(url) {
            true => match &x {
                Some(client) => client.stream_to(url, dest),
                None => burrow_lib::link::download_to(url, dest),
            },
            false => burrow_lib::link::download_to(url, dest),
        },
        |_, n, total| println!("  {n}/{total}"),
    )
    .expect("download");

    println!(
        "  downloaded {}  deduped {}  failed {}  in {:.1}s",
        done.downloaded.len(),
        done.deduplicated,
        done.failed.len(),
        started.elapsed().as_secs_f64()
    );
    for f in &done.failed {
        println!("  FAILED {}: {}", f.path, f.reason);
    }
    for a in &done.downloaded {
        println!(
            "  #{} {:?} {}x{} {}  duration={:?}ms  blob={}",
            a.id,
            a.state,
            a.width,
            a.height,
            format_bytes(a.bytes),
            a.duration_ms,
            std::path::Path::new(&a.blob_path).is_file()
        );
        assert_eq!(a.state, AssetState::Local);
        assert!(std::path::Path::new(&a.blob_path).is_file(), "no blob");
        assert!(a.bytes > 0, "downloaded asset still reports zero bytes");
    }
}

fn format_bytes(n: i64) -> String {
    if n < 1024 * 1024 {
        format!("{:.0}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / 1_048_576.0)
    }
}
