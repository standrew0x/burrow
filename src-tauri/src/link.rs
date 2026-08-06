//! Turning a pasted URL into a reference.
//!
//! Three shapes of link have to work:
//!
//! * a direct media file -- `.../clip.mp4`, `.../shot.jpg`
//! * an X post -- handled by [`crate::xsync`], which already knows how to read
//!   the timeline API
//! * any other page -- read its OpenGraph tags for a preview image
//!
//! Everything here fetches with a client that carries **no cookies**. The X
//! client attaches session credentials to its requests; sending those to an
//! arbitrary host because someone pasted a link would hand over the account.
//! Keeping the two clients separate is the mechanism that prevents it, so the
//! generic path must never be given the X client.

use std::io::Read;
use std::net::IpAddr;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::ingest::MediaKind;

/// Enough of a page to find its meta tags without reading a whole document.
/// OpenGraph lives in `<head>`; a megabyte is far past any real one.
const MAX_HTML_BYTES: u64 = 1024 * 1024;

/// Cap on a fetched preview image.
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(45);

/// Sent on generic fetches. A default reqwest agent gets 403d by a noticeable
/// share of sites, which reads as "that link is broken" rather than "that site
/// dislikes the client".
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/131.0.0.0 Safari/537.36";

/// A link, resolved far enough to make a tile out of.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The page a human would open: the post, the video page, or -- for a bare
    /// media URL -- the file itself.
    pub page_url: String,
    /// The media file this reference stands for: a video, or -- when no video
    /// file is reachable -- the preview image itself.
    pub media_url: String,
    pub kind: MediaKind,
    pub title: Option<String>,
    /// Still image bytes, in whatever format the source served.
    pub thumbnail: Vec<u8>,
}

/// Rejects URLs that are not safely fetchable.
///
/// Two separate concerns:
///
///   * scheme -- `file:`, `data:` and friends are not network resources, and
///     handing them to a fetcher (or to ffmpeg) reads local disk.
///   * address -- a URL naming a private, loopback or link-local address turns
///     this app into a confused deputy against the user's own network. The
///     cloud metadata endpoint at 169.254.169.254 is the canonical target.
///
/// The address check resolves DNS and inspects every answer. That leaves a
/// rebinding window -- the name could resolve differently when the request is
/// actually made -- which is not closable without controlling the socket. It
/// raises the cost of the attack substantially and is worth having; it is not
/// airtight, and is documented as such rather than assumed to be.
pub fn check_fetchable(url: &str) -> Result<reqwest::Url> {
    let parsed =
        reqwest::Url::parse(url).map_err(|e| Error::Link(format!("{url:?} is not a URL: {e}")))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Link(format!(
            "{} links are not supported; paste an http or https URL",
            parsed.scheme()
        )));
    }

    let Some(host) = parsed.host_str() else {
        return Err(Error::Link(format!("{url:?} has no host")));
    };

    // A literal IP needs no lookup, and must not get one: resolving "127.0.0.1"
    // as a name could return something else entirely.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public(ip) {
            Ok(parsed)
        } else {
            Err(Error::Link(format!(
                "refusing to fetch private address {ip}"
            )))
        };
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .map_err(|e| Error::Link(format!("could not resolve {host}: {e}")))?
        .map(|s| s.ip())
        .collect();

    if addrs.is_empty() {
        return Err(Error::Link(format!("{host} resolved to no addresses")));
    }
    // Every answer must be public. One private answer among several is the
    // shape a rebinding attack takes, so "any" would be the wrong quantifier.
    if let Some(bad) = addrs.iter().find(|ip| !is_public(**ip)) {
        return Err(Error::Link(format!(
            "refusing to fetch {host}: it resolves to the private address {bad}"
        )));
    }
    Ok(parsed)
}

/// Whether an address is out on the internet rather than on this machine or
/// this network.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || v4.is_unspecified()
                // 100.64.0.0/10, carrier-grade NAT. `is_shared` is unstable, so
                // it is spelled out.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                // 192.0.0.0/24, IETF protocol assignments.
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0))
        }
        IpAddr::V6(v6) => {
            // Map v4-in-v6 back and judge it as v4; ::ffff:127.0.0.1 is loopback
            // however it is spelled.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // fc00::/7 unique-local, fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// A cookieless HTTP client that re-checks every redirect hop.
///
/// The hop check is the point: validating only the URL the user pasted leaves
/// an open door, because a public host is free to redirect to 127.0.0.1.
fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            match check_fetchable(attempt.url().as_str()) {
                Ok(_) => attempt.follow(),
                Err(e) => attempt.error(e.to_string()),
            }
        }))
        .build()
        .map_err(|e| Error::Link(format!("could not build an HTTP client: {e}")))
}

/// Resolves a pasted URL.
///
/// `x` is the authenticated X client, when one is configured. X post URLs need
/// it -- the media lives behind the timeline API, not in the page's meta tags,
/// because the page is a JavaScript shell. Without a session those links fall
/// through to the generic path, which still finds a preview image.
pub fn resolve(url: &str, x: Option<&crate::xsync::XClient>) -> Result<Resolved> {
    let parsed = check_fetchable(url)?;

    // A link to a single X post.
    //
    // There is no working way to read one as of this writing, and the fallbacks
    // were each measured rather than assumed:
    //
    //   * the timeline API has no single-post operation left. Enumerating every
    //     GraphQL operation in X's web bundle -- 623 chunks -- turned up no
    //     `TweetResultByRestId`, no `TweetDetail`, nothing that fetches a post
    //     by id.
    //   * the page itself returns 404 to anything that is not X's own web app,
    //     including Chrome, Googlebot, Twitterbot and facebookexternalhit, so
    //     there are no OpenGraph tags to read either.
    //   * cdn.syndication.twimg.com, which used to serve embeds publicly, 404s
    //     with or without a valid token.
    //
    // The attempt is still made, so this starts working again by itself if X
    // restores an operation. When it fails the message points at the path that
    // does work rather than reporting a GraphQL detail nobody can act on.
    if let Some(status_id) = x_status_id(&parsed) {
        if let Some(client) = x {
            match resolve_x_post(&parsed, &status_id, client) {
                Ok(resolved) => return Ok(resolved),
                Err(e) => {
                    return Err(Error::Link(format!(
                        "X no longer lets anything but its own web app read a single post, so this \
                         link cannot be added directly. Bookmark it on X and use Sync from X, which \
                         still works. ({e})"
                    )))
                }
            }
        }
        return Err(Error::Link(
            "Connect your X account to add posts, then bookmark this one on X and use Sync from X \
             — single post links cannot be read without a session."
                .into(),
        ));
    }

    let http = client()?;
    let head = describe(&http, parsed.as_str())?;

    match head.kind {
        Some(MediaKind::Image) => {
            let bytes = get_capped(&http, parsed.as_str(), MAX_IMAGE_BYTES)?;
            Ok(Resolved {
                page_url: parsed.to_string(),
                media_url: parsed.to_string(),
                kind: MediaKind::Image,
                title: file_stem_of(&parsed),
                thumbnail: bytes,
            })
        }
        Some(MediaKind::Video) => {
            // ffmpeg pulls a frame over a range request rather than the whole
            // file, so a linked video costs a few hundred KB even when nothing
            // has published a poster for it.
            let info = crate::video::probe_source(crate::video::Source::Url(parsed.as_str()))?;
            let duration = info.as_ref().map(|i| i.duration_ms).unwrap_or(0);
            let frame = crate::video::extract_poster_frame_from(
                crate::video::Source::Url(parsed.as_str()),
                duration,
            )?;
            Ok(Resolved {
                page_url: parsed.to_string(),
                media_url: parsed.to_string(),
                kind: MediaKind::Video,
                title: file_stem_of(&parsed),
                thumbnail: frame,
            })
        }
        None => resolve_page(&http, &parsed),
    }
}

/// What a URL serves, without downloading it.
struct Description {
    kind: Option<MediaKind>,
}

fn describe(http: &reqwest::blocking::Client, url: &str) -> Result<Description> {
    // HEAD first: it settles image-vs-video-vs-page without transferring a body.
    // Not every server implements it, so a failure here is not fatal.
    let content_type = match http.head(url).send() {
        Ok(resp) if resp.status().is_success() => {
            header_value(&resp, reqwest::header::CONTENT_TYPE)
        }
        _ => None,
    };

    let content_type = match content_type {
        Some(ct) => Some(ct),
        // Fall back to a GET, but read no body -- dropping the response closes
        // the connection before the transfer gets going.
        None => {
            let resp = http
                .get(url)
                .send()
                .map_err(|e| Error::Link(format!("could not reach {url}: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Link(format!(
                    "{url} returned HTTP {}",
                    resp.status().as_u16()
                )));
            }
            header_value(&resp, reqwest::header::CONTENT_TYPE)
        }
    };

    let kind = content_type.as_deref().and_then(|ct| {
        let ct = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if ct.starts_with("image/") {
            Some(MediaKind::Image)
        } else if ct.starts_with("video/") {
            Some(MediaKind::Video)
        } else {
            None
        }
    });

    Ok(Description { kind })
}

fn header_value(
    resp: &reqwest::blocking::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn get_capped(http: &reqwest::blocking::Client, url: &str, cap: u64) -> Result<Vec<u8>> {
    let resp = http
        .get(url)
        .send()
        .map_err(|e| Error::Link(format!("could not fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Link(format!(
            "{url} returned HTTP {}",
            resp.status().as_u16()
        )));
    }
    let mut buf = Vec::new();
    resp.take(cap)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Link(format!("could not read {url}: {e}")))?;
    if buf.is_empty() {
        return Err(Error::Link(format!("{url} served an empty response")));
    }
    Ok(buf)
}

/// Streams a URL to a file, for media that is not on X.
///
/// Carries no cookies, re-checks every redirect hop, and stops at
/// [`crate::xsync::MAX_DOWNLOAD_BYTES`] -- the same ceiling the X path uses,
/// because the reason for it (a response with no natural end fills the disk)
/// has nothing to do with which host is serving.
pub fn download_to(url: &str, dest: &std::path::Path) -> Result<u64> {
    let parsed = check_fetchable(url)?;
    let http = client()?;
    let resp = http
        .get(parsed.as_str())
        .send()
        .map_err(|e| Error::Link(format!("could not fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Link(format!(
            "{url} returned HTTP {}",
            resp.status().as_u16()
        )));
    }

    let cap = crate::xsync::MAX_DOWNLOAD_BYTES;
    if let Some(len) = resp.content_length() {
        if len > cap {
            return Err(Error::Link(format!(
                "refusing {url}: {len} bytes exceeds the {cap} byte cap"
            )));
        }
    }

    let mut file = std::fs::File::create(dest).map_err(|e| Error::io(dest, e))?;
    let written = std::io::copy(&mut resp.take(cap), &mut file).map_err(|e| Error::io(dest, e))?;

    if written == cap {
        // Stopped exactly at the ceiling, so the body was still coming. A
        // truncated video imports cleanly and only fails on playback, which is
        // a worse outcome than refusing it now.
        let _ = std::fs::remove_file(dest);
        return Err(Error::Link(format!("{url} exceeded the {cap} byte cap")));
    }
    Ok(written)
}

/// Reads a page's OpenGraph tags for something to show.
fn resolve_page(http: &reqwest::blocking::Client, page: &reqwest::Url) -> Result<Resolved> {
    let html = get_capped(http, page.as_str(), MAX_HTML_BYTES)?;
    let html = String::from_utf8_lossy(&html);
    let meta = MetaTags::parse(&html);

    let image = meta
        .first(&[
            "og:image:secure_url",
            "og:image:url",
            "og:image",
            "twitter:image",
        ])
        .ok_or_else(|| {
            Error::Link(format!(
                "{page} has no preview image; nothing to show for it"
            ))
        })?;

    // Meta URLs are routinely relative, and routinely carry &amp;.
    let image_url = page
        .join(&decode_entities(&image))
        .map_err(|e| Error::Link(format!("bad preview image URL on {page}: {e}")))?;
    check_fetchable(image_url.as_str())?;
    let thumbnail = get_capped(http, image_url.as_str(), MAX_IMAGE_BYTES)?;

    // og:video is usually a player embed rather than a file. Only trust it when
    // it names something a <video> could actually load.
    let playable = meta
        .first(&["og:video:secure_url", "og:video:url", "og:video"])
        .map(|v| decode_entities(&v))
        .filter(|v| looks_like_media_file(v))
        .and_then(|v| page.join(&v).ok());

    // When there is no reachable video file, the reference *is* the preview
    // image, so that is what gets stored as the media.
    //
    // The page URL was the obvious thing to put here and it is wrong: a link
    // whose media is a web page turns the download button into a request for
    // HTML, which then fails as "neither an image nor a video". Pointing at the
    // image instead makes downloading mean something -- it saves the preview at
    // full resolution rather than the 512px thumbnail. A YouTube link therefore
    // lands as an image reference; the video is still one click away through
    // "open original", which is what page_url is for.
    let (media_url, kind) = match &playable {
        Some(url) => (url.to_string(), MediaKind::Video),
        None => (image_url.to_string(), MediaKind::Image),
    };

    Ok(Resolved {
        page_url: page.to_string(),
        media_url,
        kind,
        title: meta
            .first(&["og:title", "twitter:title"])
            .map(|t| decode_entities(&t)),
        thumbnail,
    })
}

/// The X status id in a post URL, if this is one.
fn x_status_id(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?.trim_start_matches("www.");
    if !matches!(
        host,
        "x.com" | "twitter.com" | "mobile.x.com" | "mobile.twitter.com"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    // /<user>/status/<id>
    let pos = segments
        .iter()
        .position(|s| *s == "status" || *s == "statuses")?;
    let id = segments.get(pos + 1)?;
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Some((*id).to_string())
    } else {
        None
    }
}

fn resolve_x_post(
    url: &reqwest::Url,
    status_id: &str,
    x: &crate::xsync::XClient,
) -> Result<Resolved> {
    let items = x.tweet_media(status_id)?;
    let item = items
        .into_iter()
        .next()
        .ok_or_else(|| Error::Link(format!("{url} has no image or video attached")))?;

    let thumbnail = x.fetch_thumbnail(&item)?;
    Ok(Resolved {
        page_url: item.tweet_url.clone(),
        media_url: item.media_url.clone(),
        kind: match item.kind {
            crate::xsync::BookmarkKind::Video => MediaKind::Video,
            crate::xsync::BookmarkKind::Image => MediaKind::Image,
        },
        title: Some(if item.text.is_empty() {
            format!("@{}", item.author)
        } else {
            format!("@{} — {}", item.author, truncate(&item.text, 80))
        }),
        thumbnail,
    })
}

fn truncate(s: &str, max: usize) -> String {
    let cleaned = s.replace(['\n', '\r'], " ");
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    let cut: String = cleaned.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

fn file_stem_of(url: &reqwest::Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    if last.is_empty() {
        return None;
    }
    Some(
        urlencoding::decode(last)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| last.to_string()),
    )
}

/// Whether a URL names a media file rather than a player page.
fn looks_like_media_file(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    [".mp4", ".m4v", ".webm", ".mov", ".ogv"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

// --- meta tags ---

/// The `<meta>` tags of a document, keyed by `property` or `name`.
struct MetaTags(std::collections::HashMap<String, String>);

impl MetaTags {
    /// A deliberately small parser rather than a DOM.
    ///
    /// OpenGraph tags are flat, self-closing, and live in `<head>`; a full HTML
    /// parser would be a large dependency to answer one question. This reads
    /// `<meta ...>` elements and pulls two attributes out of each.
    fn parse(html: &str) -> Self {
        use std::sync::OnceLock;
        static META: OnceLock<regex::Regex> = OnceLock::new();
        static ATTR: OnceLock<regex::Regex> = OnceLock::new();

        // Compiled once: building these per call showed up as the dominant cost
        // when resolving several links at a time.
        let meta = META.get_or_init(|| regex::Regex::new(r"(?is)<meta\b([^>]*)>").unwrap());
        let attr = ATTR.get_or_init(|| {
            regex::Regex::new(r#"(?is)([a-z0-9_:\-]+)\s*=\s*("([^"]*)"|'([^']*)'|([^\s"'>]+))"#)
                .unwrap()
        });

        let mut out = std::collections::HashMap::new();
        // Only the head matters, and stopping there avoids scanning a megabyte
        // of body for tags that cannot be there.
        let head = match html.to_ascii_lowercase().find("</head") {
            Some(end) => &html[..end],
            None => html,
        };

        for tag in meta.captures_iter(head) {
            let mut key: Option<String> = None;
            let mut value: Option<String> = None;
            for a in attr.captures_iter(&tag[1]) {
                let name = a[1].to_ascii_lowercase();
                let val = a
                    .get(3)
                    .or_else(|| a.get(4))
                    .or_else(|| a.get(5))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                match name.as_str() {
                    "property" | "name" => key = Some(val.to_ascii_lowercase()),
                    "content" => value = Some(val),
                    _ => {}
                }
            }
            if let (Some(k), Some(v)) = (key, value) {
                // First wins: pages repeat og:image for multiple sizes and the
                // first is the primary one.
                out.entry(k).or_insert(v);
            }
        }
        Self(out)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).filter(|v| !v.trim().is_empty()).cloned()
    }

    fn first(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|k| self.get(k))
    }
}

/// Decodes the handful of entities that actually appear in meta URLs.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_urls_are_fetchable() {
        for bad in [
            "file:///C:/Windows/win.ini",
            "data:text/html,<script>",
            "javascript:alert(1)",
            "ftp://example.com/a",
            "not a url",
        ] {
            assert!(check_fetchable(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn private_addresses_are_refused() {
        // The app must not be usable as a probe of the user's own network.
        for bad in [
            "http://127.0.0.1/",
            "http://localhost:8080/", // resolves to loopback
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://10.0.0.1/",
            "http://192.168.1.1/admin",
            "http://172.16.5.4/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/", // loopback wearing a v6 hat
            "http://0.0.0.0/",
            "http://100.64.0.1/", // carrier-grade NAT
        ] {
            assert!(
                check_fetchable(bad).is_err(),
                "{bad} should be refused as private"
            );
        }
    }

    #[test]
    fn public_literals_are_allowed() {
        // No DNS involved, so this stays offline and deterministic.
        assert!(check_fetchable("https://1.1.1.1/").is_ok());
        assert!(check_fetchable("https://[2606:4700:4700::1111]/").is_ok());
    }

    #[test]
    fn x_post_urls_are_recognised() {
        let cases = [
            (
                "https://x.com/a16z/status/2084994967710433280",
                Some("2084994967710433280"),
            ),
            ("https://twitter.com/a16z/status/123", Some("123")),
            ("https://www.x.com/a/status/456?s=20", Some("456")),
            ("https://mobile.twitter.com/a/status/789", Some("789")),
            ("https://x.com/a16z", None),
            ("https://x.com/i/bookmarks", None),
            ("https://example.com/a/status/123", None),
            // Not a snowflake; refuse rather than pass junk to the API.
            ("https://x.com/a/status/abc", None),
        ];
        for (url, want) in cases {
            let got = x_status_id(&reqwest::Url::parse(url).unwrap());
            assert_eq!(got.as_deref(), want, "for {url}");
        }
    }

    #[test]
    fn meta_tags_are_read_in_either_attribute_order() {
        let html = r#"
            <html><head>
            <meta content="A title" property="og:title">
            <meta property='og:image' content='https://cdn.example/a.jpg?w=1&amp;h=2'>
            <meta name="twitter:image" content="https://cdn.example/b.jpg">
            <meta charset="utf-8">
            </head><body>
            <meta property="og:image" content="https://cdn.example/IGNORED.jpg">
            </body></html>"#;
        let meta = MetaTags::parse(html);

        assert_eq!(meta.get("og:title").as_deref(), Some("A title"));
        assert_eq!(
            decode_entities(&meta.get("og:image").unwrap()),
            "https://cdn.example/a.jpg?w=1&h=2"
        );
        // `name=` is as valid as `property=` and Twitter's tags use it.
        assert_eq!(
            meta.get("twitter:image").as_deref(),
            Some("https://cdn.example/b.jpg")
        );
        // Body content must not win over head content.
        assert!(!meta.get("og:image").unwrap().contains("IGNORED"));
    }

    #[test]
    fn the_first_og_image_wins() {
        // Pages list several sizes; the first is the primary.
        let html = r#"<head>
            <meta property="og:image" content="https://a/first.jpg">
            <meta property="og:image" content="https://a/second.jpg">
        </head>"#;
        assert_eq!(
            MetaTags::parse(html).get("og:image").as_deref(),
            Some("https://a/first.jpg")
        );
    }

    #[test]
    fn a_player_embed_is_not_treated_as_a_playable_file() {
        // The distinction the download button depends on.
        assert!(!looks_like_media_file(
            "https://www.youtube.com/embed/dQw4w9WgXcQ"
        ));
        assert!(!looks_like_media_file(
            "https://player.vimeo.com/video/12345"
        ));
        assert!(looks_like_media_file("https://cdn.example/clip.mp4"));
        assert!(looks_like_media_file(
            "https://cdn.example/clip.MP4?token=x"
        ));
        assert!(looks_like_media_file("https://cdn.example/a.webm#t=1"));
    }

    #[test]
    fn titles_are_truncated_on_character_boundaries() {
        // Byte slicing here would panic on the first multi-byte character.
        let s = "日本語のテキストがとても長い場合の切り詰め処理のテスト".repeat(4);
        let out = truncate(&s, 20);
        assert_eq!(out.chars().count(), 21, "20 chars plus the ellipsis");
        assert!(out.ends_with('…'));

        assert_eq!(truncate("short", 80), "short");
        assert_eq!(truncate("a\nb", 80), "a b", "newlines would break the tile");
    }

    #[test]
    fn file_stems_are_percent_decoded() {
        let url = reqwest::Url::parse("https://cdn.example/a/my%20clip.mp4").unwrap();
        assert_eq!(file_stem_of(&url).as_deref(), Some("my clip.mp4"));
    }
}
