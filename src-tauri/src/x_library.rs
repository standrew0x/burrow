//! Human-readable filesystem view of downloaded X videos.
//!
//! Burrow's blob tree is deliberately content-addressed. Replacing it with
//! human filenames would weaken deduplication and make renames dangerous, so
//! this module creates hard links instead: two useful paths, one set of bytes.

use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Error, Result};
use crate::store::Library;

const ROOT: &str = "X Downloads";

#[derive(Debug)]
struct XVideo {
    id: i64,
    hash: String,
    ext: String,
    original_name: Option<String>,
    source_url: String,
    posted_at: Option<String>,
}

pub fn root(lib: &Library) -> PathBuf {
    lib.root().join(ROOT).join("Videos")
}

/// Adds any downloaded X videos that predate the organised view.
pub fn organize_existing(lib: &Library, conn: &Connection) -> Result<usize> {
    std::fs::create_dir_all(root(lib)).map_err(|error| Error::io(root(lib), error))?;
    let mut statement = conn.prepare(
        "SELECT id, hash, ext, original_name, source_url, posted_at
           FROM assets
          WHERE state = 'local' AND kind = 'video'
            AND source_url IS NOT NULL
            AND (source_url LIKE 'https://x.com/%/status/%'
                 OR source_url LIKE 'https://twitter.com/%/status/%')",
    )?;
    let videos = statement
        .query_map([], |row| {
            Ok(XVideo {
                id: row.get(0)?,
                hash: row.get(1)?,
                ext: row.get(2)?,
                original_name: row.get(3)?,
                source_url: row.get(4)?,
                posted_at: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut created = 0;
    for video in videos {
        if organize_video(lib, conn, &video)? {
            created += 1;
        }
    }
    remove_empty_directories(&root(lib));
    Ok(created)
}

pub fn organize_asset(lib: &Library, conn: &Connection, asset_id: i64) -> Result<bool> {
    let video = conn
        .query_row(
            "SELECT id, hash, ext, original_name, source_url, posted_at
               FROM assets
              WHERE id = ?1 AND state = 'local' AND kind = 'video'
                AND source_url IS NOT NULL
                AND (source_url LIKE 'https://x.com/%/status/%'
                     OR source_url LIKE 'https://twitter.com/%/status/%')",
            [asset_id],
            |row| {
                Ok(XVideo {
                    id: row.get(0)?,
                    hash: row.get(1)?,
                    ext: row.get(2)?,
                    original_name: row.get(3)?,
                    source_url: row.get(4)?,
                    posted_at: row.get(5)?,
                })
            },
        )
        .optional()?;
    match video {
        Some(video) => organize_video(lib, conn, &video),
        None => Ok(false),
    }
}

fn organize_video(lib: &Library, conn: &Connection, video: &XVideo) -> Result<bool> {
    let relative = relative_path(video);
    let destination = lib.root().join(&relative);
    let source = lib.blob_path(&video.hash, &video.ext);
    if !source.is_file() {
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }

    let previous: Option<String> = conn
        .query_row(
            "SELECT relative_path FROM x_download_paths WHERE asset_id=?1",
            [video.id],
            |row| row.get(0),
        )
        .optional()?;
    let mut created = false;
    if !destination.exists() {
        std::fs::hard_link(&source, &destination)
            .map_err(|error| Error::io(&destination, error))?;
        created = true;
    }
    conn.execute(
        "INSERT INTO x_download_paths(asset_id, relative_path) VALUES (?1, ?2)
         ON CONFLICT(asset_id) DO UPDATE SET relative_path = excluded.relative_path",
        rusqlite::params![video.id, relative.to_string_lossy()],
    )?;
    if let Some(previous) = previous.filter(|previous| previous != &relative.to_string_lossy()) {
        if let Some(old_path) = resolve_stored_path(lib, &previous) {
            let _ = std::fs::remove_file(old_path);
        }
    }
    Ok(created)
}

fn remove_empty_directories(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_empty_directories(&path);
        }
    }
    if directory.file_name().is_some_and(|name| name != "Videos") {
        let _ = std::fs::remove_dir(directory);
    }
}

fn relative_path(video: &XVideo) -> PathBuf {
    let (year, month) = date_parts(video.posted_at.as_deref());
    let (handle, post_id) = x_identity(&video.source_url);
    let extension = safe_extension(&video.ext);
    let fallback = format!("{post_id}.{extension}");
    let name = safe_filename(
        video.original_name.as_deref().unwrap_or(&fallback),
        &extension,
    );
    PathBuf::from(ROOT)
        .join("Videos")
        .join(year)
        .join(month)
        .join(format!("@{handle}"))
        .join(name)
}

fn date_parts(posted_at: Option<&str>) -> (String, String) {
    let date = posted_at.unwrap_or_default().as_bytes();
    if date.len() >= 7
        && date[0..4].iter().all(u8::is_ascii_digit)
        && date[4] == b'-'
        && date[5..7].iter().all(u8::is_ascii_digit)
    {
        (
            String::from_utf8_lossy(&date[0..4]).into_owned(),
            String::from_utf8_lossy(&date[5..7]).into_owned(),
        )
    } else {
        ("Unknown date".to_string(), "Unknown month".to_string())
    }
}

fn x_identity(url: &str) -> (String, String) {
    let segments: Vec<&str> = url
        .split(['/', '?', '#'])
        .filter(|part| !part.is_empty())
        .collect();
    let status = segments.iter().position(|part| *part == "status");
    let handle = status
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| segments.get(index))
        .map(|value| safe_component(value, 24))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let post_id = status
        .and_then(|index| segments.get(index + 1))
        .map(|value| {
            value
                .chars()
                .filter(char::is_ascii_digit)
                .take(32)
                .collect()
        })
        .filter(|value: &String| !value.is_empty())
        .unwrap_or_else(|| "post".to_string());
    (handle, post_id)
}

fn safe_extension(extension: &str) -> String {
    let value: String = extension
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if value.is_empty() {
        "mp4".to_string()
    } else {
        value.to_ascii_lowercase()
    }
}

fn safe_filename(name: &str, extension: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "video".to_string());
    let stem = safe_component(&stem, 150);
    format!(
        "{}.{}",
        if stem.is_empty() { "video" } else { &stem },
        extension
    )
}

fn safe_component(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .take(limit)
        .collect::<String>()
        .trim()
        .trim_end_matches([' ', '.'])
        .to_string()
}

/// Resolves a stored relative path without permitting it to leave the library.
pub fn resolve_stored_path(lib: &Library, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(lib.root().join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (Library, Connection, PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "burrow-x-library-{}-{name}-{nonce}",
            std::process::id()
        ));
        let library = Library::open(&directory).unwrap();
        let connection = crate::db::open(&library.db_path()).unwrap();
        (library, connection, directory)
    }

    fn insert_x_video(library: &Library, connection: &Connection) -> i64 {
        let hash = "b".repeat(64);
        library
            .write_if_absent(&library.blob_path(&hash, "mp4"), b"one set of video bytes")
            .unwrap();
        connection
            .execute(
                "INSERT INTO assets
                    (hash, ext, mime, width, height, bytes, original_name,
                     source_url, imported_at, kind, state, remote_url, content_hash,
                     posted_at)
                 VALUES (?1, 'mp4', 'video/mp4', 1920, 1080, 22,
                         '2026-09-15_artist_demo_12345_0.mp4',
                         'https://x.com/artist/status/12345', 1, 'video', 'local',
                         'https://video.twimg.com/demo.mp4', ?1,
                         '2026-09-15T12:00:00Z')",
                [&hash],
            )
            .unwrap();
        connection.last_insert_rowid()
    }

    #[test]
    fn path_is_grouped_by_date_and_author() {
        let video = XVideo {
            id: 1,
            hash: "a".repeat(64),
            ext: "MP4".into(),
            original_name: Some("2026-08-05_Lovable_design_2085043732903301438_0.mp4".into()),
            source_url: "https://x.com/Lovable/status/2085043732903301438".into(),
            posted_at: Some("2026-08-05T16:42:18Z".into()),
        };
        assert_eq!(
            relative_path(&video),
            PathBuf::from("X Downloads/Videos/2026/08/@Lovable/2026-08-05_Lovable_design_2085043732903301438_0.mp4")
        );
    }

    #[test]
    fn stored_paths_cannot_escape_the_library() {
        let root = std::env::temp_dir().join("burrow-x-path-test");
        let lib = Library::open(&root).unwrap();
        assert!(resolve_stored_path(&lib, "X Downloads/Videos/a.mp4").is_some());
        assert!(resolve_stored_path(&lib, "../outside.mp4").is_none());
        assert!(resolve_stored_path(&lib, "C:\\outside.mp4").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn organiser_creates_one_file_name_without_copying_the_bytes() {
        let (library, connection, directory) = fixture("hard-link");
        let asset_id = insert_x_video(&library, &connection);
        assert!(organize_asset(&library, &connection, asset_id).unwrap());

        let relative: String = connection
            .query_row(
                "SELECT relative_path FROM x_download_paths WHERE asset_id=?1",
                [asset_id],
                |row| row.get(0),
            )
            .unwrap();
        let friendly = resolve_stored_path(&library, &relative).unwrap();
        assert_eq!(std::fs::read(&friendly).unwrap(), b"one set of video bytes");
        assert_eq!(
            std::fs::metadata(&friendly).unwrap().len(),
            std::fs::metadata(library.blob_path(&"b".repeat(64), "mp4"))
                .unwrap()
                .len()
        );

        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn deleting_an_asset_removes_its_friendly_hard_link() {
        let (library, mut connection, directory) = fixture("delete");
        let asset_id = insert_x_video(&library, &connection);
        organize_asset(&library, &connection, asset_id).unwrap();
        let relative: String = connection
            .query_row(
                "SELECT relative_path FROM x_download_paths WHERE asset_id=?1",
                [asset_id],
                |row| row.get(0),
            )
            .unwrap();
        let friendly = resolve_stored_path(&library, &relative).unwrap();

        let report = crate::ingest::delete_assets(&library, &mut connection, &[asset_id]).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(!friendly.exists());
        assert!(!library.blob_path(&"b".repeat(64), "mp4").exists());

        std::fs::remove_dir_all(directory).ok();
    }
}
