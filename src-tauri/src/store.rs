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
    /// Bundle identifier, matching `tauri.conf.json`. The library path is
    /// derived from it so that it equals Tauri's `$APPLOCALDATA`, which is what
    /// the asset-protocol scope and the opener capability are written against.
    pub const IDENTIFIER: &'static str = "co.burrow.app";

    /// `%LOCALAPPDATA%\co.burrow.app`.
    ///
    /// Named for the identifier, NOT the product name, and that is load-bearing.
    /// NSIS per-user installs default to `$LOCALAPPDATA\<ProductName>`, so a
    /// library at `%LOCALAPPDATA%\burrow` lands in the *install directory* --
    /// and NSIS clears `$INSTDIR` when installing over an existing version,
    /// deleting the database. That is not hypothetical; it destroyed a real
    /// library during development. See `library_root_cannot_collide_with_the_
    /// installer` below.
    ///
    /// Also deliberately not Documents or any user-visible folder: OneDrive's
    /// Known Folder Move redirects those, and a redirected library means every
    /// imported blob gets uploaded to a cloud the user did not opt into --
    /// which defeats the point of a local-first tool.
    /// `BURROW_LIBRARY` overrides the location entirely.
    ///
    /// Exists so a second copy of the app can be run against a scratch library
    /// rather than the real one -- verifying a change used to mean opening the
    /// live library in a second process and migrating it underneath whatever
    /// was already running. It doubles as the escape hatch for anyone who wants
    /// their library on another drive.
    pub fn default_root() -> Result<PathBuf> {
        Self::root_from(std::env::var_os("BURROW_LIBRARY"))
    }

    /// The override taken as an argument rather than read here, so the choice
    /// can be tested without writing a process-global environment variable that
    /// every other test in the binary would race against.
    fn root_from(override_path: Option<std::ffi::OsString>) -> Result<PathBuf> {
        if let Some(path) = override_path {
            let path = PathBuf::from(path);
            // An empty value is how a shell spells "unset"; honouring it would
            // put the library at the filesystem root.
            if !path.as_os_str().is_empty() {
                return Ok(path);
            }
        }
        dirs::data_local_dir()
            .map(|d| d.join(Self::IDENTIFIER))
            .ok_or(Error::NoLibraryDir)
    }

    /// Opens (creating if absent) a library rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [
            &root,
            &root.join("blobs"),
            &root.join("thumbs"),
            &root.join("X Downloads").join("Videos"),
        ] {
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
    ///
    /// Only for content already in memory -- images, extracted frames, encoded
    /// thumbnails. Use [`Library::copy_if_absent`] for originals on disk, which
    /// may be gigabytes.
    pub fn write_if_absent(&self, path: &Path, bytes: &[u8]) -> Result<bool> {
        self.stage(path, |tmp| {
            std::fs::write(tmp, bytes).map_err(|e| Error::io(tmp, e))
        })
    }

    /// Copies `src` into the store without loading it into memory.
    ///
    /// `std::fs::copy` streams through the OS, so a 2GB video costs a constant
    /// amount of RAM. Reading it into a `Vec<u8>` first would not.
    pub fn copy_if_absent(&self, path: &Path, src: &Path) -> Result<bool> {
        self.stage(path, |tmp| {
            std::fs::copy(src, tmp)
                .map(|_| ())
                .map_err(|e| Error::io(src, e))
        })
    }

    /// Writes via a temp sibling then renames, so a crash mid-write cannot
    /// leave a truncated blob at a path whose digest claims it is complete.
    fn stage(&self, path: &Path, fill: impl FnOnce(&Path) -> Result<()>) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let tmp = path.with_extension("partial");
        match fill(&tmp) {
            Ok(()) => {
                std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
                Ok(true)
            }
            Err(e) => {
                // Never leave a partial behind for the next run to trip over.
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Digests a file without reading it into the heap.
///
/// Memory-mapped and hashed in parallel. The obvious `fs::read` + `hash` costs
/// the file's full size in RAM, which is invisible for a 3MB JPEG and fatal for
/// a 500MB video -- multiplied by however many files are being processed at once.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .map_err(|e| Error::io(path, e))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Reads at most `n` leading bytes, for format sniffing.
pub fn read_header(path: &Path, n: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut buf = Vec::with_capacity(n);
    file.take(n as u64)
        .read_to_end(&mut buf)
        .map_err(|e| Error::io(path, e))?;
    Ok(buf)
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

    /// Regression guard for a real data-loss bug.
    ///
    /// The library used to live at `%LOCALAPPDATA%\burrow`. NSIS per-user
    /// installs default to `$LOCALAPPDATA\<ProductName>` -- `...\Burrow` --
    /// and Windows paths are case-insensitive, so the install directory *was*
    /// the library directory. Installing over an existing version cleared
    /// `$INSTDIR` and took `burrow.db` with it, leaving every blob orphaned.
    #[test]
    fn library_root_cannot_collide_with_the_installer() {
        // `root_from(None)` rather than `default_root()`, so this asserts the
        // default location even on a machine where BURROW_LIBRARY is set.
        let root = Library::root_from(None).expect("a local data dir");
        let leaf = root
            .file_name()
            .and_then(|n| n.to_str())
            .expect("root has a final component");

        assert_eq!(
            leaf,
            Library::IDENTIFIER,
            "library must be named for the bundle identifier"
        );
        assert!(
            !leaf.eq_ignore_ascii_case("burrow"),
            "library root {leaf:?} matches the NSIS install directory \
             ($LOCALAPPDATA\\<ProductName>); installing would delete the database"
        );
    }

    #[test]
    fn an_override_relocates_the_library_but_an_empty_one_does_not() {
        let custom = Library::root_from(Some("D:/refs/burrow".into())).expect("override");
        assert_eq!(custom, PathBuf::from("D:/refs/burrow"));

        // An empty variable is how a shell spells "unset". Taking it literally
        // would put the library at the filesystem root.
        let blank = Library::root_from(Some("".into())).expect("blank falls back");
        assert_eq!(blank, Library::root_from(None).unwrap());
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

    #[test]
    fn streaming_file_hash_matches_in_memory_hash() {
        let (_lib, root) = temp_library();
        let path = root.join("sample.bin");
        // Larger than blake3's internal chunk size, so the streaming path is
        // actually exercised rather than trivially matching on one block.
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        assert_eq!(hash_file(&path).unwrap(), hash_bytes(&data));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hash_file_reports_a_missing_path() {
        let err = hash_file(Path::new("no-such-file.bin")).unwrap_err();
        assert!(err.to_string().contains("no-such-file.bin"), "got: {err}");
    }

    #[test]
    fn copy_if_absent_streams_and_is_idempotent() {
        let (lib, root) = temp_library();
        let src = root.join("source.bin");
        std::fs::write(&src, b"video-ish bytes").unwrap();

        let hash = hash_file(&src).unwrap();
        let dest = lib.blob_path(&hash, "mp4");

        assert!(lib.copy_if_absent(&dest, &src).expect("first copy"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"video-ish bytes");

        assert!(!lib.copy_if_absent(&dest, &src).expect("second copy"));
        assert!(!dest.with_extension("partial").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_copy_leaves_no_partial_behind() {
        let (lib, root) = temp_library();
        let dest = lib.blob_path(&"c".repeat(64), "mp4");

        // Source does not exist, so the copy fails mid-stage.
        assert!(lib
            .copy_if_absent(&dest, &root.join("missing.bin"))
            .is_err());
        assert!(!dest.exists());
        assert!(
            !dest.with_extension("partial").exists(),
            "a partial survived a failed copy and would poison the next run"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_header_stops_at_the_requested_length() {
        let (_lib, root) = temp_library();
        let path = root.join("big.bin");
        std::fs::write(&path, vec![7u8; 10_000]).unwrap();

        assert_eq!(read_header(&path, 64).unwrap().len(), 64);
        // Asking for more than exists yields what exists, not an error.
        assert_eq!(read_header(&path, 20_000).unwrap().len(), 10_000);

        std::fs::remove_dir_all(&root).ok();
    }
}
