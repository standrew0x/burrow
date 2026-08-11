//! Tests non-GraphQL routes to a single public post.
//!
//!   cargo run --example single_post_probe -- <library-root> <status-id>
//!
//! The GraphQL bundle carries no single-post operation any more (verified by
//! enumerating all 630 chunks). These are the remaining candidates.

use burrow_lib::store::Library;
use burrow_lib::xsync::XSession;
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

/// The embed endpoint's anti-scrape token: derived from the id, no auth.
///
/// `((id / 1e15) * PI)` in base 36 with zeros and the dot stripped.
fn syndication_token(id: u64) -> String {
    let v = (id as f64 / 1e15) * std::f64::consts::PI;
    let s = to_base36(v);
    s.chars().filter(|c| *c != '0' && *c != '.').collect()
}

/// JS `Number.prototype.toString(36)`, enough digits to match.
fn to_base36(mut v: f64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let int_part = v.trunc();
    v -= int_part;
    let mut int_s = String::new();
    let mut n = int_part as u64;
    if n == 0 {
        int_s.push('0');
    }
    while n > 0 {
        int_s.insert(0, DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    let mut frac = String::new();
    // JS emits ~ 11 base-36 fraction digits for these magnitudes.
    for _ in 0..12 {
        v *= 36.0;
        let d = v.trunc() as usize;
        frac.push(DIGITS[d.min(35)] as char);
        v -= v.trunc();
        if v == 0.0 {
            break;
        }
    }
    format!("{int_s}.{frac}")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .expect("usage: single_post_probe <root> <status-id>");
    let id_str = args.next().unwrap_or_else(|| "2085043732903301438".into());
    let id: u64 = id_str.parse().expect("numeric status id");

    let lib = Library::open(&root).expect("open library");
    let session = XSession::load(&lib).ok();

    let http = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    // --- 1. syndication embed endpoint, with the derived token ---
    let token = syndication_token(id);
    println!("derived syndication token: {token}");
    for t in [token.as_str(), "a"] {
        let url =
            format!("https://cdn.syndication.twimg.com/tweet-result?id={id}&token={t}&lang=en");
        match http
            .get(&url)
            .header("referer", "https://platform.twitter.com/")
            .send()
        {
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().unwrap_or_default();
                println!(
                    "\n[syndication token={t}] HTTP {status}  {} bytes",
                    body.len()
                );
                if status == 200 {
                    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    report_media(&v);
                } else {
                    println!("  {}", head(&body, 200));
                }
            }
            Err(e) => println!("[syndication token={t}] failed: {e}"),
        }
    }

    // --- 2. legacy REST v1.1 through the web client's own host ---
    if let Some(s) = &session {
        let url = format!(
            "https://x.com/i/api/1.1/statuses/show.json?id={id}&tweet_mode=extended&include_entities=true"
        );
        let r = http
            .get(&url)
            .header("authorization", "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA")
            .header("x-twitter-auth-type", "OAuth2Session")
            .header("x-twitter-active-user", "yes")
            .header("x-csrf-token", &s.ct0)
            .header("cookie", format!("auth_token={}; ct0={}", s.auth_token, s.ct0))
            .send();
        match r {
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().unwrap_or_default();
                println!(
                    "\n[REST 1.1 statuses/show] HTTP {status}  {} bytes",
                    body.len()
                );
                if status == 200 {
                    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    report_media(&v);
                } else {
                    println!("  {}", head(&body, 300));
                }
            }
            Err(e) => println!("[REST 1.1] failed: {e}"),
        }
    }

    // --- 3. plain page fetch, for OpenGraph ---
    for ua in [UA, "facebookexternalhit/1.1", "Twitterbot/1.0"] {
        let url = format!("https://x.com/i/status/{id}");
        match http.get(&url).header("user-agent", ua).send() {
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().unwrap_or_default();
                let og = body.contains("og:image");
                println!(
                    "\n[page ua={}] HTTP {status}  {} bytes  og:image={og}",
                    &ua[..ua.len().min(24)],
                    body.len()
                );
            }
            Err(e) => println!("[page] failed: {e}"),
        }
    }
}

fn report_media(v: &Value) {
    let media = v
        .pointer("/mediaDetails")
        .or_else(|| v.pointer("/extended_entities/media"))
        .or_else(|| v.pointer("/entities/media"));
    match media.and_then(|m| m.as_array()) {
        Some(items) => {
            println!("  media items: {}", items.len());
            for m in items {
                let t = m.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                let poster = m
                    .get("media_url_https")
                    .and_then(|u| u.as_str())
                    .unwrap_or("-");
                let best = m
                    .pointer("/video_info/variants")
                    .and_then(|x| x.as_array())
                    .map(|vs| {
                        vs.iter()
                            .filter(|x| {
                                x.get("content_type").and_then(|c| c.as_str()) == Some("video/mp4")
                            })
                            .max_by_key(|x| x.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0))
                            .and_then(|x| x.get("url").and_then(|u| u.as_str()))
                            .unwrap_or("-")
                            .to_string()
                    })
                    .unwrap_or_else(|| "-".into());
                println!("    type={t}\n      poster={poster}\n      mp4={best}");
            }
            let user = v
                .pointer("/user/screen_name")
                .and_then(|u| u.as_str())
                .unwrap_or("?");
            let text = v
                .get("text")
                .or_else(|| v.get("full_text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let created = v.get("created_at").and_then(|c| c.as_str()).unwrap_or("?");
            println!(
                "  author=@{user}  created={created}\n  text={}",
                head(text, 120)
            );
        }
        None => println!("  no media array; top-level keys: {:?}", keys(v)),
    }
}

fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
