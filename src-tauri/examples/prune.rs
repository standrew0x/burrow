//! Maintenance CLI: remove references from a library by name.
//!
//! Dry-run by default. Deletion is the one irreversible operation here, so
//! actually erasing anything requires `--yes` on top of a matching filter.
//!
//!   cargo run --example prune -- <library> --match bars_ --match smpte_
//!   cargo run --example prune -- <library> --match bars_ --yes
//!
//! `--match` is a case-insensitive substring of the original filename.

use burrow_lib::{db, ingest, store::Library};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: prune <library-root> --match <substring> [--match ...] [--yes]");
        std::process::exit(2);
    };

    let mut patterns: Vec<String> = Vec::new();
    let mut ids: Vec<i64> = Vec::new();
    let mut confirmed = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--match" => {
                if let Some(p) = args.next() {
                    patterns.push(p.to_lowercase());
                }
            }
            // Explicit ids, for when a substring would be ambiguous. Safer than
            // a clever pattern: nothing unexpected can match.
            "--id" => {
                if let Some(v) = args.next() {
                    match v.parse() {
                        Ok(id) => ids.push(id),
                        Err(_) => {
                            eprintln!("not a numeric id: {v}");
                            std::process::exit(2);
                        }
                    }
                }
            }
            "--yes" => confirmed = true,
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if patterns.is_empty() && ids.is_empty() {
        eprintln!("refusing to run without at least one --match or --id filter");
        std::process::exit(2);
    }

    let lib = Library::open(&root).expect("open library");
    let mut conn = db::open(&lib.db_path()).expect("open db");
    println!("library: {}", lib.root().display());

    let all = ingest::list_assets(&lib, &conn, 100_000, 0).expect("list");
    let doomed: Vec<_> = all
        .iter()
        .filter(|a| {
            let name = a.original_name.clone().unwrap_or_default().to_lowercase();
            ids.contains(&a.id) || patterns.iter().any(|p| name.contains(p))
        })
        .collect();

    if doomed.is_empty() {
        println!("nothing matched; {} references left untouched", all.len());
        return;
    }

    println!("\nmatched {} of {}:", doomed.len(), all.len());
    for a in &doomed {
        println!(
            "   [{:>5}] {:>8}  {}",
            format!("{:?}", a.kind).to_lowercase(),
            format!("{:.1}MB", a.bytes as f64 / 1_048_576.0),
            a.original_name.as_deref().unwrap_or(&a.hash)
        );
    }

    if !confirmed {
        println!("\nDRY RUN. Re-run with --yes to delete these permanently.");
        return;
    }

    let ids: Vec<i64> = doomed.iter().map(|a| a.id).collect();
    let report = ingest::delete_assets(&lib, &mut conn, &ids).expect("delete");
    println!(
        "\ndeleted {} | {:.1}MB freed | {} orphaned files",
        report.deleted,
        report.bytes_freed as f64 / 1_048_576.0,
        report.orphaned_files.len()
    );
    for f in &report.orphaned_files {
        println!("   could not unlink: {f}");
    }

    let left = ingest::list_assets(&lib, &conn, 100_000, 0).expect("list");
    println!("\n{} reference(s) remain:", left.len());
    for a in &left {
        println!("   {}", a.original_name.as_deref().unwrap_or(&a.hash));
    }
}
