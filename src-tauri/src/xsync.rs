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

/// Hard cap on pages walked in one fetch. A narrow date window deep in the
/// past would otherwise page through the entire bookmark history; better to
/// return what was found than to hammer X indefinitely.
const MAX_PAGES: usize = 40;

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

        // Shared chunks first: they carry several operations at once, so the
        // common case resolves everything in one fetch.
        let mut candidates: Vec<&(String, String)> = names
            .iter()
            .filter(|(_, n)| n.contains("Bookmark"))
            .collect();
        candidates.sort_by_key(|(_, n)| !n.contains("shared"));

        // Compiled once, not per chunk per operation: regex construction is far
        // more expensive than the match itself.
        let switch_re = regex::Regex::new(r#""([^"]+)""#).unwrap();
        let op_res: Vec<(&str, regex::Regex)> = operations
            .iter()
            .map(|op| {
                let pattern = format!(
                    r#"queryId:"([^"]+)",operationName:"{}"[^}}]*?featureSwitches:\[([^\]]*)\]"#,
                    regex::escape(op)
                );
                (*op, regex::Regex::new(&pattern).unwrap())
            })
            .collect();

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

            for (op, re) in &op_res {
                if found_names.iter().any(|f| f == op) {
                    continue;
                }
                if let Some(c) = re.captures(&chunk) {
                    let switches = switch_re
                        .captures_iter(&c[2])
                        .map(|s| s[1].to_string())
                        .collect();
                    found.push(QuerySpec {
                        query_id: c[1].to_string(),
                        feature_switches: switches,
                    });
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

    /// Images and videos from bookmarks, newest first.
    ///
    /// The timeline is strictly newest-first, which makes the date window
    /// cheap: anything newer than `to` is skipped, and the first post older
    /// than `from` ends paging entirely rather than walking the whole history.
    pub fn fetch_bookmarks(
        &self,
        spec: &QuerySpec,
        source: &BookmarkSource,
        opts: &FetchOptions,
    ) -> std::result::Result<Vec<BookmarkMedia>, XError> {
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

        'paging: while out.len() < opts.limit && pages < MAX_PAGES {
            pages += 1;
            let mut vars = base_vars.clone();
            if let Some(c) = &cursor {
                vars["cursor"] = Value::String(c.clone());
            }

            let data = self.graphql(spec, op_name, vars)?;
            let (tweets, next) = extract_tweets_and_cursor(&data);
            if tweets.is_empty() {
                break;
            }

            for tweet in &tweets {
                let all_items = media_from_tweet(tweet);
                // Date is a property of the post, so read it before filtering by
                // kind -- otherwise a photo-only post in an images-excluded sync
                // would stop contributing its date and break the paging cutoff.
                let Some(first) = all_items.first() else {
                    continue;
                };

                // Undated posts are kept: dropping them would silently lose
                // references over a parsing detail.
                if !first.date.is_empty() {
                    if let Some(from) = &opts.from {
                        if first.date.as_str() < from.as_str() {
                            break 'paging;
                        }
                    }
                    if let Some(to) = &opts.to {
                        if first.date.as_str() > to.as_str() {
                            continue;
                        }
                    }
                }

                for item in all_items {
                    if !opts.wants(item.kind) {
                        continue;
                    }
                    out.push(item);
                    if out.len() >= opts.limit {
                        break 'paging;
                    }
                }
            }

            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(out)
    }

    /// Streams one media item to `dir`. Returns the written path.
    ///
    /// Named by tweet id plus position, so the four photos of a single post do
    /// not overwrite each other.
    pub fn download(&self, item: &BookmarkMedia, dir: &Path) -> Result<PathBuf> {
        let dest = dir.join(format!("x_{}_{}.{}", item.tweet_id, item.index, item.ext));
        let mut resp = self
            .http
            .get(&item.media_url)
            .send()
            .map_err(|e| Error::X(format!("downloading {}: {e}", item.tweet_url)))?;
        if !resp.status().is_success() {
            return Err(Error::X(format!(
                "downloading {} returned HTTP {}",
                item.tweet_url,
                resp.status().as_u16()
            )));
        }
        let mut file = std::fs::File::create(&dest).map_err(|e| Error::io(&dest, e))?;
        // Streamed, not buffered: videos run to 150MB.
        std::io::copy(&mut resp, &mut file).map_err(|e| Error::io(&dest, e))?;
        Ok(dest)
    }
}

// --- parsing helpers ---

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
    /// File extension to save under.
    pub ext: &'static str,
    /// `YYYY-MM-DD` the post was created, for date filtering.
    pub date: String,
    /// Position within the post; a tweet can carry up to four photos.
    pub index: usize,
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

/// `Sun Aug 02 11:34:55 +0000 2026` -> `2026-08-02`.
///
/// Compared as strings: X always reports +0000, so lexicographic ordering on
/// `YYYY-MM-DD` is chronological and needs no date library.
fn parse_created_at(created_at: &str) -> Option<String> {
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
                    tweets.push(result.clone());
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

        let found = match kind_str {
            // animated_gif is served as a silent mp4, not a .gif.
            "video" | "animated_gif" => best_mp4(m).map(|url| (BookmarkKind::Video, url, "mp4")),
            // Presence of video_info is the real signal; `type` is only a hint
            // and has been absent on nested/quoted results.
            "" => best_mp4(m).map(|url| (BookmarkKind::Video, url, "mp4")),
            "photo" => m
                .get("media_url_https")
                .or_else(|| m.get("media_url"))
                .and_then(|u| u.as_str())
                // Without ?name=orig X serves a downscaled render -- 1200px
                // wide instead of the 2048px original.
                .map(|u| (BookmarkKind::Image, format!("{u}?name=orig"), "jpg")),
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
                ext,
                date: date.clone(),
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
                { "content": { "__typename": "TimelineTimelineItem",
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
