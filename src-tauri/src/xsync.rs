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

    /// Videos in a folder, newest first, stopping after `limit`.
    pub fn folder_videos(
        &self,
        spec: &QuerySpec,
        folder_id: &str,
        limit: usize,
    ) -> std::result::Result<Vec<BookmarkVideo>, XError> {
        let mut out: Vec<BookmarkVideo> = Vec::new();
        let mut cursor: Option<String> = None;

        while out.len() < limit {
            let mut vars = serde_json::json!({
                "count": PAGE_SIZE,
                "bookmark_collection_id": folder_id,
            });
            if let Some(c) = &cursor {
                vars["cursor"] = Value::String(c.clone());
            }

            let data = self.graphql(spec, "BookmarkFolderTimeline", vars)?;
            let (tweets, next) = extract_tweets_and_cursor(&data);
            if tweets.is_empty() {
                break;
            }
            for tweet in &tweets {
                if let Some(v) = video_from_tweet(tweet) {
                    out.push(v);
                    if out.len() == limit {
                        break;
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

    /// Streams a video to `dir`, named by tweet id. Returns the written path.
    pub fn download(&self, video: &BookmarkVideo, dir: &Path) -> Result<PathBuf> {
        let dest = dir.join(format!("x_{}.mp4", video.tweet_id));
        let mut resp = self
            .http
            .get(&video.video_url)
            .send()
            .map_err(|e| Error::X(format!("downloading {}: {e}", video.tweet_url)))?;
        if !resp.status().is_success() {
            return Err(Error::X(format!(
                "downloading {} returned HTTP {}",
                video.tweet_url,
                resp.status().as_u16()
            )));
        }
        let mut file = std::fs::File::create(&dest).map_err(|e| Error::io(&dest, e))?;
        // Streamed, not buffered: these run to 150MB.
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkVideo {
    pub tweet_id: String,
    pub tweet_url: String,
    pub author: String,
    pub text: String,
    /// Highest-bitrate mp4 variant.
    pub video_url: String,
    pub bitrate: u64,
}

fn extract_tweets_and_cursor(data: &Value) -> (Vec<Value>, Option<String>) {
    let mut tweets = Vec::new();
    let mut cursor = None;

    let instructions = data
        .pointer("/bookmark_collection_timeline/timeline/instructions")
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

fn video_from_tweet(result: &Value) -> Option<BookmarkVideo> {
    // Quoted/retweeted posts nest the real tweet one level down.
    let tweet = result.get("tweet").unwrap_or(result);
    let legacy = tweet.get("legacy")?;
    let tweet_id = legacy
        .get("id_str")
        .and_then(|v| v.as_str())
        .or_else(|| tweet.get("rest_id").and_then(|v| v.as_str()))?
        .to_string();

    let author = tweet
        .pointer("/core/user_results/result/core/screen_name")
        .or_else(|| tweet.pointer("/core/user_results/result/legacy/screen_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // extended_entities carries video; entities alone often does not.
    let media = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .and_then(|v| v.as_array())?;

    let mut best: Option<(u64, String)> = None;
    for m in media {
        let Some(variants) = m.pointer("/video_info/variants").and_then(|v| v.as_array()) else {
            continue;
        };
        for v in variants {
            // Skip the HLS playlist: it is a manifest, not a file, and would
            // need a muxer to turn into something the library can store.
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
    }
    let (bitrate, video_url) = best?;

    Some(BookmarkVideo {
        tweet_url: format!("https://x.com/{author}/status/{tweet_id}"),
        tweet_id,
        author,
        text: legacy
            .get("full_text")
            .or_else(|| legacy.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        video_url,
        bitrate,
    })
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
        let v = video_from_tweet(&tweet).expect("should find a video");
        assert_eq!(v.video_url, "https://x/high.mp4");
        assert_eq!(v.bitrate, 2176000);
        assert_eq!(v.author, "someone");
        assert_eq!(v.tweet_url, "https://x.com/someone/status/123");
    }

    #[test]
    fn a_tweet_with_no_video_is_skipped() {
        let photo = serde_json::json!({
            "legacy": {
                "id_str": "1",
                "extended_entities": { "media": [{ "type": "photo",
                    "media_url_https": "https://x/p.jpg" }] }
            }
        });
        assert!(video_from_tweet(&photo).is_none());

        let text_only = serde_json::json!({ "legacy": { "id_str": "2" } });
        assert!(video_from_tweet(&text_only).is_none());
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
        assert_eq!(video_from_tweet(&wrapped).unwrap().tweet_id, "9");
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
