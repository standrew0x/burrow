//! Range-aware proxy used by the in-app player for linked X videos.
//!
//! WebView2 cannot reliably play X CDN URLs directly. The custom protocol keeps
//! those requests inside Burrow, validates the stored host on every request and
//! forwards small byte ranges so seeking does not download the whole video.

use std::io::Read;

use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, RANGE};
use rusqlite::OptionalExtension;
use tauri::Manager;

use crate::commands::AppState;

const MAX_CHUNK: u64 = 8 * 1024 * 1024;

pub fn handle(
    app: tauri::AppHandle,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    // convertFileSrc percent-encodes the supplied path as one component (for
    // example /%2Fvideo%2F42 on Windows), so decode before routing it.
    let asset_id = asset_id_from_path(request.uri().path());
    let Some(asset_id) = asset_id else {
        return text_response(tauri::http::StatusCode::BAD_REQUEST, "invalid video id");
    };

    let state = app.state::<AppState>();
    let stored_remote_url = {
        let Ok(conn) = state.conn.lock() else {
            return text_response(
                tauri::http::StatusCode::INTERNAL_SERVER_ERROR,
                "library is busy",
            );
        };
        conn.query_row(
            "SELECT remote_url FROM assets
              WHERE id = ?1 AND state = 'linked' AND kind = 'video'
                AND remote_url IS NOT NULL",
            [asset_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    };
    let Some(stored_remote_url) = stored_remote_url else {
        return text_response(tauri::http::StatusCode::NOT_FOUND, "video is not linked");
    };
    let remote_url = quality_url_from_path(request.uri().path()).unwrap_or(stored_remote_url);
    if !crate::xsync::is_x_media(&remote_url) {
        return text_response(tauri::http::StatusCode::FORBIDDEN, "untrusted media host");
    }

    let requested_range = request
        .headers()
        .get(tauri::http::header::RANGE)
        .and_then(|value| value.to_str().ok());
    match fetch(
        &remote_url,
        requested_range,
        request.method() == tauri::http::Method::HEAD,
    ) {
        Ok(response) => response,
        Err(message) => text_response(tauri::http::StatusCode::BAD_GATEWAY, &message),
    }
}

fn asset_id_from_path(path: &str) -> Option<i64> {
    let decoded_path = urlencoding::decode(path.trim_matches('/')).ok()?;
    decoded_path
        .trim_matches('/')
        .strip_prefix("video/")
        .and_then(|value| value.split('/').next())
        .and_then(|value| value.parse::<i64>().ok())
}

fn quality_url_from_path(path: &str) -> Option<String> {
    let decoded_path = urlencoding::decode(path.trim_matches('/')).ok()?;
    let encoded = decoded_path
        .trim_matches('/')
        .strip_prefix("video/")?
        .split_once('/')?
        .1;
    let url = urlencoding::decode(encoded).ok()?.into_owned();
    crate::xsync::is_x_media(&url).then_some(url)
}

fn fetch(
    url: &str,
    requested_range: Option<&str>,
    head: bool,
) -> std::result::Result<tauri::http::Response<Vec<u8>>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 Burrow/0.6.2")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let range = bounded_range(requested_range);
    let mut upstream = if head {
        client.head(url)
    } else {
        client.get(url).header(RANGE, &range)
    }
    .send()
    .map_err(|error| format!("X video request failed: {error}"))?;

    if !upstream.status().is_success() {
        return Err(format!("X returned HTTP {}", upstream.status().as_u16()));
    }

    let upstream_status = upstream.status();
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("video/mp4")
        .to_string();
    let upstream_range = upstream
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let upstream_length = upstream.content_length();

    let mut body = Vec::new();
    if !head {
        upstream
            .by_ref()
            .take(MAX_CHUNK + 1)
            .read_to_end(&mut body)
            .map_err(|error| format!("X video response could not be read: {error}"))?;
        if body.len() as u64 > MAX_CHUNK {
            body.truncate(MAX_CHUNK as usize);
        }
    }

    let (start, _) = range_bounds(&range).unwrap_or((0, MAX_CHUNK - 1));
    let synthesized_range = if upstream_range.is_none()
        && upstream_status == reqwest::StatusCode::OK
        && !head
        && upstream_length.is_some_and(|length| length > body.len() as u64)
    {
        let total = upstream_length.unwrap_or(body.len() as u64);
        Some(format!(
            "bytes {start}-{}/{total}",
            start + body.len().saturating_sub(1) as u64
        ))
    } else {
        None
    };
    let content_range = upstream_range.or(synthesized_range);
    let status = if content_range.is_some() {
        tauri::http::StatusCode::PARTIAL_CONTENT
    } else {
        tauri::http::StatusCode::from_u16(upstream_status.as_u16())
            .unwrap_or(tauri::http::StatusCode::OK)
    };

    let response_length = if head {
        upstream_length.unwrap_or(0)
    } else {
        body.len() as u64
    };
    let mut builder = tauri::http::Response::builder()
        .status(status)
        .header(tauri::http::header::CONTENT_TYPE, content_type)
        .header(tauri::http::header::ACCEPT_RANGES, "bytes")
        .header(tauri::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(tauri::http::header::CACHE_CONTROL, "private, max-age=300")
        .header(
            tauri::http::header::CONTENT_LENGTH,
            response_length.to_string(),
        );
    if let Some(value) = content_range {
        builder = builder.header(tauri::http::header::CONTENT_RANGE, value);
    }
    builder.body(body).map_err(|error| error.to_string())
}

fn bounded_range(value: Option<&str>) -> String {
    let Some(value) = value else {
        return format!("bytes=0-{}", MAX_CHUNK - 1);
    };
    let Some((start, end)) = range_bounds(value) else {
        return format!("bytes=0-{}", MAX_CHUNK - 1);
    };
    let end = end.min(start.saturating_add(MAX_CHUNK - 1));
    format!("bytes={start}-{end}")
}

fn range_bounds(value: &str) -> Option<(u64, u64)> {
    let raw = value.strip_prefix("bytes=")?.split(',').next()?;
    let (start, end) = raw.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        start.saturating_add(MAX_CHUNK - 1)
    } else {
        end.parse::<u64>().ok()?
    };
    (end >= start).then_some((start, end))
}

fn text_response(status: tauri::http::StatusCode, message: &str) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(status)
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .header(tauri::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(message.as_bytes().to_vec())
        .expect("static response headers are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_ended_ranges_are_bounded() {
        assert_eq!(
            bounded_range(Some("bytes=42-")),
            format!("bytes=42-{}", 42 + MAX_CHUNK - 1)
        );
    }

    #[test]
    fn huge_ranges_are_capped() {
        assert_eq!(
            bounded_range(Some("bytes=100-999999999")),
            format!("bytes=100-{}", 100 + MAX_CHUNK - 1)
        );
    }

    #[test]
    fn malformed_and_multi_ranges_are_safe() {
        assert_eq!(
            bounded_range(Some("nonsense")),
            format!("bytes=0-{}", MAX_CHUNK - 1)
        );
        assert_eq!(bounded_range(Some("bytes=10-20,30-40")), "bytes=10-20");
    }

    #[test]
    fn tauri_encoded_paths_route_to_the_asset() {
        assert_eq!(asset_id_from_path("/%2Fvideo%2F42"), Some(42));
        assert_eq!(asset_id_from_path("/video%2F7"), Some(7));
        assert_eq!(asset_id_from_path("/video%2F7%2Fquality"), Some(7));
        assert_eq!(asset_id_from_path("/%2Fvideo%2Fnope"), None);
    }

    #[test]
    fn an_encoded_quality_url_is_read_but_untrusted_hosts_are_rejected() {
        assert_eq!(
            quality_url_from_path(
                "/%2Fvideo%2F7%2Fhttps%253A%252F%252Fvideo.twimg.com%252Flow.mp4"
            )
            .as_deref(),
            Some("https://video.twimg.com/low.mp4")
        );
        assert!(quality_url_from_path(
            "/%2Fvideo%2F7%2Fhttps%253A%252F%252Fexample.com%252Fevil.mp4"
        )
        .is_none());
    }
}
