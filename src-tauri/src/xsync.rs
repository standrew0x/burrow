//! Pulls videos from an X bookmark folder into the library.
//!
//! Uses X's internal GraphQL API with the user's session cookies -- the same
//! route the web client takes. There is no supported API for this on the free
//! tier, and the endpoint ids are not stable: they live inside a webpack chunk
//! whose name and hash change with every X deploy, so they are rediscovered at
//! run time rather than pinned.
//!
//! Expect this to break when X ships a frontend change. The failure modes are
//! deliberately distinguishable -- [`XError::Discovery`] means the bundle moved,
//! [`XError::Auth`] means the session expired -- because they look identical
//! from the outside and need completely different fixes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::store::Library;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

/// Filename of the session file inside the library root.
const SESSION_FILE: &str = "x_session.json";

/// How far past the chunk-loader anchor to scan for its two object literals.
const LOADER_SCAN_BYTES: usize = 400_000;

/// Timeline page size. X accepts up to 100.
const PAGE_SIZE: u32 = 100;

/// Hard cap on pages walked in one fetch, so a request for "everything" cannot
/// hammer X indefinitely. At 100 posts a page this is 10,000 bookmarks; hitting
/// it is reported rather than passed off as the end of the list.
const MAX_PAGES: usize = 100;

/// Consecutive pages entirely older than `from` before paging gives up.
///
/// Cannot be 1. Bookmarks come back in the order they were bookmarked, so page
/// ranges overlap -- one measured pair ran 2025-11-20..2026-01-22 followed by
/// 2025-10-28..2025-12-30. Three pages is 300 posts of margin past the point
/// where the window looks finished.
const STALE_PAGES_BEFORE_STOP: usize = 3;

#[derive(Debug)]
pub enum XError {
    /// No session file, or it is unusable.
    NoSession(String),
    /// 401/403 -- cookies expired. Distinct from every other failure because
    /// the fix is "log in again", not "fix the code".
    Auth,
    /// The bundle layout changed and query ids could not be found.
    Discovery(String),
    Http(String),
    Parse(String),
}

impl std::fmt::Display for XError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XError::NoSession(m) => write!(f, "{m}"),
            XError::Auth => write!(
                f,
                "X rejected the session (401/403) -- the cookies have expired. \
                 Refresh auth_token and ct0 from a logged-in browser."
            ),
            XError::Discovery(m) => write!(
                f,
                "Could not find X's GraphQL query ids ({m}). X has likely changed \
                 its frontend bundle; the discovery code needs updating."
            ),
            XError::Http(m) => write!(f, "network error talking to X: {m}"),
            XError::Parse(m) => write!(f, "unexpected response shape from X: {m}"),
        }
    }
}

impl From<XError> for Error {
    fn from(e: XError) -> Self {
        Error::X(e.to_string())
    }
}

// --- session ---

#[derive(Debug, Deserialize)]
pub struct XSession {
    pub auth_token: String,
    pub ct0: String,
}

impl XSession {
    pub fn path(lib: &Library) -> PathBuf {
        lib.root().join(SESSION_FILE)
    }

    /// Reads the session from the library root.
    ///
    /// Kept beside the library rather than in the app directory: the app
    /// directory is wiped by the installer, and re-pasting cookies after every
    /// update would be miserable.
    pub fn load(lib: &Library) -> std::result::Result<Self, XError> {
        let path = Self::path(lib);
        if !path.exists() {
            return Err(XError::NoSession(format!(
                "No X session found. Create {} containing \
                 {{\"auth_token\": \"...\", \"ct0\": \"...\"}} using the cookie values \
                 from a logged-in x.com browser tab (DevTools > Application > Cookies).",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| XError::NoSession(format!("could not read {}: {e}", path.display())))?;
        let session: XSession = serde_json::from_str(&text)
            .map_err(|e| XError::NoSession(format!("{} is not valid JSON: {e}", path.display())))?;
        if session.auth_token.trim().is_empty() || session.ct0.trim().is_empty() {
            return Err(XError::NoSession(format!(
                "{} is missing auth_token or ct0",
                path.display()
            )));
        }
        Ok(session)
    }

    /// Writes the session to the library root.
    ///
    /// Values are trimmed because pasting from DevTools routinely drags along
    /// whitespace, and a stray space turns a valid token into a 401 that looks
    /// exactly like an expired login.
    pub fn save(lib: &Library, auth_token: &str, ct0: &str) -> Result<()> {
        let auth_token = auth_token.trim();
        let ct0 = ct0.trim();
        if auth_token.is_empty() || ct0.is_empty() {
            return Err(Error::X("both auth_token and ct0 are required".into()));
        }
        let path = Self::path(lib);
        let body = serde_json::json!({ "auth_token": auth_token, "ct0": ct0 });
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        )
        .map_err(|e| Error::io(&path, e))?;
        Ok(())
    }

    /// Removes the stored session.
    pub fn clear(lib: &Library) -> Result<()> {
        let path = Self::path(lib);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        }
        Ok(())
    }
}

// --- client ---

pub struct XClient {
    http: reqwest::blocking::Client,
    session: XSession,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuerySpec {
    pub query_id: String,
    pub feature_switches: Vec<String>,
}

impl XClient {
    pub fn new(session: XSession) -> std::result::Result<Self, XError> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| XError::Http(e.to_string()))?;
        Ok(Self { http, session })
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        // This bearer token is the public web-client constant, not a secret.
        h.insert("authorization", HeaderValue::from_static(
            "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA",
        ));
        h.insert(
            "x-twitter-auth-type",
            HeaderValue::from_static("OAuth2Session"),
        );
        h.insert("x-twitter-active-user", HeaderValue::from_static("yes"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        if let Ok(v) = HeaderValue::from_str(&self.session.ct0) {
            h.insert("x-csrf-token", v);
        }
        let cookie = format!(
            "auth_token={}; ct0={}",
            self.session.auth_token, self.session.ct0
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            h.insert("cookie", v);
        }
        h
    }

    /// Rediscovers GraphQL query ids from the live web bundle.
    pub fn discover(&self, operations: &[&str]) -> std::result::Result<Vec<QuerySpec>, XError> {
        let html = self
            .http
            .get("https://x.com/i/bookmarks")
            .send()
            .and_then(|r| r.text())
            .map_err(|e| XError::Http(e.to_string()))?;

        // The leading identifier is minified and changes between builds -- it
        // was `g`, it is now `p` -- so it must not be hardcoded.
        let anchor = regex::Regex::new(r"\b[A-Za-z_$][A-Za-z0-9_$]{0,2}\.u=e=>").unwrap();
        let m = anchor
            .find(&html)
            .ok_or_else(|| XError::Discovery("chunk loader <var>.u=e=> not found".into()))?;

        // Two separate object literals: `(({names})[e]||e)+"."+({hashes})[e]`.
        // A parenthesis-depth walk stops after the first and silently yields an
        // empty hash map, so read brace-balanced blocks instead.
        let tail = &html[m.end()..(m.end() + LOADER_SCAN_BYTES).min(html.len())];
        let blocks = balanced_brace_blocks(tail, 2);
        if blocks.len() < 2 {
            return Err(XError::Discovery(format!(
                "expected 2 object literals after the loader, found {}",
                blocks.len()
            )));
        }

        let entry = regex::Regex::new(r#"(\d+):"([^"]*)""#).unwrap();
        let names: Vec<(String, String)> = entry
            .captures_iter(blocks[0])
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect();
        let hashes: std::collections::HashMap<String, String> = entry
            .captures_iter(blocks[1])
            .map(|c| (c[1].to_string(), c[2].to_string()))
            .collect();

        // Which chunks are worth fetching.
        //
        // This used to be `n.contains("Bookmark")`, which quietly meant only
        // bookmark operations were ever discoverable -- asking for anything
        // else scanned no chunks at all and reported "X changed its frontend",
        // which is a misleading way to say "we never looked". The keywords now
        // come from the operations requested.
        let keywords = chunk_keywords(operations);
        let mut candidates: Vec<&(String, String)> = names
            .iter()
            .filter(|(_, n)| {
                let lower = n.to_ascii_lowercase();
                // Shared chunks carry many operations and are named for none of
                // them, so they have to be included on faith.
                lower.contains("shared") || keywords.iter().any(|k| lower.contains(k))
            })
            .collect();
        // Shared chunks first: they carry several operations at once, so the
        // common case resolves everything in one fetch.
        candidates.sort_by_key(|(_, n)| !n.contains("shared"));
        // X occasionally renames an operation chunk to a generic bundle name.
        // Keep the targeted pass cheap, then fall back to the remaining JS
        // chunks instead of reporting a false discovery failure.
        let targeted_ids: std::collections::HashSet<String> =
            candidates.iter().map(|(cid, _)| cid.clone()).collect();
        for pair in &names {
            if !targeted_ids.contains(&pair.0) {
                candidates.push(pair);
            }
        }

        // Compiled once, not per chunk per operation: regex construction is far
        // more expensive than the match itself.
        let switch_re = regex::Regex::new(r#"featureSwitches:\[([^\]]*)\]"#).unwrap();
        let switch_value_re = regex::Regex::new(r#""([^"]+)""#).unwrap();
        let mut found: Vec<QuerySpec> = Vec::new();
        let mut found_names: Vec<String> = Vec::new();

        for (cid, cname) in candidates {
            if found.len() == operations.len() {
                break;
            }
            let Some(hash) = hashes.get(cid) else {
                continue;
            };
            let url = format!("https://abs.twimg.com/responsive-web/client-web/{cname}.{hash}a.js");
            let Ok(resp) = self.http.get(&url).send() else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(chunk) = resp.text() else { continue };

            for op in operations {
                if found_names.iter().any(|f| f == op) {
                    continue;
                }
                if let Some(spec) =
                    Self::query_spec_in_chunk(&chunk, op, &switch_re, &switch_value_re)
                {
                    found.push(spec);
                    found_names.push((*op).to_string());
                }
            }
        }

        let missing: Vec<&&str> = operations
            .iter()
            .filter(|o| !found_names.iter().any(|f| f == *o))
            .collect();
        if !missing.is_empty() {
            return Err(XError::Discovery(format!("no queryId for {missing:?}")));
        }

        // Return in the order the caller asked for.
        let ordered = operations
            .iter()
            .map(|op| {
                let idx = found_names.iter().position(|f| f == op).unwrap();
                found[idx].clone()
            })
            .collect();
        Ok(ordered)
    }

    /// Extracts one operation descriptor from a minified client chunk.
    ///
    /// X has changed the descriptor shape several times: feature switches have
    /// moved under nested metadata objects and are occasionally omitted entirely.
    /// The query id and operation name are the stable pair; switches are optional
    /// and default to an empty feature object when absent.
    fn query_spec_in_chunk(
        chunk: &str,
        operation: &str,
        switch_re: &regex::Regex,
        switch_value_re: &regex::Regex,
    ) -> Option<QuerySpec> {
        const DESCRIPTOR_SCAN_BYTES: usize = 4_000;
        let operation_re =
            regex::Regex::new(&format!(r#"operationName:"{}""#, regex::escape(operation))).ok()?;
        let operation_match = operation_re.find(chunk)?;

        // A bundle chunk contains many adjacent operation descriptors. Looking
        // forward from the first queryId in a wide window can pair Bookmarks
        // with an unrelated operation several modules earlier. Start at the
        // operation name and take the nearest preceding queryId instead.
        let mut start = operation_match
            .start()
            .saturating_sub(DESCRIPTOR_SCAN_BYTES);
        while start < operation_match.start() && !chunk.is_char_boundary(start) {
            start += 1;
        }
        let query_re = regex::Regex::new(r#"queryId:"([^"]+)""#).ok()?;
        let query = query_re
            .captures_iter(&chunk[start..operation_match.start()])
            .last()?;
        let query_match = query.get(0)?;
        let descriptor_start = start + query_match.start();

        // Stop before the next operation descriptor so a switch-less operation
        // cannot accidentally inherit the following operation's switches.
        let after_operation = operation_match.end();
        let next_query = query_re
            .find(&chunk[after_operation..])
            .map(|m| after_operation + m.start());
        let mut end = next_query
            .unwrap_or_else(|| (after_operation + DESCRIPTOR_SCAN_BYTES).min(chunk.len()));
        while end > after_operation && !chunk.is_char_boundary(end) {
            end -= 1;
        }
        let descriptor = &chunk[descriptor_start..end];
        let feature_switches = switch_re
            .captures(descriptor)
            .map(|c| {
                switch_value_re
                    .captures_iter(&c[1])
                    .map(|s| s[1].to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Some(QuerySpec {
            query_id: query[1].to_string(),
            feature_switches,
        })
    }

    fn graphql(
        &self,
        spec: &QuerySpec,
        op_name: &str,
        variables: Value,
    ) -> std::result::Result<Value, XError> {
        // Every switch defaults to true. X returns HTTP 200 with an `errors`
        // array when a flag is missing, so the set has to be complete.
        let features: serde_json::Map<String, Value> = spec
            .feature_switches
            .iter()
            .map(|s| (s.clone(), Value::Bool(true)))
            .collect();

        let url = format!(
            "https://x.com/i/api/graphql/{}/{}?variables={}&features={}",
            spec.query_id,
            op_name,
            urlencoding::encode(&variables.to_string()),
            urlencoding::encode(&Value::Object(features).to_string()),
        );

        let mut last = String::new();
        for attempt in 0..4 {
            let resp = match self.http.get(&url).headers(self.auth_headers()).send() {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(Duration::from_secs(1 << attempt));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                return Err(XError::Auth);
            }
            if status == 429 || (500..600).contains(&status) {
                last = format!("HTTP {status}");
                std::thread::sleep(Duration::from_secs(1 << attempt));
                continue;
            }
            if !resp.status().is_success() {
                return Err(XError::Http(format!("{op_name} returned HTTP {status}")));
            }
            let body: Value = resp
                .json()
                .map_err(|e| XError::Parse(format!("{op_name}: {e}")))?;
            if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
                if !errors.is_empty() {
                    let msgs: Vec<&str> = errors
                        .iter()
                        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                        .collect();
                    return Err(XError::Parse(format!("{op_name}: {}", msgs.join("; "))));
                }
            }
            return body
                .get("data")
                .cloned()
                .ok_or_else(|| XError::Parse(format!("{op_name}: no data field")));
        }
        Err(XError::Http(format!(
            "{op_name} failed after retries: {last}"
        )))
    }

    /// `(name, id)` for every bookmark folder.
    pub fn folders(&self, spec: &QuerySpec) -> std::result::Result<Vec<(String, String)>, XError> {
        let data = self.graphql(spec, "BookmarkFoldersSlice", serde_json::json!({}))?;
        let items = data
            .pointer("/viewer/user_results/result/bookmark_collections_slice/items")
            .and_then(|v| v.as_array())
            .ok_or_else(|| XError::Parse("bookmark_collections_slice missing".into()))?;
        Ok(items
            .iter()
            .filter_map(|i| {
                Some((
                    i.get("name")?.as_str()?.to_string(),
                    i.get("id")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// Images and videos from bookmarks.
    ///
    /// Walks pages until the requested number of items is collected, the
    /// bookmark list runs out, or the page cap is reached -- and says which,
    /// because a sync that quietly returns less than the filter should match is
    /// indistinguishable from a broken one.
    ///
    /// The order this timeline arrives in is the order posts were *bookmarked*,
    /// which is not the order they were written. Measured over 791 consecutive
    /// bookmarks: 185 places where a post was older than the one after it, the
    /// first only seven items into page 1. So an out-of-window post says
    /// nothing about the posts behind it, and per-item filtering has to keep
    /// going rather than conclude the window is finished.
    pub fn fetch_bookmarks(
        &self,
        spec: &QuerySpec,
        source: &BookmarkSource,
        opts: &FetchOptions,
    ) -> std::result::Result<Fetched, XError> {
        let (op_name, base_vars) = match source {
            BookmarkSource::All => (
                "Bookmarks",
                serde_json::json!({ "count": PAGE_SIZE, "includePromotedContent": false }),
            ),
            BookmarkSource::Folder(id) => (
                "BookmarkFolderTimeline",
                serde_json::json!({ "count": PAGE_SIZE, "bookmark_collection_id": id }),
            ),
        };

        let mut out: Vec<BookmarkMedia> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        let mut posts_scanned = 0usize;
        let mut stale_pages = 0usize;
        let mut stop = StopReason::EndOfBookmarks;

        'paging: while pages < MAX_PAGES {
            if out.len() >= opts.limit {
                stop = StopReason::LimitReached;
                break;
            }
            pages += 1;
            let mut vars = base_vars.clone();
            if let Some(c) = &cursor {
                vars["cursor"] = Value::String(c.clone());
            }

            let data = self.graphql(spec, op_name, vars)?;
            let (tweets, next) = extract_tweets_and_cursor(&data);
            posts_scanned += tweets.len();

            let page = collect_page(&tweets, opts, &mut out);
            if out.len() >= opts.limit {
                stop = StopReason::LimitReached;
                break 'paging;
            }

            // Give up on a lower bound only once several whole pages have been
            // older than it. One page is not enough -- page date ranges overlap.
            if let Some(from) = &opts.from {
                match &page.newest {
                    Some(newest) if newest.as_str() < from.as_str() => {
                        stale_pages += 1;
                        if stale_pages >= STALE_PAGES_BEFORE_STOP {
                            stop = StopReason::PastDateWindow;
                            break;
                        }
                    }
                    _ => stale_pages = 0,
                }
            }

            match next {
                // X hands back the same cursor at the end of some timelines
                // instead of omitting it, which would page forever.
                Some(c) if Some(&c) != cursor.as_ref() => cursor = Some(c),
                _ => {
                    stop = StopReason::EndOfBookmarks;
                    break;
                }
            }
            if pages >= MAX_PAGES {
                stop = StopReason::PageCap;
            }
        }

        Ok(Fetched {
            items: out,
            stop,
            pages,
            posts_scanned,
        })
    }

    /// Streams one media item to `dir`. Returns the written path.
    ///
    /// Named by tweet id plus position, so the four photos of a single post do
    /// not overwrite each other.
    pub fn download(&self, item: &BookmarkMedia, dir: &Path) -> Result<PathBuf> {
        let stem = item.safe_stem().ok_or_else(|| {
            Error::X(format!(
                "refusing to save {}: tweet id {:?} is not a decimal id",
                item.tweet_url, item.tweet_id
            ))
        })?;
        let dest = dir.join(format!("{stem}.{}", item.ext));
        self.stream_to(&item.media_url, &dest)?;
        Ok(dest)
    }

    /// Fetches the still image for an item -- poster frame, or the photo itself.
    pub fn fetch_thumbnail(&self, item: &BookmarkMedia) -> Result<Vec<u8>> {
        self.get_bytes(item.thumbnail_source())
    }

    /// GETs a twimg URL into memory. Only for thumbnails, which are ~100KB.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        use std::io::Read;

        if !is_twimg_host(url) {
            return Err(Error::X(format!("refusing to fetch off-network URL {url}")));
        }
        let mut resp = self
            .http
            .get(url)
            .send()
            .map_err(|e| Error::X(format!("fetching {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::X(format!(
                "fetching {url} returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        // Capped even though posters are small: the size is the server's claim,
        // not ours, and this one lands on the heap.
        const MAX_THUMB_BYTES: u64 = 32 * 1024 * 1024;
        let mut buf = Vec::new();
        resp.by_ref()
            .take(MAX_THUMB_BYTES)
            .read_to_end(&mut buf)
            .map_err(|e| Error::X(format!("reading {url}: {e}")))?;
        Ok(buf)
    }

    /// Advertised size of a media URL, without fetching the body.
    ///
    /// Used to show what a download will cost before committing to it. Returns
    /// `None` when the server declines to say, which is not an error.
    pub fn head_length(&self, url: &str) -> Result<Option<u64>> {
        if !is_twimg_host(url) {
            return Err(Error::X(format!("refusing to probe off-network URL {url}")));
        }
        let resp = self
            .http
            .head(url)
            .send()
            .map_err(|e| Error::X(format!("sizing {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::X(format!(
                "sizing {url} returned HTTP {}",
                resp.status().as_u16()
            )));
        }
        Ok(resp.content_length())
    }

    /// Streams `url` to `dest`, refusing anything off-network or oversized.
    pub fn stream_to(&self, url: &str, dest: &Path) -> Result<u64> {
        use std::io::Read;

        if !is_twimg_host(url) {
            return Err(Error::X(format!(
                "refusing to download off-network URL {url}"
            )));
        }
        let mut resp = self
            .http
            .get(url)
            .send()
            .map_err(|e| Error::X(format!("downloading {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::X(format!(
                "downloading {url} returned HTTP {}",
                resp.status().as_u16()
            )));
        }

        // Trust the advertised length only to fail early; the read below is
        // what actually enforces the cap, since Content-Length can lie.
        if let Some(len) = resp.content_length() {
            if len > MAX_DOWNLOAD_BYTES {
                return Err(Error::X(format!(
                    "refusing {url}: {len} bytes exceeds the {MAX_DOWNLOAD_BYTES} byte cap"
                )));
            }
        }

        let mut file = std::fs::File::create(dest).map_err(|e| Error::io(dest, e))?;
        // Streamed, not buffered: videos run to 150MB.
        let written = std::io::copy(&mut resp.by_ref().take(MAX_DOWNLOAD_BYTES), &mut file)
            .map_err(|e| Error::io(dest, e))?;

        if written == MAX_DOWNLOAD_BYTES {
            // Hit the ceiling exactly, so the body was almost certainly still
            // going. A truncated video is worse than no video: it would import
            // cleanly and only reveal itself on playback.
            let _ = std::fs::remove_file(dest);
            return Err(Error::X(format!(
                "{url} exceeded the {MAX_DOWNLOAD_BYTES} byte cap"
            )));
        }
        Ok(written)
    }
}

/// What one page of the timeline contributed.
struct PageSummary {
    /// Newest post date seen, regardless of whether it passed the filters.
    /// `None` when nothing on the page carried a readable date.
    newest: Option<String>,
}

/// Appends every wanted item on one page to `out`.
///
/// Split out of the paging loop so the filtering can be tested without a
/// network: the bug it exists to prevent -- ending a whole sync at the first
/// post older than the start date -- is invisible from the outside, because a
/// truncated sync and a small library return the same thing.
///
/// Stops adding once `out` reaches the limit; the caller decides what that
/// means for paging.
fn collect_page(
    tweets: &[Value],
    opts: &FetchOptions,
    out: &mut Vec<BookmarkMedia>,
) -> PageSummary {
    let mut newest: Option<String> = None;

    for tweet in tweets {
        if out.len() >= opts.limit {
            break;
        }
        let all_items = media_from_tweet(tweet);
        // The date belongs to the post, so read it before the kind filter --
        // otherwise a photo-only post in a videos-only sync stops contributing
        // its date and the caller's stop heuristic goes blind.
        let Some(first) = all_items.first() else {
            continue;
        };

        // Undated posts are kept: dropping them would silently lose references
        // over a parsing detail.
        if !first.date.is_empty() {
            if newest.as_deref().is_none_or(|n| first.date.as_str() > n) {
                newest = Some(first.date.clone());
            }
            // Skip, never stop. Bookmarks arrive in the order they were saved,
            // not the order they were written, so an out-of-window post says
            // nothing at all about the ones behind it.
            if opts
                .from
                .as_deref()
                .is_some_and(|f| first.date.as_str() < f)
            {
                continue;
            }
            if opts.to.as_deref().is_some_and(|t| first.date.as_str() > t) {
                continue;
            }
        }

        for item in all_items {
            if !opts.wants(item.kind) {
                continue;
            }
            out.push(item);
            if out.len() >= opts.limit {
                break;
            }
        }
    }

    PageSummary { newest }
}

// --- single posts ---

/// Media attached to one public post, by id.
///
/// Backs pasting a link. Deliberately a free function rather than a method on
/// [`XClient`]: this route needs no session at all, so pasting a link works
/// before X is connected and keeps working after the cookies expire.
///
/// It reads the endpoint that serves embedded posts on other people's websites.
/// The timeline API is not an option any more -- every GraphQL operation in X's
/// web bundle was enumerated (630 chunks, 191 operations) and not one of them
/// reads a single post by id; `TweetResultByRestId` and `TweetDetail` are both
/// gone. The embed endpoint is a different service, still public, and returns
/// strictly more than the page's meta tags do: the real mp4 variants rather
/// than a preview image.
///
/// The request carries no cookies. It goes to a different host than the
/// timeline API, and sending the session there would hand X's CDN -- and
/// anything that could impersonate it -- a live login.
pub fn public_post(status_id: &str) -> Result<Vec<BookmarkMedia>> {
    if status_id.is_empty()
        || !status_id.bytes().all(|b| b.is_ascii_digit())
        || status_id.len() > 32
    {
        return Err(Error::X(format!("{status_id:?} is not a post id")));
    }

    let http = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(30))
        // No redirects: the response is JSON from a known host, and following a
        // hop would be a way to move this request somewhere else entirely.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::X(e.to_string()))?;

    let url = format!(
        "https://cdn.syndication.twimg.com/tweet-result?id={}&token={}&lang=en",
        status_id,
        syndication_token(status_id)
    );
    let resp = http
        .get(&url)
        .header("referer", "https://platform.twitter.com/")
        .send()
        .map_err(|e| Error::X(format!("reading post {status_id}: {e}")))?;

    let status = resp.status().as_u16();
    if status == 404 {
        return Err(Error::X(format!(
            "post {status_id} is not public — protected accounts and deleted posts \
             cannot be read this way. Bookmark it on X and use Sync from X instead."
        )));
    }
    if !resp.status().is_success() {
        return Err(Error::X(format!("reading post {status_id}: HTTP {status}")));
    }
    let body: Value = resp
        .json()
        .map_err(|e| Error::X(format!("post {status_id}: {e}")))?;

    Ok(media_from_tweet(&syndication_to_legacy(&body)))
}

/// Reshapes an embed response into the timeline shape.
///
/// The two carry the same media objects under different names, so converting is
/// cheaper than a second extractor -- and it means a pasted link goes through
/// exactly the parsing that bookmark sync does, including picking the highest
/// bitrate mp4 and asking for photos at original size.
fn syndication_to_legacy(body: &Value) -> Value {
    let media = body
        .get("mediaDetails")
        .or_else(|| body.pointer("/extended_entities/media"))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));

    serde_json::json!({
        "legacy": {
            "id_str": body.get("id_str").and_then(|v| v.as_str()).unwrap_or(""),
            "created_at": body.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
            "full_text": body.get("text").or_else(|| body.get("full_text"))
                .and_then(|v| v.as_str()).unwrap_or(""),
            "extended_entities": { "media": media },
        },
        "core": { "user_results": { "result": { "core": {
            "screen_name": body.pointer("/user/screen_name")
                .and_then(|v| v.as_str()).unwrap_or(""),
        }}}},
    })
}

/// The embed endpoint's anti-scrape token, derived from the post id.
///
/// `((id / 1e15) * PI)` in base 36 with zeros and the decimal point removed --
/// what X's own embed widget sends. It is not currently checked (a literal "a"
/// is accepted), but sending what the widget sends costs nothing and is the
/// difference between working and not on the day they start checking.
fn syndication_token(status_id: &str) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let Ok(id) = status_id.parse::<u64>() else {
        return "a".into();
    };
    let mut v = (id as f64 / 1e15) * std::f64::consts::PI;

    let mut int_part = v.trunc() as u64;
    v -= v.trunc();
    let mut int_s = String::new();
    if int_part == 0 {
        int_s.push('0');
    }
    while int_part > 0 {
        int_s.insert(0, DIGITS[(int_part % 36) as usize] as char);
        int_part /= 36;
    }

    let mut frac = String::new();
    for _ in 0..12 {
        v *= 36.0;
        let d = v.trunc() as usize;
        frac.push(DIGITS[d.min(35)] as char);
        v -= v.trunc();
        if v == 0.0 {
            break;
        }
    }
    format!("{int_s}{frac}")
        .chars()
        .filter(|c| *c != '0')
        .collect()
}

// --- parsing helpers ---

/// Lowercase chunk-name fragments likely to contain the given operations.
///
/// Chunk names track the feature rather than the exact operation --
/// `TweetResultByRestId` lives in a chunk named for `Tweet`, not for the whole
/// identifier -- so the leading CamelCase word is the part worth matching. It is
/// truncated because plurals differ between the two: the `Bookmarks` operation
/// sits in chunks named `Bookmark`.
fn chunk_keywords(operations: &[&str]) -> Vec<String> {
    const KEYWORD_LEN: usize = 6;
    let mut out: Vec<String> = Vec::new();
    for op in operations {
        // Leading word: characters up to the second uppercase letter.
        let mut end = op.len();
        for (i, c) in op.char_indices().skip(1) {
            if c.is_ascii_uppercase() {
                end = i;
                break;
            }
        }
        let word = op[..end].to_ascii_lowercase();
        let keyword: String = word.chars().take(KEYWORD_LEN).collect();
        if !keyword.is_empty() && !out.contains(&keyword) {
            out.push(keyword);
        }
    }
    out
}

fn balanced_brace_blocks(text: &str, count: usize) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut blocks = Vec::new();
    let mut i = 0;
    while blocks.len() < count && i < bytes.len() {
        let Some(start) = text[i..].find('{').map(|p| p + i) else {
            break;
        };
        let mut depth = 0usize;
        let mut j = start;
        let mut end = None;
        while j < bytes.len() {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(j);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        match end {
            Some(e) => {
                blocks.push(&text[start..=e]);
                i = e + 1;
            }
            None => break,
        }
    }
    blocks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BookmarkKind {
    Image,
    Video,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkMedia {
    pub tweet_id: String,
    pub tweet_url: String,
    pub author: String,
    pub text: String,
    pub kind: BookmarkKind,
    /// Highest-quality URL: best mp4 variant, or the original-size photo.
    pub media_url: String,
    /// Still image representing this item, when one is cheaper than the media.
    ///
    /// For video this is X's poster frame -- roughly 100KB against a 5MB mp4 --
    /// which is what makes it possible to show a real thumbnail and extract a
    /// real palette for a reference nobody has downloaded yet. For a photo it
    /// is `None`: the media *is* the image, and fetching a second copy at a
    /// smaller size would be pure waste.
    pub poster_url: Option<String>,
    /// File extension to save under.
    pub ext: &'static str,
    /// `YYYY-MM-DD` the post was created, for date filtering.
    pub date: String,
    /// Opaque position supplied by X's bookmark timeline. It orders saves, but
    /// is not documented as (and must not be displayed as) an exact timestamp.
    pub bookmark_sort_index: Option<String>,
    /// Position within the post; a tweet can carry up to four photos.
    pub index: usize,
}

impl BookmarkMedia {
    /// Whatever should be fetched to render a tile, without pulling the media.
    pub fn thumbnail_source(&self) -> &str {
        self.poster_url.as_deref().unwrap_or(&self.media_url)
    }

    /// Filename component, guaranteed not to escape its directory.
    ///
    /// Reads as `2026-08-05_Lovable_what-the-shopping-cart-looks-like_2085…_0`:
    /// date first so a folder sorts chronologically, then who posted it, then
    /// enough of the text to recognise it, and only then the id. The id stays
    /// because it is the only part that is unique -- two posts on one day by one
    /// author with the same opening words are not hypothetical.
    ///
    /// Every component is built from remote JSON, so each is filtered down to a
    /// known-safe alphabet rather than escaped. `tweet_id` in particular used to
    /// be interpolated straight into a path, where `../../evil` would have
    /// written outside the download directory; X's ids are decimal snowflakes,
    /// so anything else is either an attack or a parser change, and both are
    /// worth refusing rather than guessing at.
    pub fn safe_stem(&self) -> Option<String> {
        if self.tweet_id.is_empty()
            || !self.tweet_id.bytes().all(|b| b.is_ascii_digit())
            || self.tweet_id.len() > 32
        {
            return None;
        }

        let mut parts: Vec<String> = Vec::with_capacity(5);
        if !self.date.is_empty() {
            parts.push(self.date.clone());
        }
        // X handles are already [A-Za-z0-9_], but this string came off the wire.
        let author: String = self
            .author
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(16)
            .collect();
        if !author.is_empty() {
            parts.push(author);
        }
        let slug = slugify(&self.text, 48);
        if !slug.is_empty() {
            parts.push(slug);
        }
        parts.push(self.tweet_id.clone());
        parts.push(self.index.to_string());
        Some(parts.join("_"))
    }
}

/// Post text reduced to a lowercase hyphenated fragment of a filename.
///
/// Drops t.co links first: every post carrying media has one appended, and a
/// name ending in `https-t-co-dkqjsuutu9` is noise where a description should
/// be. Non-ASCII goes too -- emoji and CJK survive NTFS but not the round trip
/// through archives, shells, and other people's machines that a reference
/// library exists to feed.
fn slugify(text: &str, max: usize) -> String {
    let without_links: String = text
        .split_whitespace()
        .filter(|w| !w.starts_with("http://") && !w.starts_with("https://"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut out = String::with_capacity(max);
    let mut len = 0usize;
    let mut pending_sep = false;
    for c in without_links.chars() {
        if !c.is_ascii_alphanumeric() {
            // Any run of punctuation or space collapses to a single hyphen,
            // and only when something actually follows it.
            pending_sep = true;
            continue;
        }
        // The separator counts towards the budget, or the result overruns by
        // one whenever it lands on a word boundary.
        let cost = 1 + usize::from(pending_sep && len > 0);
        if len + cost > max {
            break;
        }
        if pending_sep && len > 0 {
            out.push('-');
        }
        pending_sep = false;
        out.push(c.to_ascii_lowercase());
        len += cost;
    }
    out.trim_matches('-').to_string()
}

/// Hosts X serves media from.
///
/// `media_url` comes out of a remote JSON document and is then fetched with the
/// session cookies attached. Without this check a crafted timeline response
/// could point it at an internal address and use the app as a confused deputy,
/// or at an attacker's host and hand over the auth cookie.
/// Whether a URL is X-hosted media, and so should be fetched with the session
/// client rather than the generic one.
pub fn is_x_media(url: &str) -> bool {
    is_twimg_host(url)
}

fn is_twimg_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    parsed.host_str().is_some_and(|h| {
        h == "twimg.com" || h.ends_with(".twimg.com") || h == "x.com" || h.ends_with(".x.com")
    })
}

/// Refuse to stream a single file larger than this.
///
/// X caps uploads well below it, so tripping this means the URL is not what it
/// claimed to be. Without a cap, a redirect to an endless response fills the
/// disk with no natural stopping point.
pub const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Why a bookmark walk ended.
///
/// Exists so the UI can tell "that is all there was" apart from "there is more,
/// ask for more". Those look identical from a count alone, and confusing them
/// is what makes a working sync feel like it is missing things.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// Collected everything that was asked for. More may remain.
    LimitReached,
    /// Walked off the end of the bookmark list.
    EndOfBookmarks,
    /// Hit [`MAX_PAGES`].
    PageCap,
    /// Several consecutive pages fell entirely before the start date.
    PastDateWindow,
}

impl StopReason {
    /// A sentence for the person who ran the sync.
    pub fn explain(&self) -> &'static str {
        match self {
            StopReason::LimitReached => "stopped at the number you asked for — there may be more",
            StopReason::EndOfBookmarks => "reached the end of your bookmarks",
            StopReason::PageCap => "stopped at the page limit — there may be more",
            StopReason::PastDateWindow => "reached bookmarks older than the start date",
        }
    }

    /// Whether asking again with a bigger number could return more.
    pub fn more_available(&self) -> bool {
        matches!(self, StopReason::LimitReached | StopReason::PageCap)
    }
}

/// The result of walking bookmarks, with enough context to explain itself.
pub struct Fetched {
    pub items: Vec<BookmarkMedia>,
    pub stop: StopReason,
    /// Timeline pages requested.
    pub pages: usize,
    /// Posts looked at, including ones carrying no media.
    pub posts_scanned: usize,
}

/// Which bookmarks to read.
pub enum BookmarkSource {
    /// Every bookmark, across all folders and loose ones.
    All,
    /// A single folder, by id.
    Folder(String),
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub limit: usize,
    /// Inclusive `YYYY-MM-DD` bounds. `None` means unbounded on that side.
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_images: bool,
    pub include_videos: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            limit: 50,
            from: None,
            to: None,
            // Both on by default: a sync that silently skipped half the
            // bookmarks would look like a bug rather than a setting.
            include_images: true,
            include_videos: true,
        }
    }
}

impl FetchOptions {
    fn wants(&self, kind: BookmarkKind) -> bool {
        match kind {
            BookmarkKind::Image => self.include_images,
            BookmarkKind::Video => self.include_videos,
        }
    }
}

/// A post's creation date as `YYYY-MM-DD`.
///
/// Accepts both forms X emits: `Sun Aug 02 11:34:55 +0000 2026` from the
/// timeline API and `2026-08-05T16:42:18.000Z` from the embed endpoint. One
/// parser rather than two, so a pasted link and a synced bookmark date-filter
/// identically.
///
/// Compared as strings: X always reports UTC, so lexicographic ordering on
/// `YYYY-MM-DD` is chronological and needs no date library.
fn parse_created_at(created_at: &str) -> Option<String> {
    // ISO 8601 already starts with the answer; validate rather than trust it.
    if let Some(head) = created_at.get(..10) {
        let b = head.as_bytes();
        if b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        {
            return Some(head.to_string());
        }
    }

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = created_at.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let month = MONTHS.iter().position(|m| *m == parts[1])? + 1;
    let day: u32 = parts[2].parse().ok()?;
    let year: i32 = parts[5].parse().ok()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn extract_tweets_and_cursor(data: &Value) -> (Vec<Value>, Option<String>) {
    let mut tweets = Vec::new();
    let mut cursor = None;

    // Folder timelines and the all-bookmarks timeline use different roots.
    let instructions = data
        .pointer("/bookmark_collection_timeline/timeline/instructions")
        .or_else(|| data.pointer("/bookmark_timeline_v2/timeline/instructions"))
        .or_else(|| data.pointer("/bookmark_timeline/timeline/instructions"))
        .and_then(|v| v.as_array());
    let Some(instructions) = instructions else {
        return (tweets, cursor);
    };

    for ins in instructions {
        let Some(entries) = ins.get("entries").and_then(|e| e.as_array()) else {
            continue;
        };
        for entry in entries {
            let Some(content) = entry.get("content") else {
                continue;
            };
            let etype = content
                .get("__typename")
                .or_else(|| content.get("entryType"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if etype.contains("TimelineItem") {
                if let Some(result) = content.pointer("/itemContent/tweet_results/result") {
                    let mut result = result.clone();
                    let sort_index = entry
                        .get("sortIndex")
                        .or_else(|| content.get("sortIndex"))
                        .and_then(|v| v.as_str());
                    if let (Some(object), Some(sort_index)) = (result.as_object_mut(), sort_index) {
                        object.insert(
                            "_burrow_bookmark_sort_index".to_string(),
                            Value::String(sort_index.to_string()),
                        );
                    }
                    tweets.push(result);
                }
            } else if etype.contains("Cursor")
                && content.get("cursorType").and_then(|v| v.as_str()) == Some("Bottom")
            {
                cursor = content
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
        }
    }
    (tweets, cursor)
}

/// Every downloadable image and video attached to a post.
///
/// Returns a list rather than one item: a post can carry up to four photos, and
/// taking only the first would quietly drop three references.
fn media_from_tweet(result: &Value) -> Vec<BookmarkMedia> {
    let bookmark_sort_index = result
        .get("_burrow_bookmark_sort_index")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // Quoted/retweeted posts nest the real tweet one level down.
    let tweet = result.get("tweet").unwrap_or(result);
    let Some(legacy) = tweet.get("legacy") else {
        return Vec::new();
    };
    let Some(tweet_id) = legacy
        .get("id_str")
        .and_then(|v| v.as_str())
        .or_else(|| tweet.get("rest_id").and_then(|v| v.as_str()))
    else {
        return Vec::new();
    };

    let author = tweet
        .pointer("/core/user_results/result/core/screen_name")
        .or_else(|| tweet.pointer("/core/user_results/result/legacy/screen_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let date = legacy
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(parse_created_at)
        .unwrap_or_default();

    let text = legacy
        .get("full_text")
        .or_else(|| legacy.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // extended_entities carries video and the full photo set; entities alone
    // often carries neither.
    let Some(media) = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (index, m) in media.iter().enumerate() {
        let kind_str = m.get("type").and_then(|t| t.as_str()).unwrap_or("");

        // On a video entity this is the poster frame, not the video. That is
        // the whole basis of a linked reference: a tile and a palette for the
        // cost of a JPEG.
        let poster = m
            .get("media_url_https")
            .or_else(|| m.get("media_url"))
            .and_then(|u| u.as_str());

        let found = match kind_str {
            // animated_gif is served as a silent mp4, not a .gif.
            "video" | "animated_gif" => best_mp4(m).map(|url| (BookmarkKind::Video, url, "mp4")),
            // Presence of video_info is the real signal; `type` is only a hint
            // and has been absent on nested/quoted results.
            "" => best_mp4(m).map(|url| (BookmarkKind::Video, url, "mp4")),
            // Without ?name=orig X serves a downscaled render -- 1200px wide
            // instead of the 2048px original.
            "photo" => poster.map(|u| (BookmarkKind::Image, format!("{u}?name=orig"), "jpg")),
            _ => None,
        };

        if let Some((kind, media_url, ext)) = found {
            out.push(BookmarkMedia {
                tweet_id: tweet_id.to_string(),
                tweet_url: format!("https://x.com/{author}/status/{tweet_id}"),
                author: author.clone(),
                text: text.clone(),
                kind,
                media_url,
                // A photo's media_url is already the image; a second smaller
                // copy would be fetched for nothing.
                poster_url: match kind {
                    BookmarkKind::Video => poster.map(|u| format!("{u}?name=small")),
                    BookmarkKind::Image => None,
                },
                ext,
                date: date.clone(),
                bookmark_sort_index: bookmark_sort_index.clone(),
                index,
            });
        }
    }
    out
}

fn best_mp4(media: &Value) -> Option<String> {
    let variants = media
        .pointer("/video_info/variants")
        .and_then(|v| v.as_array())?;
    let mut best: Option<(u64, String)> = None;
    for v in variants {
        // Skip the HLS playlist: it is a manifest, not a file, and would need a
        // muxer to turn into something the library can store.
        if v.get("content_type").and_then(|c| c.as_str()) != Some("video/mp4") {
            continue;
        }
        let bitrate = v.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0);
        let Some(url) = v.get("url").and_then(|u| u.as_str()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| bitrate > *b) {
            best = Some((bitrate, url.to_string()));
        }
    }
    best.map(|(_, url)| url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_blocks_reads_two_separate_literals() {
        // Shaped like the real loader: two objects in different paren groups.
        let src = r#"""+(({1:"a",2:"b"})[e]||e)+"."+({1:"h1",2:"h2"})[e]+"a.js""#;
        let blocks = balanced_brace_blocks(src, 2);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains(r#"1:"a""#));
        assert!(blocks[1].contains(r#"1:"h1""#), "second literal missed");
    }

    fn media(tweet_id: &str) -> BookmarkMedia {
        BookmarkMedia {
            tweet_id: tweet_id.to_string(),
            tweet_url: "https://x.com/a/status/1".into(),
            author: "a".into(),
            text: String::new(),
            kind: BookmarkKind::Video,
            media_url: "https://video.twimg.com/x.mp4".into(),
            poster_url: None,
            ext: "mp4",
            date: "2026-01-01".into(),
            bookmark_sort_index: None,
            index: 0,
        }
    }

    #[test]
    fn a_tweet_id_cannot_escape_the_download_directory() {
        // The id is remote input and used to be interpolated into a path.
        for hostile in ["../../evil", "..", "1/../../2", "a\\b", "", "1;rm -rf"] {
            assert!(
                media(hostile).safe_stem().is_none(),
                "{hostile:?} was accepted as a filename component"
            );
        }
    }

    #[test]
    fn a_filename_says_what_the_reference_is() {
        let mut m = media("2085043732903301438");
        m.author = "Lovable".into();
        m.date = "2026-08-05".into();
        m.text = "What should the shopping cart look like? https://t.co/DKQjsuutU9".into();
        assert_eq!(
            m.safe_stem().as_deref(),
            Some(
                "2026-08-05_Lovable_what-should-the-shopping-cart-look-like_2085043732903301438_0"
            )
        );
        // Date first, so a folder of these sorts chronologically.
        assert!(m.safe_stem().unwrap().starts_with("2026-08-05"));
    }

    #[test]
    fn a_filename_survives_posts_with_nothing_to_name_them_by() {
        // Every optional part missing at once: no date, no author, no text.
        let mut m = media("77");
        m.author = String::new();
        m.date = String::new();
        m.text = String::new();
        assert_eq!(m.safe_stem().as_deref(), Some("77_0"));

        // Emoji-only text leaves no ASCII behind, which must not produce a
        // stray separator or an empty component.
        let mut e = media("88");
        e.author = "someone".into();
        e.date = "2026-01-02".into();
        e.text = "🔥🔥🔥".into();
        assert_eq!(e.safe_stem().as_deref(), Some("2026-01-02_someone_88_0"));
    }

    #[test]
    fn a_slug_drops_links_and_collapses_punctuation() {
        // A t.co link is appended to every post carrying media; naming files
        // after it would describe nothing.
        assert_eq!(
            slugify("Look at this! https://t.co/abc", 48),
            "look-at-this"
        );
        assert_eq!(slugify("a---b   c", 48), "a-b-c");
        assert_eq!(slugify("!!!", 48), "");
        // Bounded, and never left with a trailing separator.
        let long = slugify(&"word ".repeat(40), 20);
        assert!(long.chars().count() <= 20, "got {long:?}");
        assert!(!long.ends_with('-'), "got {long:?}");
        // Nothing that could steer a path or need quoting survives.
        let hostile = slugify("../../etc/passwd; rm -rf ~", 48);
        assert_eq!(hostile, "etc-passwd-rm-rf");
    }

    #[test]
    fn a_page_keeps_collecting_past_a_post_older_than_the_window() {
        // The regression this whole split exists for. X returns bookmarks in
        // the order they were saved, not written: measured at 185 inversions
        // across 791 consecutive bookmarks, the first seven items into page 1.
        // The old code stopped the entire sync at that first old post.
        let page: Vec<Value> = ["2026-08-11", "2026-08-10", "2026-06-25", "2026-08-07"]
            .iter()
            .enumerate()
            .map(|(i, date)| dated_photo(i, date))
            .collect();

        let opts = FetchOptions {
            limit: 100,
            from: Some("2026-08-01".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let summary = collect_page(&page, &opts, &mut out);

        assert_eq!(
            out.len(),
            3,
            "the 2026-06-25 post must be skipped, not end the walk"
        );
        assert!(out.iter().all(|i| i.date.as_str() >= "2026-08-01"));
        // The stop heuristic reads the newest post on the page, including ones
        // the window rejected, so a jumbled page is judged by its best date.
        assert_eq!(summary.newest.as_deref(), Some("2026-08-11"));
    }

    #[test]
    fn a_page_entirely_before_the_window_reports_its_newest_date() {
        // How the caller recognises it has walked past the start date. It has
        // to be the newest on the page, or one stray recent post would reset
        // the count forever.
        let page: Vec<Value> = ["2024-03-01", "2024-05-02", "2024-01-01"]
            .iter()
            .enumerate()
            .map(|(i, d)| dated_photo(i, d))
            .collect();
        let opts = FetchOptions {
            limit: 100,
            from: Some("2026-01-01".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let summary = collect_page(&page, &opts, &mut out);
        assert!(out.is_empty());
        assert_eq!(summary.newest.as_deref(), Some("2024-05-02"));
    }

    #[test]
    fn an_upper_bound_skips_newer_posts_without_ending_the_walk() {
        let page: Vec<Value> = ["2026-08-11", "2025-01-01", "2026-08-10"]
            .iter()
            .enumerate()
            .map(|(i, d)| dated_photo(i, d))
            .collect();
        let opts = FetchOptions {
            limit: 100,
            to: Some("2025-06-01".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        collect_page(&page, &opts, &mut out);
        assert_eq!(out.len(), 1, "only the 2025 post is inside the window");
        assert_eq!(out[0].date, "2025-01-01");
    }

    #[test]
    fn collecting_stops_at_the_limit_without_dropping_the_rest_of_a_post() {
        let page: Vec<Value> = (0..5).map(|i| dated_photo(i, "2026-01-01")).collect();
        let opts = FetchOptions {
            limit: 3,
            ..Default::default()
        };
        let mut out = Vec::new();
        collect_page(&page, &opts, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn undated_posts_are_kept_rather_than_filtered_out() {
        // A parsing gap must not silently cost references.
        let tweet = serde_json::json!({
            "legacy": { "id_str": "5", "extended_entities": { "media": [
                { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/A.jpg" }
            ]}}
        });
        let opts = FetchOptions {
            limit: 10,
            from: Some("2026-01-01".into()),
            to: Some("2026-12-31".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let summary = collect_page(std::slice::from_ref(&tweet), &opts, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(summary.newest, None, "nothing dated the page");
    }

    /// A single-photo post on a given date.
    fn dated_photo(i: usize, date: &str) -> Value {
        serde_json::json!({
            "legacy": {
                "id_str": format!("{}", 1000 + i),
                "created_at": format!("{date}T00:00:00.000Z"),
                "extended_entities": { "media": [
                    { "type": "photo",
                      "media_url_https": format!("https://pbs.twimg.com/media/P{i}.jpg") }
                ]}
            }
        })
    }

    #[test]
    fn a_stop_reason_says_whether_asking_again_would_help() {
        assert!(StopReason::LimitReached.more_available());
        assert!(StopReason::PageCap.more_available());
        // These two mean there is genuinely nothing more to fetch, and telling
        // someone to "try a bigger number" would be a lie.
        assert!(!StopReason::EndOfBookmarks.more_available());
        assert!(!StopReason::PastDateWindow.more_available());
    }

    #[test]
    fn an_embed_response_yields_the_same_media_as_a_timeline_one() {
        // Shaped like the live response for the link that prompted this path.
        let body = serde_json::json!({
            "id_str": "2085043732903301438",
            "created_at": "2026-08-05T16:42:18.000Z",
            "text": "What should the shopping cart look like? https://t.co/DKQjsuutU9",
            "user": { "screen_name": "Lovable" },
            "mediaDetails": [{
                "type": "video",
                "media_url_https": "https://pbs.twimg.com/amplify_video_thumb/1/img/C.jpg",
                "video_info": { "variants": [
                    { "content_type": "application/x-mpegURL", "url": "https://x/p.m3u8" },
                    { "content_type": "video/mp4", "bitrate": 632000, "url": "https://x/low.mp4" },
                    { "content_type": "video/mp4", "bitrate": 10368000, "url": "https://x/4k.mp4" }
                ]}
            }]
        });
        let items = media_from_tweet(&syndication_to_legacy(&body));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, BookmarkKind::Video);
        // Same variant selection as a synced bookmark, which is the point of
        // reshaping rather than writing a second extractor.
        assert_eq!(items[0].media_url, "https://x/4k.mp4");
        assert_eq!(items[0].author, "Lovable");
        assert_eq!(items[0].date, "2026-08-05");
        assert_eq!(
            items[0].tweet_url,
            "https://x.com/Lovable/status/2085043732903301438"
        );
        assert!(items[0].poster_url.is_some(), "no poster means no tile");
    }

    #[test]
    fn the_embed_token_matches_what_x_generates() {
        // Verified against the live endpoint for this id.
        assert_eq!(syndication_token("2085043732903301438"), "51ycw2abihcskte");
        // Never panics on input that is not an id; the caller rejects those
        // first, but this must not be the thing that decides it.
        assert_eq!(syndication_token("not-a-number"), "a");
    }

    #[test]
    fn a_post_id_must_be_a_decimal_snowflake() {
        for hostile in ["", "../x", "1e9", "12345678901234567890123456789012345"] {
            assert!(
                public_post(hostile).is_err(),
                "{hostile:?} was accepted as a post id"
            );
        }
    }

    #[test]
    fn only_x_hosts_are_fetchable() {
        for ok in [
            "https://video.twimg.com/ext_tw_video/1.mp4",
            "https://pbs.twimg.com/media/a.jpg",
            "https://x.com/i/status/1",
        ] {
            assert!(is_twimg_host(ok), "{ok} should be allowed");
        }
        for bad in [
            // Cookies ride along on these requests, so an off-network host is
            // a credential leak, and a private address is an SSRF pivot.
            "https://evil.example/a.mp4",
            "http://video.twimg.com/a.mp4",  // plaintext
            "https://twimg.com.evil.test/a", // suffix confusion
            "https://127.0.0.1/a.mp4",
            "http://169.254.169.254/latest/meta-data/",
            "file:///C:/Windows/win.ini",
            "not a url",
        ] {
            assert!(!is_twimg_host(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn a_photo_uses_itself_as_its_thumbnail() {
        let mut m = media("1");
        m.kind = BookmarkKind::Image;
        m.media_url = "https://pbs.twimg.com/media/a.jpg?name=orig".into();
        assert_eq!(m.thumbnail_source(), m.media_url);

        // A video prefers the poster, which is why linking is cheap.
        let mut v = media("1");
        v.poster_url = Some("https://pbs.twimg.com/poster.jpg".into());
        assert_eq!(v.thumbnail_source(), "https://pbs.twimg.com/poster.jpg");
    }

    #[test]
    fn chunk_keywords_match_how_x_names_its_bundles() {
        // Regression guard: the candidate filter was hardcoded to "Bookmark",
        // so asking for any other operation scanned nothing and reported the
        // failure as "X changed its frontend".
        assert_eq!(chunk_keywords(&["Bookmarks"]), vec!["bookma"]);
        assert_eq!(chunk_keywords(&["TweetResultByRestId"]), vec!["tweet"]);
        assert_eq!(
            chunk_keywords(&["BookmarkFolderTimeline"]),
            vec!["bookma"],
            "should key off the leading word, not the whole identifier"
        );
        // Two operations sharing a prefix should not scan the same chunks twice.
        assert_eq!(
            chunk_keywords(&["Bookmarks", "BookmarkFoldersSlice"]),
            vec!["bookma"]
        );

        // The keywords have to actually appear in real chunk names.
        let keywords = chunk_keywords(&["TweetResultByRestId", "Bookmarks"]);
        for name in [
            "bundle.Tweet",
            "endpoints.TweetResultByRestId",
            "bundle.Bookmarks",
        ] {
            let lower = name.to_ascii_lowercase();
            assert!(
                keywords.iter().any(|k| lower.contains(k)),
                "{name} would not be scanned"
            );
        }
    }

    #[test]
    fn query_descriptor_discovery_tolerates_x_shape_changes() {
        let switch_re = regex::Regex::new(r#"featureSwitches:\[([^\]]*)\]"#).unwrap();
        let switch_value_re = regex::Regex::new(r#""([^"]+)""#).unwrap();

        let old = r#"({queryId:"old-id",operationName:"Bookmarks",featureSwitches:["one","two"]})"#;
        let old_spec = XClient::query_spec_in_chunk(old, "Bookmarks", &switch_re, &switch_value_re)
            .expect("old descriptor shape should parse");
        assert_eq!(old_spec.query_id, "old-id");
        assert_eq!(old_spec.feature_switches, ["one", "two"]);

        let nested = r#"({queryId:"new-id",operationType:"query",metadata:{featureSwitches:["one"]},operationName:"Bookmarks"})"#;
        let nested_spec =
            XClient::query_spec_in_chunk(nested, "Bookmarks", &switch_re, &switch_value_re)
                .expect("nested descriptor shape should parse");
        assert_eq!(nested_spec.query_id, "new-id");
        assert_eq!(nested_spec.feature_switches, ["one"]);

        let no_switches =
            r#"({queryId:"minimal-id",operationName:"Bookmarks",operationType:"query"})"#;
        let minimal =
            XClient::query_spec_in_chunk(no_switches, "Bookmarks", &switch_re, &switch_value_re)
                .expect("feature switches should be optional");
        assert_eq!(minimal.query_id, "minimal-id");
        assert!(minimal.feature_switches.is_empty());

        let adjacent = r#"({queryId:"wrong-id",operationName:"BirdwatchThing",metadata:{featureSwitches:[]}}),({queryId:"right-id",operationName:"Bookmarks",metadata:{featureSwitches:["needed"]}}),({queryId:"later-id",operationName:"LaterThing",metadata:{featureSwitches:["wrong"]}})"#;
        let adjacent_spec =
            XClient::query_spec_in_chunk(adjacent, "Bookmarks", &switch_re, &switch_value_re)
                .expect("the nearest query id should belong to Bookmarks");
        assert_eq!(adjacent_spec.query_id, "right-id");
        assert_eq!(adjacent_spec.feature_switches, ["needed"]);
    }

    #[test]
    fn brace_blocks_handles_nesting() {
        let blocks = balanced_brace_blocks(r#"{a:{b:1},c:2} then {d:3}"#, 2);
        assert_eq!(blocks[0], "{a:{b:1},c:2}");
        assert_eq!(blocks[1], "{d:3}");
    }

    #[test]
    fn brace_blocks_stops_cleanly_when_unbalanced() {
        assert!(balanced_brace_blocks("{unterminated", 2).is_empty());
        assert!(balanced_brace_blocks("no braces here", 2).is_empty());
    }

    #[test]
    fn picks_the_highest_bitrate_mp4_and_ignores_hls() {
        let tweet = serde_json::json!({
            "legacy": {
                "id_str": "123",
                "full_text": "look at this",
                "extended_entities": { "media": [{
                    "type": "video",
                    "video_info": { "variants": [
                        { "content_type": "application/x-mpegURL", "url": "https://x/playlist.m3u8" },
                        { "content_type": "video/mp4", "bitrate": 632000,  "url": "https://x/low.mp4" },
                        { "content_type": "video/mp4", "bitrate": 2176000, "url": "https://x/high.mp4" },
                        { "content_type": "video/mp4", "bitrate": 950000,  "url": "https://x/mid.mp4" }
                    ]}
                }]}
            },
            "core": { "user_results": { "result": { "core": { "screen_name": "someone" } } } }
        });
        let items = media_from_tweet(&tweet);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].media_url, "https://x/high.mp4");
        assert_eq!(items[0].author, "someone");
        assert_eq!(items[0].tweet_url, "https://x.com/someone/status/123");
    }

    #[test]
    fn created_at_parses_to_a_sortable_date() {
        assert_eq!(
            parse_created_at("Sun Aug 02 11:34:55 +0000 2026").as_deref(),
            Some("2026-08-02")
        );
        assert_eq!(
            parse_created_at("Wed Jan 05 00:00:01 +0000 2022").as_deref(),
            Some("2022-01-05")
        );
        // Zero-padding is what makes string comparison chronological.
        assert!(
            parse_created_at("Wed Jan 05 00:00:01 +0000 2022").unwrap()
                < parse_created_at("Sun Aug 02 11:34:55 +0000 2026").unwrap()
        );
        assert_eq!(parse_created_at("nonsense"), None);
        assert_eq!(parse_created_at("Sun Xxx 02 11:34:55 +0000 2026"), None);
    }

    #[test]
    fn photos_are_requested_at_original_size() {
        let tweet = serde_json::json!({
            "legacy": {
                "id_str": "5",
                "created_at": "Sun Aug 02 11:34:55 +0000 2026",
                "extended_entities": { "media": [{
                    "type": "photo",
                    "media_url_https": "https://pbs.twimg.com/media/ABC.jpg"
                }]}
            }
        });
        let items = media_from_tweet(&tweet);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, BookmarkKind::Image);
        assert_eq!(items[0].ext, "jpg");
        // Without ?name=orig X serves a 1200px render of a 2048px original.
        assert!(
            items[0].media_url.ends_with("?name=orig"),
            "got {}",
            items[0].media_url
        );
        assert_eq!(items[0].date, "2026-08-02");
    }

    #[test]
    fn all_four_photos_of_a_post_are_returned() {
        let media: Vec<_> = (0..4)
            .map(|i| {
                serde_json::json!({
                    "type": "photo",
                    "media_url_https": format!("https://pbs.twimg.com/media/P{i}.jpg")
                })
            })
            .collect();
        let tweet = serde_json::json!({
            "legacy": { "id_str": "7", "extended_entities": { "media": media } }
        });
        let items = media_from_tweet(&tweet);
        assert_eq!(items.len(), 4, "taking only the first would drop three");
        // Indexes must differ or the downloads overwrite each other.
        let indexes: Vec<usize> = items.iter().map(|i| i.index).collect();
        assert_eq!(indexes, vec![0, 1, 2, 3]);
    }

    #[test]
    fn animated_gifs_are_treated_as_video() {
        let tweet = serde_json::json!({
            "legacy": { "id_str": "8", "extended_entities": { "media": [{
                "type": "animated_gif",
                "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 0, "url": "https://x/g.mp4" }
                ]}
            }]}}
        });
        let items = media_from_tweet(&tweet);
        assert_eq!(items.len(), 1);
        // X serves these as silent mp4, not .gif.
        assert_eq!(items[0].kind, BookmarkKind::Video);
        assert_eq!(items[0].ext, "mp4");
    }

    #[test]
    fn a_post_with_both_a_photo_and_a_video_yields_both() {
        let tweet = serde_json::json!({
            "legacy": { "id_str": "9", "extended_entities": { "media": [
                { "type": "photo", "media_url_https": "https://x/a.jpg" },
                { "type": "video", "video_info": { "variants": [
                    { "content_type": "video/mp4", "bitrate": 100, "url": "https://x/b.mp4" }
                ]}}
            ]}}
        });
        let items = media_from_tweet(&tweet);
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.kind == BookmarkKind::Image));
        assert!(items.iter().any(|i| i.kind == BookmarkKind::Video));
    }

    #[test]
    fn kind_filter_selects_what_is_wanted() {
        let images_only = FetchOptions {
            include_videos: false,
            ..Default::default()
        };
        assert!(images_only.wants(BookmarkKind::Image));
        assert!(!images_only.wants(BookmarkKind::Video));

        let videos_only = FetchOptions {
            include_images: false,
            ..Default::default()
        };
        assert!(videos_only.wants(BookmarkKind::Video));
        assert!(!videos_only.wants(BookmarkKind::Image));

        // The default must take everything.
        let both = FetchOptions::default();
        assert!(both.wants(BookmarkKind::Image) && both.wants(BookmarkKind::Video));
    }

    #[test]
    fn a_post_with_no_media_yields_nothing() {
        let text_only = serde_json::json!({ "legacy": { "id_str": "2" } });
        assert!(media_from_tweet(&text_only).is_empty());

        // A link card is not downloadable media.
        let link = serde_json::json!({
            "legacy": { "id_str": "3", "extended_entities": { "media": [
                { "type": "something_else", "media_url_https": "https://x/x.bin" }
            ]}}
        });
        assert!(media_from_tweet(&link).is_empty());
    }

    #[test]
    fn nested_quoted_tweets_are_unwrapped() {
        let wrapped = serde_json::json!({ "tweet": {
            "legacy": {
                "id_str": "9",
                "extended_entities": { "media": [{
                    "video_info": { "variants": [
                        { "content_type": "video/mp4", "bitrate": 1, "url": "https://x/a.mp4" }
                    ]}
                }]}
            }
        }});
        assert_eq!(media_from_tweet(&wrapped)[0].tweet_id, "9");
    }

    #[test]
    fn timeline_extraction_finds_tweets_and_the_bottom_cursor() {
        let data = serde_json::json!({ "bookmark_collection_timeline": { "timeline": {
            "instructions": [{ "entries": [
                { "sortIndex": "10000000000000000001", "content": { "__typename": "TimelineTimelineItem",
                    "itemContent": { "tweet_results": { "result": { "legacy": { "id_str": "1" } } } } } },
                { "content": { "__typename": "TimelineTimelineItem",
                    "itemContent": { "tweet_results": { "result": { "legacy": { "id_str": "2" } } } } } },
                { "content": { "__typename": "TimelineTimelineCursor",
                    "cursorType": "Top", "value": "ignore-me" } },
                { "content": { "__typename": "TimelineTimelineCursor",
                    "cursorType": "Bottom", "value": "next-page" } }
            ]}]
        }}});
        let (tweets, cursor) = extract_tweets_and_cursor(&data);
        assert_eq!(tweets.len(), 2);
        assert_eq!(
            tweets[0]
                .get("_burrow_bookmark_sort_index")
                .and_then(Value::as_str),
            Some("10000000000000000001")
        );
        assert_eq!(
            cursor.as_deref(),
            Some("next-page"),
            "must take the Bottom cursor"
        );
    }

    #[test]
    fn an_unexpected_shape_yields_nothing_rather_than_panicking() {
        let (tweets, cursor) = extract_tweets_and_cursor(&serde_json::json!({ "nope": 1 }));
        assert!(tweets.is_empty());
        assert!(cursor.is_none());
    }
}
