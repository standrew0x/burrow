import { useCallback, useEffect, useMemo, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";

import { openPath } from "@tauri-apps/plugin-opener";

import {
  blobUrl,
  importPaths,
  isPlayableInline,
  libraryRoot,
  listAssets,
  searchByColor,
  thumbUrl,
} from "./api";
import type { Asset, FailedImport } from "./types";
import "./App.css";

interface Notice {
  imported: number;
  duplicates: number;
  failed: FailedImport[];
}

/** OkLab search radius. See DEFAULT_COLOR_TOLERANCE in commands.rs for how
 *  this number was picked; keep the two in step. */
const COLOR_TOLERANCE = 0.05;

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

/** Readable ink over a swatch, chosen from OkLab lightness. */
const swatchInk = (l: number) => (l > 0.62 ? "#111" : "#fff");

function formatDuration(ms: number): string {
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

export default function App() {
  const [assets, setAssets] = useState<Asset[]>([]);
  const [loading, setLoading] = useState(true);
  const [dragging, setDragging] = useState(false);
  const [importingCount, setImportingCount] = useState<number | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [colorFilter, setColorFilter] = useState<string | null>(null);
  const [hexInput, setHexInput] = useState("");
  const [root, setRoot] = useState("");
  const [playing, setPlaying] = useState<Asset | null>(null);
  /** Set when the <video> element refuses the file despite a playable container
   *  — an HEVC or AV1 mp4, typically. */
  const [playbackFailed, setPlaybackFailed] = useState(false);

  const refresh = useCallback(async () => {
    try {
      if (colorFilter) {
        const matches = await searchByColor(colorFilter, COLOR_TOLERANCE, 500);
        setAssets(matches.map((m) => m.asset));
      } else {
        setAssets(await listAssets(500, 0));
      }
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [colorFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    libraryRoot()
      .then(setRoot)
      .catch(() => {});
  }, []);

  const runImport = useCallback(
    async (paths: string[]) => {
      if (paths.length === 0) return;
      setImportingCount(paths.length);
      setNotice(null);
      try {
        const report = await importPaths(paths);
        setNotice({
          imported: report.imported.length,
          duplicates: report.duplicates,
          failed: report.failed,
        });
        await refresh();
      } catch (e) {
        setError(String(e));
      } finally {
        setImportingCount(null);
      }
    },
    [refresh],
  );

  // Tauri's native drag-drop, not React's onDrop. With dragDropEnabled (the
  // default) Tauri intercepts the OS drop and HTML5 drag events never fire, so
  // a React onDrop handler would silently do nothing.
  //
  // This delivers *file paths*, which covers drops from Explorer. Dragging an
  // image straight out of a browser hands over CF_HTML or FileContents rather
  // than a path and is not covered here -- that needs a native IDropTarget.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;

    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type === "over") {
          setDragging(true);
        } else if (event.payload.type === "drop") {
          setDragging(false);
          void runImport(event.payload.paths);
        } else {
          setDragging(false);
        }
      })
      .then((fn) => {
        // Can resolve after unmount; drop it straight away if so.
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((e) => setError(String(e)));

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [runImport]);

  const submitHex = (e: React.FormEvent) => {
    e.preventDefault();
    const value = hexInput.trim();
    if (value) setColorFilter(value.startsWith("#") ? value : `#${value}`);
  };

  const heading = useMemo(() => {
    if (loading) return "Loading library…";
    if (importingCount !== null) {
      return `Importing ${importingCount} file${importingCount === 1 ? "" : "s"}…`;
    }
    if (colorFilter) return `${assets.length} matching ${colorFilter}`;
    return `${assets.length} reference${assets.length === 1 ? "" : "s"}`;
  }, [loading, importingCount, colorFilter, assets.length]);

  return (
    <div className={`app${dragging ? " app--dragging" : ""}`}>
      <header className="bar">
        <div className="bar__identity">
          <h1>Burrow</h1>
          <span className="bar__count">{heading}</span>
        </div>

        <form className="bar__search" onSubmit={submitHex}>
          <input
            type="text"
            value={hexInput}
            onChange={(e) => setHexInput(e.target.value)}
            placeholder="#3d6fb1"
            aria-label="Search by hex colour"
            spellCheck={false}
          />
          <button type="submit">Search colour</button>
          {colorFilter && (
            <button
              type="button"
              className="bar__clear"
              onClick={() => {
                setColorFilter(null);
                setHexInput("");
              }}
            >
              Clear
            </button>
          )}
        </form>
      </header>

      {colorFilter && (
        <div className="filter">
          <span className="filter__chip" style={{ background: colorFilter }} />
          <span>
            within {COLOR_TOLERANCE} OkLab of <code>{colorFilter}</code>
          </span>
        </div>
      )}

      {error && (
        <div className="banner banner--error">
          <strong>Something went wrong.</strong> {error}
        </div>
      )}

      {notice && (
        <div className="banner">
          <strong>{notice.imported}</strong> imported
          {notice.duplicates > 0 && <> · {notice.duplicates} already in library</>}
          {notice.failed.length > 0 && <> · {notice.failed.length} failed</>}
          <button
            type="button"
            className="banner__dismiss"
            onClick={() => setNotice(null)}
            aria-label="Dismiss"
          >
            ×
          </button>
          {notice.failed.length > 0 && (
            <ul className="banner__failures">
              {notice.failed.slice(0, 8).map((f) => (
                <li key={f.path}>
                  <code>{f.path}</code> — {f.reason}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      {importingCount !== null && <div className="progress" />}

      {!loading && assets.length === 0 ? (
        <div className="empty">
          <p className="empty__headline">
            {colorFilter ? "Nothing matches that colour." : "Drop images here."}
          </p>
          <p className="empty__detail">
            {colorFilter
              ? "Try a different hue — the tolerance is deliberately tight."
              : "Drag files or folders from Explorer. Everything stays on this machine."}
          </p>
          {root && !colorFilter && <code className="empty__path">{root}</code>}
        </div>
      ) : (
        <main className="grid">
          {assets.map((asset) => (
            <figure className="tile" key={asset.id}>
              <div className="tile__media">
                <img
                  src={thumbUrl(asset)}
                  alt={asset.originalName ?? asset.hash}
                  loading="lazy"
                  width={asset.width}
                  height={asset.height}
                />
                {asset.kind === "video" && (
                  <button
                    type="button"
                    className="tile__play"
                    onClick={() => setPlaying(asset)}
                    aria-label={`Play ${asset.originalName ?? "video"}`}
                  >
                    <span className="tile__playIcon" aria-hidden="true">
                      ▶
                    </span>
                    {asset.durationMs !== null && (
                      <span className="tile__duration">
                        {formatDuration(asset.durationMs)}
                      </span>
                    )}
                  </button>
                )}
              </div>
              <figcaption className="tile__meta">
                <span className="tile__name">
                  {asset.originalName ?? asset.hash.slice(0, 12)}
                </span>
                <span className="tile__dims">
                  {asset.width}×{asset.height} · {formatBytes(asset.bytes)}
                </span>
              </figcaption>
              <div className="palette">
                {asset.swatches.map((s, i) => (
                  <button
                    type="button"
                    key={`${asset.id}-${i}`}
                    className="palette__swatch"
                    style={{ background: s.hex, flexGrow: s.weight, color: swatchInk(s.l) }}
                    title={`${s.hex} — ${Math.round(s.weight * 100)}% · click to find similar`}
                    onClick={() => {
                      setColorFilter(s.hex);
                      setHexInput(s.hex);
                    }}
                  >
                    <span>{s.hex}</span>
                  </button>
                ))}
              </div>
            </figure>
          ))}
        </main>
      )}

      {dragging && (
        <div className="dropzone">
          <p>Release to import</p>
        </div>
      )}

      {playing && (
        <div
          className="player"
          role="dialog"
          aria-modal="true"
          aria-label={playing.originalName ?? "Video"}
          onClick={() => {
            setPlaying(null);
            setPlaybackFailed(false);
          }}
        >
          {/* Stop propagation so clicking the video itself does not dismiss. */}
          <div className="player__frame" onClick={(e) => e.stopPropagation()}>
            {isPlayableInline(playing) && !playbackFailed ? (
              <video
                src={blobUrl(playing)}
                controls
                autoPlay
                onError={() => setPlaybackFailed(true)}
              />
            ) : (
              <div className="player__fallback">
                <p>
                  This one won't play in the app
                  {playbackFailed ? " — the codec isn't supported here." : "."}
                </p>
                <p className="player__fallbackDetail">
                  {playing.mime} · {playing.ext.toUpperCase()}
                </p>
                <button type="button" onClick={() => void openPath(playing.blobPath)}>
                  Open in default player
                </button>
              </div>
            )}
            <div className="player__meta">
              <span>{playing.originalName ?? playing.hash.slice(0, 12)}</span>
              <button
                type="button"
                onClick={() => {
                  setPlaying(null);
                  setPlaybackFailed(false);
                }}
                aria-label="Close"
              >
                ×
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
