//! Video probing and frame extraction, via ffmpeg/ffprobe as child processes.
//!
//! Shelling out rather than linking `ffmpeg-next`: the C bindings are a
//! miserable build on Windows/MSVC, and a process boundary keeps ffmpeg's
//! licence from reaching into this binary. That separation is what makes it
//! legitimate to ship an LGPL ffmpeg alongside the app.
//!
//! The shipped build lives in the bundle's resources and is preferred; a copy
//! on PATH is the fallback, which is what the CLI examples and `tauri dev` use.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::{Error, Result};

/// Container extensions offered to the directory walker and drag-drop.
/// Actual format is confirmed by ffprobe, not by this list.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "m2ts", "ts",
];

/// Where to grab the poster frame, as a fraction of duration.
///
/// Not frame 0: videos routinely open on black, a fade-in, or a slate, and a
/// black thumbnail is both useless in the grid and poisons the OkLab palette
/// with a colour the video does not actually contain.
const POSTER_FRACTION: f64 = 0.10;
const POSTER_MIN_SECONDS: f64 = 0.5;
const POSTER_MAX_SECONDS: f64 = 10.0;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: i64,
    pub codec: String,
    pub has_audio: bool,
}

// --- ffprobe JSON shape (only the fields we consume) ---

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    duration: Option<String>,
}

/// Directory holding the bundled ffmpeg, set once at startup.
static BUNDLED_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Points the module at the bundled ffmpeg. Called once from `run()` with the
/// resolved resource directory; later calls are ignored.
pub fn use_bundled_dir(dir: PathBuf) {
    let _ = BUNDLED_DIR.set(dir);
}

/// Absolute path to the bundled tool, or the bare name so the OS searches PATH.
///
/// Existence is checked rather than assumed: a dev build has no resource
/// directory, and handing back a path that is not there would turn a perfectly
/// good PATH install into a confusing "not found".
fn tool(name: &str) -> PathBuf {
    if let Some(dir) = BUNDLED_DIR.get() {
        let exe = dir.join(format!("{name}.exe"));
        if exe.is_file() {
            return exe;
        }
    }
    PathBuf::from(name)
}

fn tool_missing(name: &str, e: &std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::Ffmpeg(format!(
            "{name} could not be found. It ships with Burrow, so this usually means \
             the install is incomplete -- reinstall, or put {name} on PATH."
        ))
    } else {
        Error::Ffmpeg(format!("could not run {name}: {e}"))
    }
}

/// True when both tools can be executed. Used to give one clear message up
/// front instead of one failure per dropped file.
pub fn tooling_available() -> bool {
    ["ffprobe", "ffmpeg"].iter().all(|name| {
        Command::new(tool(name))
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

/// Protocols ffmpeg may use when the input is a URL.
///
/// ffmpeg speaks far more than HTTP -- `file`, `concat`, `subfile` and friends
/// are all enabled by default, and a URL that redirects into one of those turns
/// a thumbnail request into a local file read. The input is remote-controlled,
/// so the protocol set has to be stated rather than defaulted.
const REMOTE_PROTOCOLS: &str = "http,https,tcp,tls,crypto";

/// Where ffmpeg should read from.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    File(&'a Path),
    /// An `https://` URL, read over the network by ffmpeg itself. Only a few
    /// seconds around the seek point get transferred, not the whole file.
    Url(&'a str),
}

impl Source<'_> {
    fn describe(&self) -> String {
        match self {
            Source::File(p) => p.display().to_string(),
            Source::Url(u) => (*u).to_string(),
        }
    }

    /// Arguments that must precede `-i`.
    fn guard_args(&self) -> Vec<&'static str> {
        match self {
            Source::File(_) => Vec::new(),
            Source::Url(_) => vec!["-protocol_whitelist", REMOTE_PROTOCOLS],
        }
    }

    fn input(&self) -> &std::ffi::OsStr {
        match self {
            Source::File(p) => p.as_os_str(),
            Source::Url(u) => std::ffi::OsStr::new(*u),
        }
    }
}

/// Reads stream metadata. `Ok(None)` means the file has no video stream --
/// an audio file, or something ffprobe understands but we do not want.
pub fn probe(path: &Path) -> Result<Option<VideoInfo>> {
    probe_source(Source::File(path))
}

/// Reads stream metadata from a file or a remote URL.
pub fn probe_source(source: Source<'_>) -> Result<Option<VideoInfo>> {
    let output = Command::new(tool("ffprobe"))
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name,width,height,duration",
            "-show_entries",
            "format=duration",
            "-print_format",
            "json",
        ])
        .args(source.guard_args())
        .arg(source.input())
        .output()
        .map_err(|e| tool_missing("ffprobe", &e))?;

    if !output.status.success() {
        // Not an error: ffprobe rejects non-media files, which is how a .txt
        // gets classified rather than crashing the import.
        return Ok(None);
    }

    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::Ffmpeg(format!("could not parse ffprobe output: {e}")))?;

    let has_audio = parsed.streams.iter().any(|s| s.codec_type == "audio");

    let Some(video) = parsed.streams.iter().find(|s| s.codec_type == "video") else {
        return Ok(None);
    };

    // Some containers (notably MKV) omit per-stream duration and only carry it
    // at format level, so fall back rather than reporting a 0-length video.
    let seconds = video
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            parsed
                .format
                .as_ref()
                .and_then(|f| f.duration.as_deref())
                .and_then(|d| d.parse::<f64>().ok())
        })
        .unwrap_or(0.0);

    Ok(Some(VideoInfo {
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        duration_ms: (seconds * 1000.0).round().max(0.0) as i64,
        codec: video.codec_name.clone(),
        has_audio,
    }))
}

/// Seek offset for the poster frame, in seconds.
pub fn poster_offset_seconds(duration_ms: i64) -> f64 {
    if duration_ms <= 0 {
        return 0.0;
    }
    let seconds = duration_ms as f64 / 1000.0;
    (seconds * POSTER_FRACTION)
        .clamp(POSTER_MIN_SECONDS, POSTER_MAX_SECONDS)
        // Never seek past the end -- a 1s clip would otherwise seek to 0.5s of
        // a 0.5s remainder and decode nothing.
        .min(seconds * 0.9)
}

/// Decodes one frame and returns it as PNG bytes.
///
/// Piped through stdout rather than a temp file: no cleanup, no collisions
/// between parallel imports, and the frame is small enough to hold in memory.
pub fn extract_poster_frame(path: &Path, duration_ms: i64) -> Result<Vec<u8>> {
    extract_poster_frame_from(Source::File(path), duration_ms)
}

/// Decodes one frame from a file or a remote URL.
///
/// Against a URL this is a range request around the seek point rather than a
/// full download, which is what makes it affordable to thumbnail a video that
/// is only ever going to be linked.
pub fn extract_poster_frame_from(source: Source<'_>, duration_ms: i64) -> Result<Vec<u8>> {
    let offset = poster_offset_seconds(duration_ms);

    let output = Command::new(tool("ffmpeg"))
        .args(["-v", "error"])
        .args(source.guard_args())
        // -ss BEFORE -i is the fast path: ffmpeg seeks the container instead of
        // decoding every frame up to the offset. On a long video that is the
        // difference between milliseconds and tens of seconds.
        .args(["-ss", &format!("{offset:.3}")])
        .arg("-i")
        .arg(source.input())
        .args([
            "-frames:v",
            "1",
            "-an",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ])
        .output()
        .map_err(|e| tool_missing("ffmpeg", &e))?;

    if !output.status.success() || output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().last().unwrap_or("no output").trim();
        return Err(Error::Ffmpeg(format!(
            "could not extract a frame from {}: {detail}",
            source.describe()
        )));
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders a few seconds of colour bars so the tests exercise real ffmpeg
    /// rather than mocking it. Returns None when ffmpeg is unavailable.
    fn synth_video(name: &str, seconds: u32) -> Option<std::path::PathBuf> {
        if !tooling_available() {
            return None;
        }
        let path =
            std::env::temp_dir().join(format!("burrow-vid-{}-{name}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=10:duration={seconds}"),
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn poster_offset_is_clamped_and_never_past_the_end() {
        // 10% of a 100s video, within bounds.
        assert!((poster_offset_seconds(100_000) - 10.0).abs() < 1e-6);
        // Long video: capped rather than seeking minutes in.
        assert!((poster_offset_seconds(3_600_000) - POSTER_MAX_SECONDS).abs() < 1e-6);
        // Very short clip: must stay inside the clip.
        let short = poster_offset_seconds(1_000);
        assert!(short < 1.0, "offset {short} would seek past a 1s clip");
        // Degenerate input must not panic or go negative.
        assert_eq!(poster_offset_seconds(0), 0.0);
        assert_eq!(poster_offset_seconds(-5), 0.0);
    }

    #[test]
    fn probe_reads_dimensions_and_duration() {
        let Some(path) = synth_video("probe", 3) else {
            eprintln!("ffmpeg unavailable, skipping");
            return;
        };
        let info = probe(&path)
            .expect("probe")
            .expect("should have a video stream");

        assert_eq!((info.width, info.height), (320, 240));
        assert!(
            (info.duration_ms - 3000).abs() < 400,
            "expected ~3000ms, got {}",
            info.duration_ms
        );
        assert!(!info.codec.is_empty());
        assert!(!info.has_audio, "testsrc has no audio track");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_returns_none_for_a_non_video() {
        if !tooling_available() {
            return;
        }
        let path = std::env::temp_dir().join(format!("burrow-notvid-{}.txt", std::process::id()));
        std::fs::write(&path, b"definitely not a video").unwrap();

        assert_eq!(probe(&path).expect("probe should not error"), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extracted_frame_is_a_decodable_png() {
        let Some(path) = synth_video("frame", 3) else {
            eprintln!("ffmpeg unavailable, skipping");
            return;
        };
        let info = probe(&path).unwrap().unwrap();
        let png = extract_poster_frame(&path, info.duration_ms).expect("extract");

        assert_eq!(&png[1..4], b"PNG");
        let decoded = image::load_from_memory(&png).expect("frame should decode");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (320, 240),
            "frame should match the source dimensions"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extract_reports_a_useful_error_for_a_broken_file() {
        if !tooling_available() {
            return;
        }
        let path = std::env::temp_dir().join(format!("burrow-broken-{}.mp4", std::process::id()));
        std::fs::write(&path, b"not actually an mp4").unwrap();

        let err = extract_poster_frame(&path, 1000).unwrap_err();
        assert!(
            err.to_string().contains("burrow-broken"),
            "error should name the file, got: {err}"
        );
        std::fs::remove_file(&path).ok();
    }
}
