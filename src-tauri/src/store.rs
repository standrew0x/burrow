//! Content-addressed blob store.
//!
//! Layout under the library root:
//! ```text
//! burrow.db
//! blobs/ab/cd/abcd...ef.png     originals, named by blake3 digest
//! thumbs/ab/cd/abcd...ef.webp   512px long-edge previews
//! ```
//!
//! Two levels of 256-way sharding keep any one directory small. A flat
//! directory of 100k files makes Explorer and `readdir` crawl on NTFS, and
//! the app is expected to accumulate exactly that many.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub struct Library {
    root: PathBuf,
}

impl Library {
    /// `%LOCALAPPDATA%\burrow`.
    ///
    /// Deliberately not Documents or a user-visible folder: OneDrive's
    /// Known Folder Move redirects those, and a redirected library means every
    /// imported blob gets uploaded to a cloud the user did not opt into --
    /// which defeats the point of a local-first tool.
    pub fn default_root() -> Result<PathBuf> {
        dirs::data_local_dir()
            .map(|d| d.join("burrow"))
            .ok_or(Error::NoLibraryDir)
    }

    /// Opens (creating if absent) a library rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [&root, &root.join("blobs"), &root.join("thumbs")] {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("burrow.db")
    }

    /// `blobs/ab/cd/<hash>.<ext>`. `hash` must be a 64-char blake3 hex digest.
    pub fn blob_path(&self, hash: &str, ext: &str) -> PathBuf {
        self.sharded("blobs", hash).join(format!("{hash}.{ext}"))
    }

    /// `thumbs/ab/cd/<hash>.webp`.
    pub fn thumb_path(&self, hash: &str) -> PathBuf {
        self.sharded("thumbs", hash).join(format!("{hash}.webp"))
    }

    fn sharded(&self, kind: &str, hash: &str) -> PathBuf {
        // Digests are always 64 hex chars, but a short string must not panic
        // here -- fall back to a single bucket rather than slicing out of range.
        if hash.len() >= 4 {
            self.root.join(kind).join(&hash[0..2]).join(&hash[2..4])
        } else {
            self.root.join(kind).join("__")
        }
    }

    /// Writes `bytes` to `path`, creating parents. Existing files are left
    /// alone: identical digest means identical content, so a rewrite would burn
    /// I/O to produce the same bytes.
    pub fn write_if_absent(&self, path: &Path, bytes: &[u8]) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        // Write to a temp sibling then rename, so a crash mid-write cannot
        // leave a truncated blob sitting at a path the digest says is complete.
        let tmp = path.with_extension("partial");
        std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
        Ok(true)
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_library() -> (Library, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "burrow-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let lib = Library::open(&root).expect("open library");
        (lib, root)
    }

    #[test]
    fn open_creates_expected_directories() {
        let (lib, root) = temp_library();
        assert!(root.join("blobs").is_dir());
        assert!(root.join("thumbs").is_dir());
        assert_eq!(lib.db_path(), root.join("burrow.db"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn blob_paths_are_two_level_sharded() {
        let (lib, root) = temp_library();
        let hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        assert_eq!(
            lib.blob_path(hash, "png"),
            root.join("blobs")
                .join("ab")
                .join("cd")
                .join(format!("{hash}.png"))
        );
        assert_eq!(
            lib.thumb_path(hash),
            root.join("thumbs")
                .join("ab")
                .join("cd")
                .join(format!("{hash}.webp"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn short_hash_does_not_panic() {
        let (lib, root) = temp_library();
        let _ = lib.blob_path("ab", "png");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_if_absent_is_idempotent_and_leaves_no_partials() {
        let (lib, root) = temp_library();
        let hash = hash_bytes(b"hello");
        let path = lib.blob_path(&hash, "bin");

        assert!(lib.write_if_absent(&path, b"hello").expect("first write"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        // Second call reports "already present" and does not rewrite.
        assert!(!lib.write_if_absent(&path, b"hello").expect("second write"));
        assert!(!path.with_extension("partial").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_is_stable_and_content_dependent() {
        assert_eq!(hash_bytes(b"burrow"), hash_bytes(b"burrow"));
        assert_ne!(hash_bytes(b"burrow"), hash_bytes(b"burrov"));
        assert_eq!(hash_bytes(b"burrow").len(), 64);
    }
}
