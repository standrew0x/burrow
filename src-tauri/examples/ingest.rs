//! Dev tool: run the ingest pipeline outside the app.
//!
//!   cargo run --release --example ingest -- <library-root> <file-or-dir>...
//!
//! Useful for timing real imports and for checking palette output on real
//! images, neither of which the synthetic fixtures in the unit tests cover.

use std::path::{Path, PathBuf};
use std::time::Instant;

use burrow_lib::{db, ingest, store::Library};

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    collect(&entry.path(), out);
                }
            }
            Err(e) => eprintln!("skipping {}: {e}", path.display()),
        }
    } else if path.is_file() {
        out.push(path.to_path_buf());
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: ingest <library-root> <file-or-dir>...");
        std::process::exit(2);
    };

    let mut paths = Vec::new();
    for arg in args {
        collect(Path::new(&arg), &mut paths);
    }
    if paths.is_empty() {
        eprintln!("no input files found");
        std::process::exit(2);
    }

    let library = Library::open(PathBuf::from(&root)).expect("open library");
    let mut conn = db::open(&library.db_path()).expect("open database");

    println!("library: {}", library.root().display());
    println!("candidates: {}\n", paths.len());

    let started = Instant::now();
    let report = ingest::import_paths(&library, &mut conn, &paths).expect("import");
    let elapsed = started.elapsed();

    println!(
        "imported {} | duplicates {} | failed {} | {:.2}s ({:.0} ms/file)",
        report.imported.len(),
        report.duplicates,
        report.failed.len(),
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / paths.len() as f64,
    );

    for failure in &report.failed {
        println!("  FAILED {} -- {}", failure.path, failure.reason);
    }

    println!("\nfirst few palettes:");
    for asset in report.imported.iter().take(5) {
        let strip: Vec<String> = asset
            .swatches
            .iter()
            .map(|s| format!("{} {:.0}%", s.hex, s.weight * 100.0))
            .collect();
        println!(
            "  {:>5}x{:<5} {}  {}",
            asset.width,
            asset.height,
            strip.join("  "),
            asset.original_name.as_deref().unwrap_or("<unnamed>"),
        );
    }
}
