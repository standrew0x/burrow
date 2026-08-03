//! Exercises the X sync path outside the app: discovery, folder lookup, and a
//! small download. Keeps the network work testable without driving the GUI.
//!
//!   cargo run --example xsync_probe -- <library-root> [folder] [limit]

use burrow_lib::store::Library;
use burrow_lib::xsync::{BookmarkSource, FetchOptions, XClient, XSession};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .expect("usage: xsync_probe <library-root> [folder] [limit]");
    let folder_name = args.next().unwrap_or_else(|| "Reference Edits".to_string());
    let limit: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(3);

    let lib = Library::open(&root).expect("open library");
    let session = match XSession::load(&lib) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let client = XClient::new(session).expect("client");

    println!("discovering query ids...");
    let specs = match client.discover(&["BookmarkFoldersSlice", "BookmarkFolderTimeline"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };
    for (op, spec) in ["BookmarkFoldersSlice", "BookmarkFolderTimeline"]
        .iter()
        .zip(&specs)
    {
        println!(
            "  {op:<24} {}  ({} switches)",
            spec.query_id,
            spec.feature_switches.len()
        );
    }

    println!("\nlisting folders...");
    let folders = match client.folders(&specs[0]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };
    println!("  {} folder(s)", folders.len());
    let Some((_, folder_id)) = folders.iter().find(|(n, _)| n == &folder_name) else {
        eprintln!("  folder {folder_name:?} not found");
        eprintln!(
            "  available: {:?}",
            folders.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
        std::process::exit(1);
    };

    println!("\nfetching up to {limit} media items from {folder_name:?}...");
    let opts = FetchOptions {
        limit,
        ..Default::default()
    };
    let source = BookmarkSource::Folder(folder_id.clone());
    let videos = match client.fetch_bookmarks(&specs[1], &source, &opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };
    println!("  {} item(s)", videos.len());
    for v in &videos {
        println!(
            "    @{:<20} {:<6} {:<12} {}",
            v.author,
            format!("{:?}", v.kind).to_lowercase(),
            v.date,
            v.tweet_url
        );
    }

    if let Some(first) = videos.first() {
        let dir = std::env::temp_dir().join("burrow-xsync-probe");
        std::fs::create_dir_all(&dir).unwrap();
        println!("\ndownloading one to verify the media path...");
        match client.download(first, &dir) {
            Ok(path) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                println!("  {} ({:.1}MB)", path.display(), size as f64 / 1_048_576.0);
                // Prove it is a real playable file, not an error page.
                match burrow_lib::video::probe(&path) {
                    Ok(Some(info)) => println!(
                        "  ffprobe: {}x{} {}ms {}",
                        info.width, info.height, info.duration_ms, info.codec
                    ),
                    Ok(None) => println!("  ffprobe: NOT a video (downloaded an error page?)"),
                    Err(e) => println!("  ffprobe failed: {e}"),
                }
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => println!("  download failed: {e}"),
        }
    }
}
