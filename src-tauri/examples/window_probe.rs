//! Compares what a date-filtered sync returns now against what the old
//! stop-at-the-first-old-post rule would have returned.
//!
//!   cargo run --example window_probe -- <library-root> <from> [to]
//!
//! Reads only; imports nothing.

use burrow_lib::store::Library;
use burrow_lib::xsync::{BookmarkKind, BookmarkSource, FetchOptions, XClient, XSession};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("usage: window_probe <root> <from> [to]");
    let from = args.next().expect("a from date, YYYY-MM-DD");
    let to = args.next();

    let lib = Library::open(&root).expect("open library");
    let client = XClient::new(XSession::load(&lib).expect("session")).expect("client");
    let specs = client.discover(&["Bookmarks"]).expect("discover");

    let opts = FetchOptions {
        limit: 2000,
        from: Some(from.clone()),
        to: to.clone(),
        ..Default::default()
    };
    let walk = client
        .fetch_bookmarks(&specs[0], &BookmarkSource::All, &opts)
        .expect("fetch");

    let images = walk
        .items
        .iter()
        .filter(|i| i.kind == BookmarkKind::Image)
        .count();
    println!(
        "window {from}..{}\n  {} items ({} images, {} videos)\n  {} posts over {} pages\n  {}",
        to.as_deref().unwrap_or("now"),
        walk.items.len(),
        images,
        walk.items.len() - images,
        walk.posts_scanned,
        walk.pages,
        walk.stop.explain(),
    );

    // What the old rule produced. It has to be measured against the unfiltered
    // stream: the filtered list above cannot show it, because the post that
    // used to end the walk is precisely the one the filter removes.
    let raw = client
        .fetch_bookmarks(
            &specs[0],
            &BookmarkSource::All,
            &FetchOptions {
                limit: 2000,
                ..Default::default()
            },
        )
        .expect("unfiltered fetch");

    let mut old_rule = 0usize;
    let mut stopped_on = None;
    for item in &raw.items {
        if !item.date.is_empty() && item.date.as_str() < from.as_str() {
            stopped_on = Some(item.clone());
            break;
        }
        if to.as_deref().is_some_and(|t| item.date.as_str() > t) {
            continue;
        }
        old_rule += 1;
    }
    println!(
        "\n  unfiltered timeline order: {} items\n  \
         the previous rule stopped at the first post older than {from}, \
         after {old_rule} item(s)",
        raw.items.len()
    );
    if let Some(item) = stopped_on {
        println!(
            "  it stopped on {} ({}), bookmarked recently but written long before",
            item.date, item.tweet_url
        );
    }

    if let Some(first) = walk.items.first() {
        println!(
            "\n  first filename: {:?}",
            first.safe_stem().unwrap_or_default()
        );
    }
    if let Some(last) = walk.items.last() {
        println!(
            "  last  filename: {:?}",
            last.safe_stem().unwrap_or_default()
        );
    }
}
