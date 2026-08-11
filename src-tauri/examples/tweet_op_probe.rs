//! Hunts for a GraphQL operation that can read a single post.
//!
//!   cargo run --example tweet_op_probe -- <library-root> <status-id>
//!
//! Pasting an x.com/<user>/status/<id> link fails with "no queryId for
//! TweetResultByRestId". This checks whether that operation is genuinely absent
//! from the bundle or merely unreachable by the chunk filter, and whether any
//! other operation answers for a single post.

use std::collections::HashMap;

use burrow_lib::store::Library;
use burrow_lib::xsync::{XClient, XSession};
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

/// Every operation the web client is known to use for one post.
const CANDIDATES: &[&str] = &[
    "TweetResultByRestId",
    "TweetDetail",
    "TweetResultsByRestIds",
    "TweetResultByIdQuery",
    "ConversationTimelineV2",
];

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .expect("usage: tweet_op_probe <root> <status-id>");
    let status_id = args.next().unwrap_or_else(|| "2085043732903301438".into());

    let lib = Library::open(&root).expect("open library");
    let session = XSession::load(&lib).expect("load session");
    let auth_token = session.auth_token.clone();
    let ct0 = session.ct0.clone();
    let _client = XClient::new(session).expect("client");

    let http = reqwest::blocking::Client::builder()
        .user_agent(UA)
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();

    // --- 1. enumerate every chunk in the bundle, not just filtered ones ---
    let html = http
        .get("https://x.com/i/bookmarks")
        .send()
        .and_then(|r| r.text())
        .expect("bundle html");

    let anchor = regex::Regex::new(r"\b[A-Za-z_$][A-Za-z0-9_$]{0,2}\.u=e=>").unwrap();
    let m = anchor.find(&html).expect("chunk loader anchor");
    let tail = &html[m.end()..(m.end() + 400_000).min(html.len())];
    let blocks = brace_blocks(tail, 2);
    let entry = regex::Regex::new(r#"(\d+):"([^"]*)""#).unwrap();
    let names: Vec<(String, String)> = entry
        .captures_iter(blocks[0])
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect();
    let hashes: HashMap<String, String> = entry
        .captures_iter(blocks[1])
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect();
    println!("{} chunks in the manifest\n", names.len());

    // Which chunk names would the current filter even look at?
    let looked_at = names
        .iter()
        .filter(|(_, n)| {
            let l = n.to_ascii_lowercase();
            l.contains("shared") || l.contains("tweet")
        })
        .count();
    println!("chunks the current 'tweet' filter would fetch: {looked_at}");

    // --- 2. scan EVERY chunk for the candidate operations ---
    let op_res: Vec<(&str, regex::Regex)> = CANDIDATES
        .iter()
        .map(|op| {
            let p = format!(
                r#"queryId:"([^"]+)",operationName:"{}"[^}}]*?featureSwitches:\[([^\]]*)\]"#,
                regex::escape(op)
            );
            (*op, regex::Regex::new(&p).unwrap())
        })
        .collect();
    // Also catch the operation named without the featureSwitches tail.
    let loose: Vec<(&str, regex::Regex)> = CANDIDATES
        .iter()
        .map(|op| {
            let p = format!(r#"operationName:"{}""#, regex::escape(op));
            (*op, regex::Regex::new(&p).unwrap())
        })
        .collect();

    let mut found: HashMap<&str, (String, Vec<String>)> = HashMap::new();
    let mut loose_hits: HashMap<&str, String> = HashMap::new();
    let switch_re = regex::Regex::new(r#""([^"]+)""#).unwrap();
    let mut fetched = 0usize;
    // Every operation the bundle declares, however it declares it. Guessing
    // names is how the last attempt went wrong; this enumerates instead.
    let any_op = regex::Regex::new(r#"operationName:"([A-Za-z0-9_]+)""#).unwrap();
    let qid_op = regex::Regex::new(r#"queryId:"([^"]+)",operationName:"([A-Za-z0-9_]+)""#).unwrap();
    let mut all_ops: std::collections::BTreeSet<String> = Default::default();
    let mut qid_ops: std::collections::BTreeMap<String, String> = Default::default();

    for (cid, cname) in &names {
        let Some(hash) = hashes.get(cid) else {
            continue;
        };
        let url = format!("https://abs.twimg.com/responsive-web/client-web/{cname}.{hash}a.js");
        let Ok(resp) = http.get(&url).send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(chunk) = resp.text() else { continue };
        fetched += 1;
        if fetched.is_multiple_of(100) {
            println!("  ...{fetched} chunks scanned");
        }
        for c in any_op.captures_iter(&chunk) {
            all_ops.insert(c[1].to_string());
        }
        for c in qid_op.captures_iter(&chunk) {
            qid_ops.insert(c[2].to_string(), c[1].to_string());
        }
        for (op, re) in &op_res {
            if found.contains_key(op) {
                continue;
            }
            if let Some(c) = re.captures(&chunk) {
                let switches: Vec<String> = switch_re
                    .captures_iter(&c[2])
                    .map(|s| s[1].to_string())
                    .collect();
                println!("FOUND {op} -> queryId {} in chunk {cname}", &c[1]);
                found.insert(op, (c[1].to_string(), switches));
            }
        }
        for (op, re) in &loose {
            if loose_hits.contains_key(op) || found.contains_key(op) {
                continue;
            }
            if re.is_match(&chunk) {
                println!("  (loose) operationName {op} appears in chunk {cname}");
                loose_hits.insert(op, cname.clone());
            }
        }
    }
    println!("\nscanned {fetched} chunks");
    println!("strict matches: {:?}", found.keys().collect::<Vec<_>>());
    println!(
        "loose-only matches: {:?}",
        loose_hits.keys().collect::<Vec<_>>()
    );
    println!("\n{} operationName values in the bundle", all_ops.len());
    println!("{} of them carry a queryId", qid_ops.len());
    let tweety: Vec<&String> = all_ops
        .iter()
        .filter(|o| {
            let l = o.to_ascii_lowercase();
            l.contains("tweet")
                || l.contains("post")
                || l.contains("status")
                || l.contains("detail")
        })
        .collect();
    println!(
        "\noperations mentioning tweet/post/status/detail ({}):",
        tweety.len()
    );
    for op in &tweety {
        match qid_ops.get(*op) {
            Some(q) => println!("   {op}  queryId={q}"),
            None => println!("   {op}  (no queryId nearby)"),
        }
    }

    // --- 3. actually call whatever was found ---
    for (op, (qid, switches)) in &found {
        let features: serde_json::Map<String, Value> = switches
            .iter()
            .map(|s| (s.clone(), Value::Bool(true)))
            .collect();
        let vars = match *op {
            "TweetDetail" => serde_json::json!({
                "focalTweetId": status_id,
                "with_rux_injections": false,
                "includePromotedContent": false,
                "withCommunity": true,
                "withQuickPromoteEligibilityTweetFields": false,
                "withBirdwatchNotes": false,
                "withVoice": false,
            }),
            "TweetResultsByRestIds" => serde_json::json!({
                "tweetIds": [status_id.clone()],
                "includePromotedContent": false,
            }),
            _ => serde_json::json!({
                "tweetId": status_id,
                "includePromotedContent": false,
                "withCommunity": false,
                "withVoice": false,
            }),
        };
        let url = format!(
            "https://x.com/i/api/graphql/{qid}/{op}?variables={}&features={}",
            urlencoding::encode(&vars.to_string()),
            urlencoding::encode(&Value::Object(features).to_string()),
        );
        let resp = http
            .get(&url)
            .header("authorization", "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA")
            .header("x-twitter-auth-type", "OAuth2Session")
            .header("x-twitter-active-user", "yes")
            .header("x-csrf-token", &ct0)
            .header("cookie", format!("auth_token={auth_token}; ct0={ct0}"))
            .send();
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let body: Value = r.json().unwrap_or(Value::Null);
                println!("\n--- {op} HTTP {status} ---");
                if let Some(e) = body.get("errors") {
                    println!("errors: {}", truncate(&e.to_string(), 400));
                }
                let pretty = serde_json::to_string(&body).unwrap_or_default();
                println!("body head: {}", truncate(&pretty, 700));
                // Did we get media?
                let s = pretty.contains("extended_entities");
                println!("contains extended_entities: {s}");
            }
            Err(e) => println!("{op} request failed: {e}"),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn brace_blocks(text: &str, count: usize) -> Vec<&str> {
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
