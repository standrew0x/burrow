import { useCallback, useEffect, useMemo, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { openPath } from "@tauri-apps/plugin-opener";

import {
  addToBoard,
  blobUrl,
  createBoard,
  deleteAssets,
  deleteBoard,
  importPaths,
  isPlayableInline,
  libraryRoot,
  listAssets,
  listBoardAssets,
  listBoards,
  moveToBoard,
  removeFromBoard,
  renameBoard,
  searchByColor,
  thumbUrl,
} from "./api";
import type { Asset, Board, FailedImport } from "./types";
import "./App.css";

interface Notice {
  imported: number;
  duplicates: number;
  failed: FailedImport[];
  deleted?: number;
  bytesFreed?: number;
}

/** OkLab search radius. See DEFAULT_COLOR_TOLERANCE in commands.rs for how
 *  this number was picked; keep the two in step. */
const COLOR_TOLERANCE = 0.05;

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDuration(ms: number): string {
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** Readable ink over a swatch, chosen from OkLab lightness. */
const swatchInk = (l: number) => (l > 0.62 ? "#111" : "#fff");

export default function App() {
  const [assets, setAssets] = useState<Asset[]>([]);
  const [boards, setBoards] = useState<Board[]>([]);
  const [loading, setLoading] = useState(true);
  const [dragging, setDragging] = useState(false);
  const [importingCount, setImportingCount] = useState<number | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [colorFilter, setColorFilter] = useState<string | null>(null);
  const [hexInput, setHexInput] = useState("");
  const [root, setRoot] = useState("");
  const [playing, setPlaying] = useState<Asset | null>(null);
  const [playbackFailed, setPlaybackFailed] = useState(false);

  /** null = the whole library. */
  const [activeBoardId, setActiveBoardId] = useState<number | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());

  const activeBoard = useMemo(
    () => boards.find((b) => b.id === activeBoardId) ?? null,
    [boards, activeBoardId],
  );

  const refreshBoards = useCallback(async () => {
    try {
      setBoards(await listBoards());
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refresh = useCallback(async () => {
    try {
      if (activeBoardId !== null) {
        setAssets(await listBoardAssets(activeBoardId, 500, 0));
      } else if (colorFilter) {
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
  }, [activeBoardId, colorFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    void refreshBoards();
    libraryRoot()
      .then(setRoot)
      .catch(() => {});
  }, [refreshBoards]);

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
        // A drop always lands in the library, so show it there rather than
        // leaving the user staring at an unchanged board.
        setActiveBoardId(null);
        setColorFilter(null);
        await refresh();
        await refreshBoards();
      } catch (e) {
        setError(String(e));
      } finally {
        setImportingCount(null);
      }
    },
    [refresh, refreshBoards],
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
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch((e) => setError(String(e)));

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [runImport]);

  const toggleSelected = (id: number) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const clearSelection = () => setSelected(new Set());

  const showBoard = (id: number | null) => {
    setActiveBoardId(id);
    setColorFilter(null);
    setHexInput("");
    clearSelection();
  };

  const submitHex = (e: React.FormEvent) => {
    e.preventDefault();
    const value = hexInput.trim();
    if (!value) return;
    // Colour search spans the whole library, so it leaves any active board.
    setActiveBoardId(null);
    setColorFilter(value.startsWith("#") ? value : `#${value}`);
    clearSelection();
  };

  const addSelectionTo = async (boardId: number) => {
    try {
      await addToBoard(boardId, [...selected]);
      clearSelection();
      await refreshBoards();
      if (activeBoardId !== null) await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const addSelectionToNewBoard = async () => {
    const name = window.prompt("Name this board");
    if (!name?.trim()) return;
    try {
      const board = await createBoard(name);
      await addToBoard(board.id, [...selected]);
      clearSelection();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    }
  };

  const removeSelectionFromBoard = async () => {
    if (activeBoardId === null) return;
    try {
      await removeFromBoard(activeBoardId, [...selected]);
      clearSelection();
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    }
  };

  const moveSelectionTo = async (toBoardId: number) => {
    if (activeBoardId === null) return;
    try {
      await moveToBoard(activeBoardId, toBoardId, [...selected]);
      clearSelection();
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    }
  };

  /** The only irreversible action in the app, so it states exactly what goes. */
  const deleteSelection = async () => {
    const count = selected.size;
    const confirmed = window.confirm(
      `Permanently delete ${count} reference${count === 1 ? "" : "s"}?\n\n` +
        `The stored file${count === 1 ? "" : "s"} and thumbnail${count === 1 ? "" : "s"} ` +
        `will be erased from your library, and ${count === 1 ? "it" : "they"} will be ` +
        `removed from every board.\n\nYour original file${count === 1 ? "" : "s"} on disk ` +
        `${count === 1 ? "is" : "are"} not touched. This cannot be undone.`,
    );
    if (!confirmed) return;

    try {
      const report = await deleteAssets([...selected]);
      clearSelection();
      setNotice({
        imported: 0,
        duplicates: 0,
        failed: report.orphanedFiles.map((path) => ({
          path,
          reason: "row deleted, but the file could not be unlinked",
        })),
        deleted: report.deleted,
        bytesFreed: report.bytesFreed,
      });
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    }
  };

  const heading = useMemo(() => {
    if (loading) return "Loading library…";
    if (importingCount !== null) {
      return `Importing ${importingCount} file${importingCount === 1 ? "" : "s"}…`;
    }
    if (activeBoard) {
      return `${assets.length} on ${activeBoard.name}`;
    }
    if (colorFilter) return `${assets.length} matching ${colorFilter}`;
    return `${assets.length} reference${assets.length === 1 ? "" : "s"}`;
  }, [loading, importingCount, activeBoard, colorFilter, assets.length]);

  return (
    <div className={`app${dragging ? " app--dragging" : ""}`}>
      <aside className="rail">
        <button
          type="button"
          className={`rail__item${activeBoardId === null ? " rail__item--active" : ""}`}
          onClick={() => showBoard(null)}
        >
          <span className="rail__name">All references</span>
        </button>

        <div className="rail__heading">Boards</div>

        {boards.length === 0 && (
          <p className="rail__empty">
            Select references, then group them into a named board.
          </p>
        )}

        {boards.map((board) => (
          <div
            key={board.id}
            className={`rail__row${board.id === activeBoardId ? " rail__item--active" : ""}`}
          >
            <button
              type="button"
              className="rail__item rail__item--board"
              onClick={() => showBoard(board.id)}
            >
              {board.coverThumbPath ? (
                <img
                  className="rail__cover"
                  src={thumbUrl({ thumbPath: board.coverThumbPath } as Asset)}
                  alt=""
                />
              ) : (
                <span className="rail__cover rail__cover--empty" />
              )}
              <span className="rail__name">{board.name}</span>
              <span className="rail__count">{board.itemCount}</span>
            </button>
            <div className="rail__actions">
              <button
                type="button"
                title="Rename"
                onClick={async () => {
                  const name = window.prompt("Rename board", board.name);
                  if (!name?.trim()) return;
                  await renameBoard(board.id, name);
                  await refreshBoards();
                }}
              >
                ✎
              </button>
              <button
                type="button"
                title="Delete board (references are kept)"
                onClick={async () => {
                  if (
                    !window.confirm(
                      `Delete the board "${board.name}"?\n\nThe ${board.itemCount} reference(s) on it stay in your library.`,
                    )
                  )
                    return;
                  await deleteBoard(board.id);
                  if (activeBoardId === board.id) setActiveBoardId(null);
                  await refreshBoards();
                }}
              >
                ×
              </button>
            </div>
          </div>
        ))}
      </aside>

      <div className="main">
        <header className="bar">
          <div className="bar__identity">
            <h1>{activeBoard ? activeBoard.name : "Burrow"}</h1>
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
            {notice.deleted !== undefined ? (
              <>
                <strong>{notice.deleted}</strong> deleted
                {notice.bytesFreed ? <> · {formatBytes(notice.bytesFreed)} freed</> : null}
              </>
            ) : (
              <>
                <strong>{notice.imported}</strong> imported
                {notice.duplicates > 0 && <> · {notice.duplicates} already in library</>}
              </>
            )}
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
              {activeBoard
                ? "This board is empty."
                : colorFilter
                  ? "Nothing matches that colour."
                  : "Drop images or video here."}
            </p>
            <p className="empty__detail">
              {activeBoard
                ? "Go to All references, select some tiles, and add them here."
                : colorFilter
                  ? "Try a different hue — the tolerance is deliberately tight."
                  : "Drag files or folders from Explorer. Everything stays on this machine."}
            </p>
            {root && !colorFilter && !activeBoard && (
              <code className="empty__path">{root}</code>
            )}
          </div>
        ) : (
          <main className="grid">
            {assets.map((asset) => {
              const isSelected = selected.has(asset.id);
              return (
                <figure
                  className={`tile${isSelected ? " tile--selected" : ""}`}
                  key={asset.id}
                >
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
                    {/* Above .tile__play, which covers the whole media box. */}
                    <button
                      type="button"
                      className={`tile__select${isSelected ? " tile__select--on" : ""}`}
                      onClick={() => toggleSelected(asset.id)}
                      aria-pressed={isSelected}
                      aria-label={isSelected ? "Deselect" : "Select"}
                    >
                      {isSelected ? "✓" : ""}
                    </button>
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
                        style={{
                          background: s.hex,
                          flexGrow: s.weight,
                          color: swatchInk(s.l),
                        }}
                        title={`${s.hex} — ${Math.round(s.weight * 100)}% · click to find similar`}
                        onClick={() => {
                          setActiveBoardId(null);
                          setColorFilter(s.hex);
                          setHexInput(s.hex);
                          clearSelection();
                        }}
                      >
                        <span>{s.hex}</span>
                      </button>
                    ))}
                  </div>
                </figure>
              );
            })}
          </main>
        )}
      </div>

      {selected.size > 0 && (
        <div className="tray">
          <span className="tray__count">
            {selected.size} selected
          </span>

          <select
            className="tray__picker"
            defaultValue=""
            onChange={(e) => {
              const value = e.target.value;
              e.target.value = "";
              if (value === "__new") void addSelectionToNewBoard();
              else if (value) void addSelectionTo(Number(value));
            }}
          >
            <option value="" disabled>
              Add to board…
            </option>
            {boards.map((b) => (
              <option key={b.id} value={b.id}>
                {b.name}
              </option>
            ))}
            <option value="__new">＋ New board…</option>
          </select>

          {/* Moving only makes sense from a board — from the library there is
              no source to move out of, which is what "Add to board" is for. */}
          {activeBoard && boards.length > 1 && (
            <select
              className="tray__picker"
              defaultValue=""
              onChange={(e) => {
                const value = e.target.value;
                e.target.value = "";
                if (value) void moveSelectionTo(Number(value));
              }}
            >
              <option value="" disabled>
                Move to…
              </option>
              {boards
                .filter((b) => b.id !== activeBoard.id)
                .map((b) => (
                  <option key={b.id} value={b.id}>
                    {b.name}
                  </option>
                ))}
            </select>
          )}

          {activeBoard && (
            <button type="button" onClick={() => void removeSelectionFromBoard()}>
              Remove from board
            </button>
          )}

          <button
            type="button"
            className="tray__delete"
            onClick={() => void deleteSelection()}
            title="Erase from the library and delete the stored files"
          >
            Delete…
          </button>

          <button type="button" className="tray__clear" onClick={clearSelection}>
            Clear
          </button>
        </div>
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
