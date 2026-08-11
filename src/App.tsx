import { useCallback, useEffect, useMemo, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { openPath } from "@tauri-apps/plugin-opener";

import {
  addLinks,
  addToBoard,
  createBoard,
  deleteAssets,
  deleteBoard,
  downloadAssets,
  importPaths,
  isPlayableInline,
  libraryRoot,
  listAssets,
  listBoardAssets,
  listBoards,
  listDismissed,
  moveToBoard,
  undismiss,
  playbackUrl,
  removeFromBoard,
  renameBoard,
  searchByColor,
  searchNotes,
  setNote,
  syncFromX,
  thumbUrl,
  clearXSession,
  saveXSession,
  xFolders,
  xStatus,
} from "./api";
import type {
  Asset,
  Board,
  Dismissed,
  FailedImport,
  SyncKinds,
  XStatus,
} from "./types";
import "./App.css";

interface Notice {
  imported: number;
  duplicates: number;
  failed: FailedImport[];
  deleted?: number;
  bytesFreed?: number;
  /** Present when the notice came from an X sync rather than a drop. */
  syncedFrom?: string;
  images?: number;
  videos?: number;
  /** Present when the notice came from downloading linked references. */
  downloaded?: number;
  bytesWritten?: number;
  deduplicated?: number;
  /** Skipped because they were deleted from the library before. */
  dismissed?: number;
  /**
   * Why a sync stopped, and whether asking for more would return more.
   *
   * A count on its own cannot tell those two apart, and reading "38 imported"
   * as "that is all there was" when it was really "that is all you asked for"
   * is exactly what makes a working sync feel like it is losing things.
   */
  stoppedBecause?: string;
  moreAvailable?: boolean;
  scanned?: string;
}

/** Anything that looks like a link the app could resolve. */
const URL_PATTERN = /^https?:\/\/\S+$/i;

/**
 * Pulls URLs out of pasted text.
 *
 * Split on whitespace rather than taking the whole string: copying a link out
 * of a page routinely drags along surrounding text, and pasting several at once
 * is the fastest way to add a batch.
 */
function urlsIn(text: string): string[] {
  return text
    .split(/\s+/)
    .map((s) => s.replace(/[),.]+$/, "").trim())
    .filter((s) => URL_PATTERN.test(s));
}

/** Batch sizes offered next to the Sync button.
 *
 *  These are presets, not the limit — the box beside them takes any number, and
 *  0 means everything. A fixed menu is what made a large bookmark collection
 *  feel permanently truncated: measured against the real account, the timeline
 *  holds ~1500 media items, so a menu topping out at 100 could never reach most
 *  of it.
 *
 *  Cost differs enormously by mode. Linking fetches one poster per item and is
 *  quick; downloading measured ~26s per video end to end, so a few hundred
 *  videos is hours. That is why link-only is the default. */
const SYNC_BATCH_OPTIONS = [10, 25, 50, 100, 250, 500] as const;
const DEFAULT_SYNC_BATCH = 25;
/** Matches MAX_SYNC_LIMIT in commands.rs; keep the two in step. */
const MAX_SYNC_BATCH = 5000;

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
  const [syncing, setSyncing] = useState(false);
  const [syncBatch, setSyncBatch] = useState<number>(DEFAULT_SYNC_BATCH);
  const [syncOpen, setSyncOpen] = useState(false);
  const [syncKinds, setSyncKinds] = useState<SyncKinds>("all");
  /** "" means every bookmark, not a folder. */
  const [syncFolder, setSyncFolder] = useState("");
  const [syncFrom, setSyncFrom] = useState("");
  const [syncTo, setSyncTo] = useState("");
  const [folders, setFolders] = useState<string[]>([]);
  const [foldersLoading, setFoldersLoading] = useState(false);
  const [foldersError, setFoldersError] = useState<string | null>(null);
  const [connectOpen, setConnectOpen] = useState(false);
  const [xState, setXState] = useState<XStatus | null>(null);
  const [xChecking, setXChecking] = useState(false);
  const [authTokenInput, setAuthTokenInput] = useState("");
  const [ct0Input, setCt0Input] = useState("");

  /** null = the whole library. */
  const [activeBoardId, setActiveBoardId] = useState<number | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [linkInput, setLinkInput] = useState("");
  /** Reference whose note is open for editing, and the draft text. */
  const [editingNote, setEditingNote] = useState<number | null>(null);
  const [noteDraft, setNoteDraft] = useState("");
  const [noteQuery, setNoteQuery] = useState("");
  const [noteFilter, setNoteFilter] = useState<string | null>(null);
  const [addingLinks, setAddingLinks] = useState(false);
  const [syncDownload, setSyncDownload] = useState(false);
  /** Viewing the tombstone list rather than any set of references. */
  const [showDismissed, setShowDismissed] = useState(false);
  const [dismissed, setDismissed] = useState<Dismissed[]>([]);
  /** Ids currently being fetched, so their tiles can show it. */
  const [downloading, setDownloading] = useState<Set<number>>(new Set());

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
      } else if (noteFilter) {
        setAssets(await searchNotes(noteFilter, 500));
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
  }, [activeBoardId, colorFilter, noteFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    void refreshBoards();
    libraryRoot()
      .then(setRoot)
      .catch(() => {});
    // One authenticated call at startup so the Sync button can say whether it
    // will actually work before the user presses it.
    setXChecking(true);
    xStatus()
      .then(setXState)
      .catch(() => {})
      .finally(() => setXChecking(false));
  }, [refreshBoards]);

  const connectX = async () => {
    setXChecking(true);
    setError(null);
    try {
      const status = await saveXSession(authTokenInput, ct0Input);
      setXState(status);
      if (status.connected) {
        // Only clear the fields on success; keeping them on failure lets the
        // user fix one value rather than re-paste both.
        setAuthTokenInput("");
        setCt0Input("");
        setConnectOpen(false);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setXChecking(false);
    }
  };

  const disconnectX = async () => {
    try {
      setXState(await clearXSession());
      setFolders([]);
    } catch (e) {
      setError(String(e));
    }
  };

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

  const runAddLinks = useCallback(
    async (urls: string[]) => {
      if (urls.length === 0) return;
      setAddingLinks(true);
      setError(null);
      setNotice(null);
      try {
        const report = await addLinks(urls);
        setActiveBoardId(null);
        setColorFilter(null);
        setNotice({
          imported: report.imported.length,
          duplicates: report.duplicates,
          // Without this, pasting a link you deleted earlier reports a bare
          // "0 imported" and looks broken, which is the whole failure mode
          // tombstones were supposed to avoid rather than create.
          dismissed: report.dismissed,
          failed: report.failed,
        });
        setLinkInput("");
        await refresh();
        await refreshBoards();
      } catch (e) {
        setError(String(e));
      } finally {
        setAddingLinks(false);
      }
    },
    [refresh, refreshBoards],
  );

  // Paste is the primary way links get in. Dragging a link cannot be caught:
  // Tauri's dragDropEnabled (which the file drop above depends on) makes
  // WebView2 hand OS drops to Rust as file paths and suppresses the DOM drop
  // event, and a dragged URL carries no paths — so it arrives as an empty list
  // with nothing to read. Turning that off to catch URL drags would break file
  // drops, which is the worse trade.
  useEffect(() => {
    const onPaste = (e: ClipboardEvent) => {
      const target = e.target as HTMLElement | null;
      // Never steal a paste aimed at a field — the cookie inputs and the link
      // box itself are all typed into.
      if (
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable)
      ) {
        return;
      }
      const urls = urlsIn(e.clipboardData?.getData("text") ?? "");
      if (urls.length === 0) return;
      e.preventDefault();
      void runAddLinks(urls);
    };
    window.addEventListener("paste", onPaste);
    return () => window.removeEventListener("paste", onPaste);
  }, [runAddLinks]);

  const runDownload = useCallback(
    async (ids: number[]) => {
      if (ids.length === 0) return;
      setDownloading(new Set(ids));
      setError(null);
      setNotice(null);
      try {
        const report = await downloadAssets(ids);
        setNotice({
          imported: 0,
          duplicates: 0,
          failed: report.failed,
          downloaded: report.downloaded.length,
          bytesWritten: report.bytesWritten,
          deduplicated: report.deduplicated,
        });
        await refresh();
        await refreshBoards();
      } catch (e) {
        setError(String(e));
      } finally {
        setDownloading(new Set());
      }
    },
    [refresh, refreshBoards],
  );

  const openNote = (asset: Asset) => {
    setEditingNote(asset.id);
    setNoteDraft(asset.note ?? "");
  };

  const cancelNote = () => {
    setEditingNote(null);
    setNoteDraft("");
  };

  const saveNote = async (assetId: number) => {
    const draft = noteDraft;
    // Close first: the write is fast and local, and leaving the editor open
    // until the round trip returns makes typing feel like it stuck.
    setEditingNote(null);
    setNoteDraft("");
    try {
      const stored = await setNote(assetId, draft);
      // Patch in place rather than refetching the whole grid, which would
      // scroll the user away from what they were annotating.
      setAssets((prev) =>
        prev.map((a) => (a.id === assetId ? { ...a, note: stored } : a)),
      );
    } catch (e) {
      setError(String(e));
    }
  };

  const toggleSelected = (id: number) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const clearSelection = () => setSelected(new Set());

  /** Selected references that still live on someone else's server. */
  const selectedLinked = useMemo(
    () =>
      assets.filter((a) => selected.has(a.id) && a.state === "linked").map((a) => a.id),
    [assets, selected],
  );

  const showBoard = (id: number | null) => {
    setActiveBoardId(id);
    setColorFilter(null);
    setHexInput("");
    setNoteFilter(null);
    setNoteQuery("");
    setShowDismissed(false);
    clearSelection();
  };

  const refreshDismissed = useCallback(async () => {
    try {
      setDismissed(await listDismissed(500));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const openDismissed = async () => {
    setShowDismissed(true);
    clearSelection();
    await refreshDismissed();
  };

  /** Lets a reference be offered by sync again. Does not restore it by itself. */
  const allowAgain = async (urls: string[]) => {
    try {
      await undismiss(urls);
      await refreshDismissed();
    } catch (e) {
      setError(String(e));
    }
  };

  const runSync = async () => {
    setSyncing(true);
    setNotice(null);
    setError(null);
    try {
      const report = await syncFromX({
        limit: syncBatch,
        folder: syncFolder || undefined,
        from: syncFrom || undefined,
        to: syncTo || undefined,
        kinds: syncKinds,
        download: syncDownload,
      });
      // A sync always lands in the library, so show it there rather than
      // leaving the user on a board that did not change.
      setActiveBoardId(null);
      setColorFilter(null);
      setNotice({
        imported: report.imported,
        duplicates: report.duplicates,
        dismissed: report.dismissed,
        failed: report.failed,
        syncedFrom: `${report.source} · ${report.found} found`,
        images: report.images,
        videos: report.videos,
        stoppedBecause: report.stoppedBecause,
        moreAvailable: report.moreAvailable,
        scanned: `${report.postsScanned} posts over ${report.pages} page${
          report.pages === 1 ? "" : "s"
        }`,
      });
      setSyncOpen(false);
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    } finally {
      setSyncing(false);
    }
  };

  const submitHex = (e: React.FormEvent) => {
    e.preventDefault();
    const value = hexInput.trim();
    if (!value) return;
    // Colour search spans the whole library, so it leaves any active board.
    setActiveBoardId(null);
    setNoteFilter(null);
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
    const plural = count === 1 ? "" : "s";
    // How many of these will also stop being offered by future syncs. Stated up
    // front: finding out later that deleting quietly changed what sync returns
    // is indistinguishable from the sync breaking.
    const fromSource = assets.filter(
      (a) => selected.has(a.id) && (a.remoteUrl || a.sourceUrl),
    ).length;
    const confirmed = window.confirm(
      `Permanently delete ${count} reference${plural}?\n\n` +
        `The stored file${plural} and thumbnail${plural} will be erased from your ` +
        `library, and ${count === 1 ? "it" : "they"} will be removed from every ` +
        `board.\n\nYour original file${plural} on disk ${count === 1 ? "is" : "are"} ` +
        `not touched. This cannot be undone.` +
        (fromSource > 0
          ? `\n\n${fromSource} of these came from a link or from X, so ${
              fromSource === 1 ? "it" : "they"
            } will also be kept out of future syncs. You can undo that under ` +
            `Dismissed in the sidebar.`
          : ""),
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
        dismissed: report.dismissed,
        bytesFreed: report.bytesFreed,
      });
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    }
  };

  const heading = useMemo(() => {
    if (showDismissed) {
      return `${dismissed.length} kept out of sync`;
    }
    if (loading) return "Loading library…";
    if (importingCount !== null) {
      return `Importing ${importingCount} file${importingCount === 1 ? "" : "s"}…`;
    }
    if (activeBoard) {
      return `${assets.length} on ${activeBoard.name}`;
    }
    if (noteFilter) return `${assets.length} noting “${noteFilter}”`;
    if (colorFilter) return `${assets.length} matching ${colorFilter}`;
    return `${assets.length} reference${assets.length === 1 ? "" : "s"}`;
  }, [
    loading,
    importingCount,
    activeBoard,
    colorFilter,
    noteFilter,
    assets.length,
    showDismissed,
    dismissed.length,
  ]);

  return (
    <div className={`app${dragging ? " app--dragging" : ""}`}>
      <aside className="rail">
        <button
          type="button"
          className={`rail__item${
            activeBoardId === null && !showDismissed ? " rail__item--active" : ""
          }`}
          onClick={() => showBoard(null)}
        >
          <span className="rail__name">All references</span>
        </button>

        {/* Deliberately always visible, not only when non-empty. A rule that
            silently withholds sync results has to be findable before you know
            to go looking for it. */}
        <button
          type="button"
          className={`rail__item${showDismissed ? " rail__item--active" : ""}`}
          onClick={() => void openDismissed()}
          title="References you deleted, which sync will not offer again"
        >
          <span className="rail__name">Dismissed</span>
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
            <h1>
              {showDismissed ? "Dismissed" : activeBoard ? activeBoard.name : "Burrow"}
            </h1>
            <span className="bar__count">{heading}</span>
          </div>

          <div className="bar__actions">
            <span
              className={`bar__xdot${xState?.connected ? " bar__xdot--on" : ""}`}
              title={
                xChecking
                  ? "Checking X connection…"
                  : (xState?.detail ?? "Not connected to X")
              }
              aria-hidden="true"
            />
            <button
              type="button"
              className="bar__sync"
              onClick={() => setConnectOpen((v) => !v)}
            >
              {xState?.connected ? "X connected" : "Connect X"}
            </button>
            <button
              type="button"
              className="bar__sync"
              disabled={syncing || !xState?.connected}
              title={
                xState?.connected
                  ? "Choose what to pull from your bookmarks"
                  : "Connect your X account first"
              }
              onClick={() => {
                const next = !syncOpen;
                setSyncOpen(next);
                // Folder names cost a round trip to X, so only fetch them when
                // the panel is actually opened.
                if (next && folders.length === 0 && !foldersLoading) {
                  setFoldersLoading(true);
                  setFoldersError(null);
                  xFolders()
                    .then(setFolders)
                    .catch((e) => setFoldersError(String(e)))
                    .finally(() => setFoldersLoading(false));
                }
              }}
              aria-expanded={syncOpen}
            >
              {syncing ? `Syncing ${syncBatch}…` : "Sync from X"}
            </button>
          </div>

          <form
            className="bar__link"
            onSubmit={(e) => {
              e.preventDefault();
              void runAddLinks(urlsIn(linkInput));
            }}
          >
            <input
              type="url"
              value={linkInput}
              onChange={(e) => setLinkInput(e.target.value)}
              placeholder="Paste a link…"
              aria-label="Add a reference from a URL"
              spellCheck={false}
            />
            <button type="submit" disabled={addingLinks || !urlsIn(linkInput).length}>
              {addingLinks ? "Adding…" : "Add link"}
            </button>
          </form>

          <form
            className="bar__search"
            onSubmit={(e) => {
              e.preventDefault();
              const q = noteQuery.trim();
              if (!q) return;
              // Searching notes spans the library, so it leaves any board or
              // colour filter rather than intersecting with them.
              setActiveBoardId(null);
              setColorFilter(null);
              setHexInput("");
              setNoteFilter(q);
              clearSelection();
            }}
          >
            <input
              type="search"
              value={noteQuery}
              onChange={(e) => setNoteQuery(e.target.value)}
              placeholder="Search notes…"
              aria-label="Search notes"
            />
            <button type="submit">Find</button>
            {noteFilter && (
              <button
                type="button"
                className="bar__clear"
                onClick={() => {
                  setNoteFilter(null);
                  setNoteQuery("");
                }}
              >
                Clear
              </button>
            )}
          </form>

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

        {connectOpen && (
          <div className="connect">
            <div className="connect__head">
              <strong>Connect your X account</strong>
              <span
                className={xState?.connected ? "connect__ok" : "connect__bad"}
              >
                {xChecking
                  ? "checking…"
                  : xState?.connected
                    ? `connected — ${xState.detail}`
                    : (xState?.detail ?? "not connected")}
              </span>
            </div>

            <ol className="connect__steps">
              <li>Open <code>x.com</code> in your browser, signed in.</li>
              <li>Press <kbd>F12</kbd>, then open the <b>Application</b> tab.</li>
              <li>In the sidebar choose <b>Cookies &rarr; https://x.com</b>.</li>
              <li>
                Find <code>auth_token</code> and <code>ct0</code> and copy each
                value into the boxes below.
              </li>
            </ol>

            <div className="connect__fields">
              <label>
                <span>auth_token</span>
                <input
                  type="password"
                  value={authTokenInput}
                  onChange={(e) => setAuthTokenInput(e.target.value)}
                  placeholder="40 characters"
                  spellCheck={false}
                  autoComplete="off"
                />
              </label>
              <label>
                <span>ct0</span>
                <input
                  type="password"
                  value={ct0Input}
                  onChange={(e) => setCt0Input(e.target.value)}
                  placeholder="160 characters"
                  spellCheck={false}
                  autoComplete="off"
                />
              </label>
              <button
                type="button"
                className="sync__go"
                onClick={() => void connectX()}
                disabled={xChecking || !authTokenInput.trim() || !ct0Input.trim()}
              >
                {xChecking ? "Checking…" : "Connect"}
              </button>
              {xState?.hasSession && (
                <button type="button" onClick={() => void disconnectX()}>
                  Disconnect
                </button>
              )}
            </div>

            <p className="connect__note">
              These are stored only on this PC, in your library folder, and are
              sent nowhere except x.com. They are as powerful as your password,
              so do not share them. X expires them every few weeks &mdash; when
              Sync starts failing, paste fresh ones here.
            </p>
          </div>
        )}

        {syncOpen && (
          <div className="sync">
            <label>
              <span>From</span>
              <select value={syncFolder} onChange={(e) => setSyncFolder(e.target.value)}>
                <option value="">All bookmarks</option>
                {folders.map((f) => (
                  <option key={f} value={f}>
                    {f}
                  </option>
                ))}
              </select>
            </label>

            <label>
              <span>Media</span>
              <select
                value={syncKinds}
                onChange={(e) => setSyncKinds(e.target.value as SyncKinds)}
              >
                <option value="all">Images and video</option>
                <option value="images">Images only</option>
                <option value="videos">Video only</option>
              </select>
            </label>

            <label>
              <span>Fetch</span>
              <select
                value={syncDownload ? "download" : "link"}
                onChange={(e) => setSyncDownload(e.target.value === "download")}
              >
                <option value="link">Thumbnails only (fast)</option>
                <option value="download">Full media (slow, large)</option>
              </select>
            </label>

            <label>
              <span>Posted after</span>
              <input type="date" value={syncFrom} onChange={(e) => setSyncFrom(e.target.value)} />
            </label>

            <label>
              <span>and before</span>
              <input type="date" value={syncTo} onChange={(e) => setSyncTo(e.target.value)} />
            </label>

            {/* A preset menu answers the common case and the box answers the
                rest. Either alone is what made a 1500-item bookmark list feel
                permanently truncated. */}
            <label>
              <span>How many</span>
              <select
                value={
                  // 0 is the same request as picking "Everything", so the menu
                  // has to say so — otherwise choosing Everything and typing 0
                  // leave the control reading two different things.
                  syncBatch === 0
                    ? "all"
                    : SYNC_BATCH_OPTIONS.includes(syncBatch as never)
                      ? syncBatch
                      : "custom"
                }
                onChange={(e) => {
                  const v = e.target.value;
                  if (v === "all") setSyncBatch(0);
                  else if (v !== "custom") setSyncBatch(Number(v));
                }}
              >
                {SYNC_BATCH_OPTIONS.map((n) => (
                  <option key={n} value={n}>
                    {n} items
                  </option>
                ))}
                <option value="all">Everything</option>
                <option value="custom">Custom…</option>
              </select>
            </label>

            <label>
              <span>{syncBatch === 0 ? "All of them" : "Exactly"}</span>
              <input
                type="number"
                min={0}
                max={MAX_SYNC_BATCH}
                step={1}
                value={syncBatch}
                aria-label="Number of items to sync, 0 for everything"
                title="0 syncs everything it can reach"
                onChange={(e) => {
                  // Clamped here rather than only in Rust so the field cannot
                  // display a number the sync will silently not honour.
                  const n = Number(e.target.value);
                  if (Number.isFinite(n)) {
                    setSyncBatch(Math.max(0, Math.min(MAX_SYNC_BATCH, Math.floor(n))));
                  }
                }}
              />
            </label>

            <button type="button" className="sync__go" onClick={() => void runSync()} disabled={syncing}>
              {syncing ? "Syncing…" : "Start sync"}
            </button>

            <p className="sync__note">
              Videos take roughly 26s each; images are near-instant. Dates are
              when the post was made, not when you bookmarked it.
              {foldersLoading && " Loading folders…"}
              {foldersError && (
                <>
                  {" "}
                  Folder list unavailable, so only <b>All bookmarks</b> is
                  offered — bookmark folders are an X Premium feature. Syncing
                  everything still works.
                </>
              )}
            </p>
          </div>
        )}

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
                {notice.dismissed ? (
                  <> · {notice.dismissed} will not be re-synced</>
                ) : null}
              </>
            ) : notice.downloaded !== undefined ? (
              <>
                <strong>{notice.downloaded}</strong> downloaded
                {notice.bytesWritten ? <> · {formatBytes(notice.bytesWritten)}</> : null}
                {/* Not a failure: the bytes were already here, so the link was
                    merged into the copy that has them. */}
                {notice.deduplicated ? (
                  <> · {notice.deduplicated} already in library</>
                ) : null}
              </>
            ) : (
              <>
                <strong>{notice.imported}</strong> imported
                {notice.duplicates > 0 && <> · {notice.duplicates} already in library</>}
                {notice.images !== undefined && (notice.images > 0 || notice.videos! > 0) && (
                  <> ({notice.images} image{notice.images === 1 ? "" : "s"},{" "}
                  {notice.videos} video{notice.videos === 1 ? "" : "s"})</>
                )}
                {notice.dismissed ? (
                  <> · {notice.dismissed} skipped (deleted before)</>
                ) : null}
                {notice.syncedFrom && <> · from {notice.syncedFrom}</>}
                {/* The line that answers "why didn't it get everything?".
                    Without it, a limit and an exhausted list look identical. */}
                {notice.stoppedBecause && (
                  <div className="banner__why">
                    {notice.stoppedBecause}
                    {notice.scanned && <> · {notice.scanned}</>}
                    {notice.moreAvailable && (
                      <> · <b>ask for a larger number to get more</b></>
                    )}
                  </div>
                )}
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

        {(importingCount !== null || syncing) && <div className="progress" />}

        {showDismissed ? (
          <section className="dismissed">
            <p className="dismissed__lede">
              These were deleted on purpose, so syncing from X will not offer
              them again. Allowing one back does not restore it — it just lets
              the next sync pick it up.
            </p>
            {dismissed.length === 0 ? (
              <p className="empty__detail">
                Nothing is being skipped. Deleting a reference that came from a
                link or from X will add it here.
              </p>
            ) : (
              <>
                <div className="dismissed__bar">
                  <span>
                    {dismissed.length} skipped
                  </span>
                  <button
                    type="button"
                    onClick={() => {
                      if (
                        window.confirm(
                          `Allow all ${dismissed.length} back?\n\nThe next sync will offer them again.`,
                        )
                      )
                        void allowAgain([]);
                    }}
                  >
                    Allow all again
                  </button>
                </div>
                <ul className="dismissed__list">
                  {dismissed.map((d) => (
                    <li className="dismissed__row" key={d.remoteUrl}>
                      <span className="dismissed__text">
                        <span className="dismissed__title">
                          {d.title ?? d.pageUrl ?? "Untitled reference"}
                        </span>
                        <span className="dismissed__url">{d.remoteUrl}</span>
                      </span>
                      <button
                        type="button"
                        onClick={() => void allowAgain([d.remoteUrl])}
                      >
                        Allow again
                      </button>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </section>
        ) : !loading && assets.length === 0 ? (
          <div className="empty">
            <p className="empty__headline">
              {activeBoard
                ? "This board is empty."
                : noteFilter
                  ? `No notes mention “${noteFilter}”.`
                  : colorFilter
                    ? "Nothing matches that colour."
                    : "Drop images or video here, or paste a link."}
            </p>
            <p className="empty__detail">
              {activeBoard
                ? "Go to All references, select some tiles, and add them here."
                : noteFilter
                  ? "Notes match on any part of a word, so try a shorter fragment."
                  : colorFilter
                    ? "Try a different hue — the tolerance is deliberately tight."
                    : "Drag files or folders from Explorer, or press Ctrl+V with a link copied. Linked references store only a thumbnail until you download them."}
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
                    {asset.state === "linked" && (
                      <button
                        type="button"
                        className={`tile__download${
                          downloading.has(asset.id) ? " tile__download--busy" : ""
                        }`}
                        disabled={downloading.has(asset.id)}
                        onClick={() => void runDownload([asset.id])}
                        title={
                          downloading.has(asset.id)
                            ? "Downloading…"
                            : "Linked — streams from its source. Click to save a copy."
                        }
                        aria-label={`Download ${asset.originalName ?? "reference"}`}
                      >
                        {downloading.has(asset.id) ? "…" : "↓"}
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
                    {/* Stays visible once a note exists, so an annotated tile
                        is identifiable without hovering every one. */}
                    <button
                      type="button"
                      className={`tile__note${asset.note ? " tile__note--has" : ""}`}
                      onClick={() => openNote(asset)}
                      title={asset.note ?? "Add a note"}
                      aria-label={asset.note ? "Edit note" : "Add a note"}
                    >
                      {asset.note ? "✎" : "+"}
                    </button>
                  </div>
                  <figcaption className="tile__meta">
                    <span className="tile__name">
                      {asset.originalName ?? asset.hash.slice(0, 12)}
                    </span>
                    <span className="tile__dims">
                      {asset.width}×{asset.height} ·{" "}
                      {asset.state === "linked" ? (
                        <span className="tile__linked" title={asset.remoteUrl ?? ""}>
                          linked
                        </span>
                      ) : (
                        formatBytes(asset.bytes)
                      )}
                    </span>
                  </figcaption>
                  {editingNote === asset.id ? (
                    <div className="note note--editing">
                      <textarea
                        className="note__input"
                        value={noteDraft}
                        onChange={(e) => setNoteDraft(e.target.value)}
                        placeholder="What is this for?"
                        rows={3}
                        autoFocus
                        onKeyDown={(e) => {
                          // Enter inserts a newline; notes are prose. Ctrl+Enter
                          // commits, Escape abandons — the shape people expect
                          // from an inline editor.
                          if (e.key === "Escape") {
                            e.preventDefault();
                            cancelNote();
                          } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                            e.preventDefault();
                            void saveNote(asset.id);
                          }
                        }}
                        // Clicking away saves rather than discarding: losing
                        // typing to a stray click is the worse failure.
                        onBlur={() => void saveNote(asset.id)}
                      />
                      <div className="note__hint">Ctrl+Enter saves · Esc cancels</div>
                    </div>
                  ) : (
                    asset.note && (
                      <p
                        className="note"
                        onClick={() => openNote(asset)}
                        title="Click to edit"
                      >
                        {asset.note}
                      </p>
                    )
                  )}
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

          {selectedLinked.length > 0 && (
            <button
              type="button"
              disabled={downloading.size > 0}
              onClick={() => void runDownload(selectedLinked)}
              title="Fetch the media for the linked references in this selection"
            >
              {downloading.size > 0
                ? `Downloading ${downloading.size}…`
                : `Download ${selectedLinked.length}`}
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
            {isPlayableInline(playing) && !playbackFailed && playbackUrl(playing) ? (
              <video
                // A linked reference streams straight from its host; a local one
                // plays off disk. The poster is the cached thumbnail either way,
                // so there is something on screen before the first frame lands.
                src={playbackUrl(playing) ?? undefined}
                poster={thumbUrl(playing)}
                controls
                autoPlay
                onError={() => setPlaybackFailed(true)}
              />
            ) : (
              <div className="player__fallback">
                <p>
                  {playing.state === "linked" && playbackFailed
                    ? "That didn't stream — the link may have expired."
                    : `This one won't play in the app${
                        playbackFailed ? " — the codec isn't supported here." : "."
                      }`}
                </p>
                <p className="player__fallbackDetail">
                  {playing.mime} · {playing.ext.toUpperCase()}
                </p>
                {playing.state === "linked" ? (
                  <button
                    type="button"
                    disabled={downloading.has(playing.id)}
                    onClick={() => void runDownload([playing.id])}
                  >
                    {downloading.has(playing.id) ? "Downloading…" : "Download it"}
                  </button>
                ) : (
                  <button type="button" onClick={() => void openPath(playing.blobPath)}>
                    Open in default player
                  </button>
                )}
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
