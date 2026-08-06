//! Exercises the X sync path outside the app: discovery, media extraction, and
//! the poster/download split. Keeps the network work testable without driving
//! the GUI.
//!
//!   cargo run --example xsync_probe -- <library-root> [limit]
//!
//! Reads all bookmarks rather than a folder. Bookmark folders are an X Premium
//! feature and the folder endpoint answers "not authorized" on accounts without
//! it, which says nothing about whether syncing works.

use burrow_lib::store::Library;
use burrow_lib::xsync::{BookmarkKind, BookmarkSource, FetchOptions, XClient, XSession};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .expect("usage: xsync_probe <library-root> [limit]");
    let limit: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(12);

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
    let specs = match client.discover(&["Bookmarks"]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };
    println!(
        "  Bookmarks  {}  ({} switches)",
        specs[0].query_id,
        specs[0].feature_switches.len()
    );

    println!("\nfetching up to {limit} media items from all bookmarks...");
    let opts = FetchOptions {
        limit,
        ..Default::default()
    };
    let items = match client.fetch_bookmarks(&specs[0], &BookmarkSource::All, &opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };
    println!("  {} item(s)", items.len());

    // The question this probe exists to answer: does a video entity carry a
    // poster image? If not, a linked video reference has no thumbnail and the
    // whole link-without-downloading idea collapses to a grey box.
    let videos: Vec<_> = items
        .iter()
        .filter(|i| i.kind == BookmarkKind::Video)
        .collect();
    let with_poster = videos.iter().filter(|i| i.poster_url.is_some()).count();
    println!(
        "\n  videos: {}   with a poster URL: {}",
        videos.len(),
        with_poster
    );
    for v in videos.iter().take(4) {
        println!(
            "    @{:<18} {}\n      media  {}\n      poster {}",
            v.author,
            v.date,
            v.media_url,
            v.poster_url.as_deref().unwrap_or("(none)")
        );
    }

    // Prove the poster is a real, decodable image and measure what linking
    // actually costs against downloading.
    let Some(sample) = videos.iter().find(|v| v.poster_url.is_some()) else {
        eprintln!("\nno video had a poster; linked video references are not viable this way");
        std::process::exit(1);
    };

    println!("\nfetching one poster...");
    match client.fetch_thumbnail(sample) {
        Ok(bytes) => {
            let format = burrow_lib::image_ops::format_of(&bytes);
            println!(
                "  {} bytes, sniffed as {:?}",
                bytes.len(),
                format.map(|(ext, _)| ext).unwrap_or("UNRECOGNISED")
            );
            if format.is_none() {
                eprintln!("  poster did not sniff as an image -- an error page?");
                std::process::exit(1);
            }

            // The saving is the entire argument for linking by default.
            match client.head_length(&sample.media_url) {
                Ok(Some(video_bytes)) => println!(
                    "  video is {:.1}MB, poster is {:.0}KB -- linking costs {:.1}% as much",
                    video_bytes as f64 / 1_048_576.0,
                    bytes.len() as f64 / 1024.0,
                    100.0 * bytes.len() as f64 / video_bytes as f64
                ),
                Ok(None) => println!("  (server did not advertise the video's length)"),
                Err(e) => println!("  could not size the video: {e}"),
            }
        }
        Err(e) => {
            eprintln!("  poster fetch failed: {e}");
            std::process::exit(1);
        }
    }
}
