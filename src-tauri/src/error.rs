use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("could not decode {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },

    #[error("webp encoding failed: {0}")]
    WebpEncode(String),

    #[error("unsupported file type: {0}")]
    Unsupported(PathBuf),

    #[error("no library directory available on this platform")]
    NoLibraryDir,

    #[error("library is locked by another operation")]
    Poisoned,
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

/// Tauri needs command errors to cross the IPC boundary as JSON. The Display
/// impl carries the path/source context, so a flat string loses nothing the
/// frontend can act on.
impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
