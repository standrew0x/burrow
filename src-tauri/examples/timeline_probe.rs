//! Diagnoses why a bookmark sync returns fewer items than the filter should
//! match, by walking the raw timeline and reporting what each page contained.
//!
//!   cargo run --example timeline_probe -- <library-root> [pages]
//!
//! Talks to GraphQL directly instead of going through `fetch_bookmarks`, so it
//! observes the pages that function *would* have seen -- including the ones it
//! stops early on.

use std::collections::HashSet;

use burrow_lib::store::Library;
use burrow_lib::xsync::{XClient, XSession};
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("usage: timeline_probe <root> [pages]");
    let max_pages: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(6);

    let lib = Library::open(&root).expect("open library");
    let session = XSession::load(&lib).expect("load session");
    let auth_token = session.auth_token.clone();
    let ct0 = session.ct0.clone();
    let client = XClient::new(session).expect("client");

    let spec = client.discover(&["Bookmarks"]).expect("discover").remove(0);
    println!(
        "queryId {} ({} switches)\n",
        spec.query_id,
        spec.feature_switches.len()
    );

    let http = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();

    let features: serde_json::Map<String, Value> = spec
        .feature_switches
        .iter()
        .map(|s| (s.clone(), Value::Bool(true)))
        .collect();
    let features = urlencoding::encode(&Value::Object(features).to_string()).into_owned();

    let mut cursor: Option<String> = None;
    let mut page = 0usize;
    let mut all_dates: Vec<String> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut totals = (0usize, 0usize, 0usize, 0usize); // entries, tweets, photos, videos

    while page < max_pages {
        page += 1;
        let mut vars = serde_json::json!({ "count": 100, "includePromotedContent": false });
        if let Some(c) = &cursor {
            vars["cursor"] = Value::String(c.clone());
        }
        let url = format!(
            "https://x.com/i/api/graphql/{}/Bookmarks?variables={}&features={}",
            spec.query_id,
            urlencoding::encode(&vars.to_string()),
            features
        );
        let resp = http
            .get(&url)
            .header("authorization", "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA")
            .header("x-twitter-auth-type", "OAuth2Session")
            .header("x-twitter-active-user", "yes")
            .header("x-csrf-token", &ct0)
            .header("cookie", format!("auth_token={auth_token}; ct0={ct0}"))
            .send()
            .expect("graphql");
        let status = resp.status().as_u16();
        let body: Value = resp.json().expect("json");
        if let Some(errs) = body.get("errors") {
            println!("page {page}: HTTP {status} errors {errs}");
            break;
        }
        let data = body.get("data").cloned().unwrap_or(Value::Null);

        let instructions = data
            .pointer("/bookmark_timeline_v2/timeline/instructions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut entries = 0usize;
        let mut tweets = 0usize;
        let mut photos = 0usize;
        let mut videos = 0usize;
        let mut no_media = 0usize;
        let mut tombstones = 0usize;
        let mut dupes = 0usize;
        let mut dates: Vec<String> = Vec::new();
        let mut next: Option<String> = None;
        let mut type_counts: std::collections::BTreeMap<String, usize> = Default::default();

        for ins in &instructions {
            let Some(es) = ins.get("entries").and_then(|e| e.as_array()) else {
                continue;
            };
            for entry in es {
                entries += 1;
                let Some(content) = entry.get("content") else {
                    continue;
                };
                let etype = content
                    .get("__typename")
                    .or_else(|| content.get("entryType"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                *type_counts.entry(etype.to_string()).or_default() += 1;

                if etype.contains("Cursor") {
                    if content.get("cursorType").and_then(|v| v.as_str()) == Some("Bottom") {
                        next = content
                            .get("value")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }
                    continue;
                }
                let Some(result) = content.pointer("/itemContent/tweet_results/result") else {
                    continue;
                };
                tweets += 1;
                let tn = result
                    .get("__typename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if tn.contains("Tombstone")
                    || result.get("legacy").is_none() && result.get("tweet").is_none()
                {
                    tombstones += 1;
                    continue;
                }
                let tweet = result.get("tweet").unwrap_or(result);
                let legacy = tweet.get("legacy").unwrap_or(&Value::Null);
                if let Some(id) = legacy.get("id_str").and_then(|v| v.as_str()) {
                    if !seen_ids.insert(id.to_string()) {
                        dupes += 1;
                    }
                }
                if let Some(created) = legacy.get("created_at").and_then(|v| v.as_str()) {
                    let parts: Vec<&str> = created.split_whitespace().collect();
                    if parts.len() >= 6 {
                        const M: [&str; 12] = [
                            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                            "Nov", "Dec",
                        ];
                        if let Some(mi) = M.iter().position(|m| *m == parts[1]) {
                            dates.push(format!(
                                "{}-{:02}-{:02}",
                                parts[5],
                                mi + 1,
                                parts[2].parse::<u32>().unwrap_or(0)
                            ));
                        }
                    }
                }
                let media = legacy
                    .pointer("/extended_entities/media")
                    .or_else(|| legacy.pointer("/entities/media"))
                    .and_then(|v| v.as_array());
                match media {
                    None => no_media += 1,
                    Some(ms) => {
                        for m in ms {
                            match m.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                                "photo" => photos += 1,
                                "video" | "animated_gif" => videos += 1,
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        totals.0 += entries;
        totals.1 += tweets;
        totals.2 += photos;
        totals.3 += videos;

        // Is this page in descending post-date order?
        let sorted_desc = dates.windows(2).all(|w| w[0] >= w[1]);
        let inversions = dates.windows(2).filter(|w| w[0] < w[1]).count();

        println!(
            "page {page}: entries {entries}  tweets {tweets}  photos {photos}  videos {videos}  \
             textonly {no_media}  tombstones {tombstones}  dupes {dupes}"
        );
        println!("   types: {type_counts:?}");
        println!(
            "   dates {}..{}  descending={sorted_desc}  inversions={inversions}",
            dates.last().cloned().unwrap_or_default(),
            dates.first().cloned().unwrap_or_default(),
        );
        if !sorted_desc {
            let sample: Vec<&String> = dates.iter().take(12).collect();
            println!("   first 12 in page order: {sample:?}");
        }
        all_dates.extend(dates);

        match next {
            Some(c) => {
                if Some(&c) == cursor.as_ref() {
                    println!("   cursor did not advance -- would loop");
                    break;
                }
                cursor = Some(c);
            }
            None => {
                println!("   no bottom cursor -- end of timeline");
                break;
            }
        }
    }

    println!(
        "\ntotals over {page} pages: entries {} tweets {} photos {} videos {}",
        totals.0, totals.1, totals.2, totals.3
    );
    let global_desc = all_dates.windows(2).all(|w| w[0] >= w[1]);
    let global_inv = all_dates.windows(2).filter(|w| w[0] < w[1]).count();
    println!(
        "post dates across all pages: descending={global_desc} inversions={global_inv} of {}",
        all_dates.len().saturating_sub(1)
    );
    if !global_desc {
        println!(
            "=> the timeline is NOT ordered by post date, so a `from` cutoff that\n\
             \x20  stops paging at the first older post truncates the sync."
        );
    }
}
