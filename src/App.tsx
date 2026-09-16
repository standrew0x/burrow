import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";

import {
  addLinks,
  addToBoard,
  createBoard,
  captureVideoFrame,
  captureRenderedVideoFrame,
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
  blobUrl,
  removeFromBoard,
  renameBoard,
  searchAssets,
  setNote,
  syncFromX,
  thumbUrl,
  clearXSession,
  saveXSession,
  xFolders,
  xDownloadsFolder,
  xStatus,
  xVideoQualities,
} from "./api";
import type {
  Asset,
  Board,
  Dismissed,
  DownloadReport,
  FailedImport,
  SyncKinds,
  XStatus,
  XVideoQuality,
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
  /** Present when a video frame was saved into the library. */
  captured?: string;
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

function sourceLabel(value: string | null): string | null {
  if (!value) return null;
  try {
    const host = new URL(value).hostname.replace(/^www\./, "");
    if (["youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"].includes(host)) {
      return "YouTube";
    }
    if (host === "open.spotify.com" || host === "spotify.link") return "Spotify";
    if (host === "music.apple.com") return "Apple Music";
    if (["x.com", "twitter.com", "mobile.x.com", "mobile.twitter.com"].includes(host)) {
      return "X";
    }
    return "Source";
  } catch {
    return null;
  }
}

function isMusicSource(value: string | null): boolean {
  const label = sourceLabel(value);
  return label === "YouTube" || label === "Spotify" || label === "Apple Music";
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
/** Load the full supported library so a client-side sort is not just sorting
 *  the newest page and quietly omitting older saves. */
const VIEW_LIMIT = MAX_SYNC_BATCH;

/**
 * Recording-only mode. It never reads or writes X credentials and is inert in
 * normal builds. Local paths supplied by the demo launcher are still imported
 * through Burrow's real ingestion pipeline so the resulting library is honest.
 */
const DEMO_MODE = import.meta.env.VITE_BURROW_DEMO === "1";
const DEMO_X_PATHS = (import.meta.env.VITE_BURROW_DEMO_X_PATHS ?? "")
  .split(/[|;]/)
  .map((path: string) => path.trim())
  .filter(Boolean);
const DEMO_IMPORT_PATHS = (import.meta.env.VITE_BURROW_DEMO_IMPORT_PATHS ?? "")
  // PowerShell treats semicolons naturally in environment values; accept both
  // separators so the demo picker remains portable across launch methods.
  .split(/[|;]/)
  .map((path: string) => path.trim())
  .filter(Boolean);

/** OkLab search radius. See DEFAULT_COLOR_TOLERANCE in commands.rs for how
 *  this number was picked; keep the two in step. */
const COLOR_TOLERANCE = 0.05;

function normalizedColorQuery(value: string | null): string | null {
  if (!value) return null;
  const digits = value.trim().replace(/^#/, "");
  if (!/^(?:[0-9a-f]{3}|[0-9a-f]{6})$/i.test(digits)) return null;
  return `#${digits.toLowerCase()}`;
}

async function renderedFramePng(video: HTMLVideoElement): Promise<number[]> {
  if (video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA || !video.videoWidth || !video.videoHeight) {
    throw new Error("The video frame is not ready yet. Press play, then try again.");
  }
  const canvas = document.createElement("canvas");
  canvas.width = video.videoWidth;
  canvas.height = video.videoHeight;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("Burrow could not create a snapshot canvas.");
  context.drawImage(video, 0, 0, canvas.width, canvas.height);
  const blob = await new Promise<Blob>((resolve, reject) => {
    try {
      canvas.toBlob(
        (value) => (value ? resolve(value) : reject(new Error("The video frame could not be encoded."))),
        "image/png",
      );
    } catch {
      reject(new Error("The video source did not allow its frame to be captured."));
    }
  });
  return Array.from(new Uint8Array(await blob.arrayBuffer()));
}

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

type SortKey = "added" | "name" | "posted" | "xSaved" | "kind" | "size" | "duration";
type SortDirection = "asc" | "desc";
type SourceFilter = "all" | "local" | "x" | "links";
type Theme = "light" | "dark";

const NAME_COLLATOR = new Intl.Collator(undefined, {
  numeric: true,
  sensitivity: "base",
});

/** Compare decimal strings without losing precision in JavaScript numbers. */
function compareDecimalStrings(a: string, b: string): number {
  if (!/^\d+$/.test(a) || !/^\d+$/.test(b)) return NAME_COLLATOR.compare(a, b);
  const left = a.replace(/^0+/, "") || "0";
  const right = b.replace(/^0+/, "") || "0";
  return left.length === right.length
    ? left.localeCompare(right)
    : left.length - right.length;
}

function defaultDirection(key: SortKey): SortDirection {
  return key === "name" || key === "kind" ? "asc" : "desc";
}

function directionLabel(key: SortKey, direction: SortDirection): string {
  if (key === "name" || key === "kind") return direction === "asc" ? "A–Z" : "Z–A";
  if (key === "size") return direction === "asc" ? "Smallest first" : "Largest first";
  if (key === "duration") return direction === "asc" ? "Shortest first" : "Longest first";
  if (key === "xSaved") return direction === "asc" ? "Earlier saves first" : "Recent saves first";
  if (key === "posted") return direction === "asc" ? "Oldest posts first" : "Newest posts first";
  return direction === "asc" ? "Oldest added first" : "Newest added first";
}

function formatSourceDate(value: string): string {
  // Legacy rows contain YYYY-MM-DD; newer X syncs preserve the exact time.
  // Noon keeps the legacy form from shifting back a day in western timezones.
  const date = new Date(value.includes("T") ? value : `${value}T12:00:00`);
  return Number.isNaN(date.getTime())
    ? value
    : new Intl.DateTimeFormat(undefined, {
        month: "short",
        day: "numeric",
        year: "numeric",
        ...(value.includes("T") ? { hour: "numeric", minute: "2-digit" } : {}),
      }).format(date);
}

function formatImportedDate(value: number): string {
  // v9 stores microseconds. Accept older second/millisecond values as well so
  // an open view stays readable while its database migration completes.
  const milliseconds =
    value >= 100_000_000_000_000
      ? value / 1000
      : value >= 100_000_000_000
        ? value
        : value * 1000;
  const date = new Date(milliseconds);
  return Number.isNaN(date.getTime())
    ? "Unknown"
    : new Intl.DateTimeFormat(undefined, {
        month: "short",
        day: "numeric",
        year: "numeric",
        hour: "numeric",
        minute: "2-digit",
      }).format(date);
}

function sourceGroup(asset: Asset): Exclude<SourceFilter, "all"> {
  if (!asset.sourceUrl && !asset.remoteUrl) return "local";
  return sourceLabel(asset.sourceUrl) === "X" ? "x" : "links";
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
  const [root, setRoot] = useState("");
  const [viewing, setViewing] = useState<Asset | null>(null);
  const [viewerFullscreen, setViewerFullscreen] = useState(false);
  const viewerFrame = useRef<HTMLDivElement | null>(null);
  const viewerVideo = useRef<HTMLVideoElement | null>(null);
  const [capturingFrame, setCapturingFrame] = useState(false);
  const [playbackFailed, setPlaybackFailed] = useState(false);
  const [videoQualities, setVideoQualities] = useState<XVideoQuality[]>([]);
  const [videoQualityUrl, setVideoQualityUrl] = useState<string | null>(null);
  const [qualitiesLoading, setQualitiesLoading] = useState(false);
  const [qualityError, setQualityError] = useState<string | null>(null);
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
  const [searchQuery, setSearchQuery] = useState("");
  const [searchFilter, setSearchFilter] = useState<string | null>(null);
  const [addingLinks, setAddingLinks] = useState(false);
  const [syncDownload, setSyncDownload] = useState(false);
  /** Viewing the tombstone list rather than any set of references. */
  const [showDismissed, setShowDismissed] = useState(false);
  const [dismissed, setDismissed] = useState<Dismissed[]>([]);
  /** Ids currently being fetched, so their tiles can show it. */
  const [downloading, setDownloading] = useState<Set<number>>(new Set());
  const [sortKey, setSortKey] = useState<SortKey>("added");
  const [sortDirection, setSortDirection] = useState<SortDirection>("desc");
  const [sourceFilter, setSourceFilter] = useState<SourceFilter>("all");
  const [theme, setTheme] = useState<Theme>(() =>
    window.localStorage.getItem("burrow-theme") === "dark" ? "dark" : "light",
  );
  const selectionAnchor = useRef<number | null>(null);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  const activeBoard = useMemo(
    () => boards.find((b) => b.id === activeBoardId) ?? null,
    [boards, activeBoardId],
  );

  const sortedAssets = useMemo(() => {
    const value = (asset: Asset): string | number | null => {
      switch (sortKey) {
        case "name":
          return asset.originalName ?? asset.hash;
        case "posted":
          return asset.postedAt;
        case "xSaved":
          return asset.xBookmarkSortIndex;
        case "kind":
          return `${asset.kind} ${asset.state}`;
        case "size":
          return asset.bytes;
        case "duration":
          return asset.durationMs;
        case "added":
        default:
          return asset.importedAt;
      }
    };

    const visible =
      sourceFilter === "all"
        ? assets
        : assets.filter((asset) => sourceGroup(asset) === sourceFilter);

    return [...visible].sort((left, right) => {
      const a = value(left);
      const b = value(right);
      // Unknown source dates/order and image durations always go last. Flipping
      // direction should not make rows with no value look newest or largest.
      if (a === null && b !== null) return 1;
      if (a !== null && b === null) return -1;
      if (a === null && b === null)
        return sortDirection === "asc" ? left.id - right.id : right.id - left.id;

      let compared: number;
      if (sortKey === "xSaved") {
        compared = compareDecimalStrings(String(a), String(b));
      } else if (typeof a === "number" && typeof b === "number") {
        compared = a - b;
      } else {
        compared = NAME_COLLATOR.compare(String(a), String(b));
      }
      if (compared === 0) {
        // IDs reflect insertion order and make same-batch timestamps stable.
        // Filename was the old fallback, which made date sorts look random.
        compared = left.id - right.id;
      }
      return sortDirection === "asc" ? compared : -compared;
    });
  }, [assets, sortDirection, sortKey, sourceFilter]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    window.localStorage.setItem("burrow-theme", theme);
  }, [theme]);

  useEffect(() => {
    const onFullscreenChange = () =>
      setViewerFullscreen(document.fullscreenElement === viewerFrame.current);
    document.addEventListener("fullscreenchange", onFullscreenChange);
    return () => document.removeEventListener("fullscreenchange", onFullscreenChange);
  }, []);

  useEffect(() => {
    setVideoQualities([]);
    setVideoQualityUrl(null);
    setQualityError(null);
    if (
      !viewing ||
      viewing.kind !== "video" ||
      viewing.state !== "linked" ||
      sourceLabel(viewing.sourceUrl) !== "X"
    ) {
      setQualitiesLoading(false);
      return;
    }

    let cancelled = false;
    setQualitiesLoading(true);
    xVideoQualities(viewing.id)
      .then((qualities) => {
        if (cancelled) return;
        setVideoQualities(qualities);
        const initial =
          qualities.find((quality) => quality.url === viewing.remoteUrl) ?? qualities[0];
        setVideoQualityUrl(initial?.url ?? null);
      })
      .catch(() => {
        if (!cancelled) {
          setQualityError("Quality options are unavailable for this post.");
        }
      })
      .finally(() => {
        if (!cancelled) setQualitiesLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [viewing]);

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
        setAssets(await listBoardAssets(activeBoardId, VIEW_LIMIT, 0));
      } else if (searchFilter) {
        setAssets(await searchAssets(searchFilter, COLOR_TOLERANCE, VIEW_LIMIT));
      } else {
        setAssets(await listAssets(VIEW_LIMIT, 0));
      }
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [activeBoardId, searchFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    void refreshBoards();
    libraryRoot()
      .then(setRoot)
      .catch(() => {});
    if (DEMO_MODE) {
      setXState({
        connected: false,
        hasSession: false,
        detail: "Demo account ready to connect",
      });
      return;
    }
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
      if (DEMO_MODE) {
        await new Promise((resolve) => window.setTimeout(resolve, 900));
        setXState({
          connected: true,
          hasSession: true,
          detail: "@a16z-demo · mock connection",
        });
        setConnectOpen(false);
        return;
      }
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
      if (DEMO_MODE) {
        setXState({
          connected: false,
          hasSession: false,
          detail: "Demo account ready to connect",
        });
        setFolders([]);
        return;
      }
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
        setSearchFilter(null);
        setSearchQuery("");
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
        setSearchFilter(null);
        setSearchQuery("");
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
    async (ids: number[]): Promise<DownloadReport | null> => {
      if (ids.length === 0) return null;
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
        return report;
      } catch (e) {
        setError(String(e));
        return null;
      } finally {
        setDownloading(new Set());
      }
    },
    [refresh, refreshBoards],
  );

  const pickMedia = useCallback(async () => {
    if (DEMO_MODE) {
      await runImport(DEMO_IMPORT_PATHS);
      return;
    }
    try {
      const selected = await openFileDialog({
        multiple: true,
        directory: false,
        pickerMode: "media",
        fileAccessMode: "copy",
        filters: [
          {
            name: "Images and video",
            extensions: [
              "png",
              "jpg",
              "jpeg",
              "webp",
              "gif",
              "avif",
              "bmp",
              "tif",
              "tiff",
              "mp4",
              "m4v",
              "mov",
              "webm",
              "mkv",
            ],
          },
        ],
      });
      if (!selected) return;
      const paths = Array.isArray(selected) ? selected : [selected];
      await runImport(paths);
    } catch (e) {
      setError(String(e));
    }
  }, [runImport]);

  const downloadAndPlay = useCallback(
    async (asset: Asset) => {
      setPlaybackFailed(false);
      const report = await runDownload([asset.id]);
      if (!report) {
        setPlaybackFailed(true);
        return;
      }

      const local = report.downloaded.find((item) => item.id === asset.id);
      if (local) {
        setViewing(local);
        setPlaybackFailed(false);
        return;
      }

      // A failure remains visible in the download notice; keep this dialog in
      // its actionable state so the user can retry. If the bytes were already
      // held under another row, the backend merged the duplicate and the grid
      // now points at that local copy.
      if (report.deduplicated > 0) setViewing(null);
      else if (report.failed.length > 0) setPlaybackFailed(true);
    },
    [runDownload],
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

  const selectFromPointer = (event: React.MouseEvent, id: number) => {
    event.stopPropagation();
    const additive = event.ctrlKey || event.metaKey;
    const anchor = selectionAnchor.current;

    if (event.shiftKey && anchor !== null) {
      const anchorIndex = sortedAssets.findIndex((asset) => asset.id === anchor);
      const clickedIndex = sortedAssets.findIndex((asset) => asset.id === id);
      if (anchorIndex !== -1 && clickedIndex !== -1) {
        const start = Math.min(anchorIndex, clickedIndex);
        const end = Math.max(anchorIndex, clickedIndex);
        const range = sortedAssets.slice(start, end + 1).map((asset) => asset.id);
        setSelected((previous) => {
          const next = additive ? new Set(previous) : new Set<number>();
          range.forEach((assetId) => next.add(assetId));
          return next;
        });
        return;
      }
    }

    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
    selectionAnchor.current = id;
  };

  const openViewer = (event: React.MouseEvent, asset: Asset) => {
    if (event.shiftKey || event.ctrlKey || event.metaKey) {
      selectFromPointer(event, asset.id);
      return;
    }
    setPlaybackFailed(false);
    setViewing(asset);
  };

  const clearSelection = () => {
    selectionAnchor.current = null;
    setSelected(new Set());
  };

  const stepViewer = useCallback(
    (direction: -1 | 1) => {
      setViewing((current) => {
        if (!current || sortedAssets.length < 2) return current;
        const index = sortedAssets.findIndex((asset) => asset.id === current.id);
        const next = (Math.max(index, 0) + direction + sortedAssets.length) % sortedAssets.length;
        setPlaybackFailed(false);
        return sortedAssets[next];
      });
    },
    [sortedAssets],
  );

  const closeViewer = useCallback(async () => {
    if (document.fullscreenElement === viewerFrame.current) {
      await document.exitFullscreen().catch(() => {});
    }
    setViewing(null);
    setPlaybackFailed(false);
  }, []);

  const toggleViewerFullscreen = async () => {
    if (document.fullscreenElement === viewerFrame.current) {
      await document.exitFullscreen();
    } else {
      await viewerFrame.current?.requestFullscreen();
    }
  };

  const takeVideoSnapshot = useCallback(async () => {
    if (!viewing || viewing.kind !== "video") return;
    const video = viewerVideo.current;
    if (!video || !Number.isFinite(video.currentTime)) {
      setError("The video is not ready for a snapshot yet.");
      return;
    }

    setCapturingFrame(true);
    setError(null);
    try {
      const positionMs = Math.max(0, Math.round(video.currentTime * 1000));
      const result =
        viewing.state === "linked"
          ? await captureRenderedVideoFrame(
              viewing.id,
              positionMs,
              await renderedFramePng(video),
              activeBoardId,
            )
          : await captureVideoFrame(viewing.id, positionMs, activeBoardId);
      setNotice({
        imported: result.duplicate ? 0 : 1,
        duplicates: result.duplicate ? 1 : 0,
        failed: [],
        captured: result.asset.originalName ?? "Video snapshot",
      });
      await refresh();
      await refreshBoards();
    } catch (e) {
      setError(String(e));
    } finally {
      setCapturingFrame(false);
    }
  }, [activeBoardId, refresh, refreshBoards, viewing]);

  useEffect(() => {
    if (!viewing) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        stepViewer(-1);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        stepViewer(1);
      } else if (event.key === "Escape" && !document.fullscreenElement) {
        event.preventDefault();
        void closeViewer();
      } else if (
        event.shiftKey &&
        !event.ctrlKey &&
        !event.metaKey &&
        event.key.toLowerCase() === "s" &&
        viewing.kind === "video" &&
        !event.repeat
      ) {
        event.preventDefault();
        void takeVideoSnapshot();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [closeViewer, stepViewer, takeVideoSnapshot, viewing]);

  /** Selected references that still live on someone else's server. */
  const selectedLinked = useMemo(
    () =>
      assets.filter((a) => selected.has(a.id) && a.state === "linked").map((a) => a.id),
    [assets, selected],
  );

  const showBoard = (id: number | null) => {
    setActiveBoardId(id);
    setSearchFilter(null);
    setSearchQuery("");
    setShowDismissed(false);
    setMobileNavOpen(false);
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
    setMobileNavOpen(false);
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
      if (DEMO_MODE) {
        await new Promise((resolve) => window.setTimeout(resolve, 1200));
        const report = await importPaths(DEMO_X_PATHS);
        setActiveBoardId(null);
        setSearchFilter(null);
        setSearchQuery("");
        setNotice({
          imported: report.imported.length,
          duplicates: report.duplicates,
          failed: report.failed,
          syncedFrom: `${syncFolder || "All bookmarks"} · ${DEMO_X_PATHS.length} found`,
          images: report.imported.length,
          videos: 0,
          stoppedBecause: "Reached the end of this demo bookmark folder.",
          moreAvailable: false,
          scanned: `${DEMO_X_PATHS.length} posts over 1 page`,
        });
        setSyncOpen(false);
        await refresh();
        await refreshBoards();
        return;
      }
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
      setSearchFilter(null);
      setSearchQuery("");
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

  const deleteViewedAsset = async () => {
    if (!viewing) return;
    const fromSource = Boolean(viewing.remoteUrl || viewing.sourceUrl);
    const confirmed = window.confirm(
      `Permanently delete this reference?\n\n` +
        `Its stored file and thumbnail will be erased from your library, and it ` +
        `will be removed from every board.\n\nYour original file on disk is not ` +
        `touched. This cannot be undone.` +
        (fromSource
          ? `\n\nIt came from a link or from X, so it will also be kept out of ` +
            `future syncs. You can undo that under Dismissed in the sidebar.`
          : ""),
    );
    if (!confirmed) return;

    try {
      // Close first so WebView2 releases a local video file before Rust unlinks
      // it on Windows. Linked videos stop their range requests here too.
      await closeViewer();
      const report = await deleteAssets([viewing.id]);
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

  const activeColorQuery = normalizedColorQuery(searchFilter);

  const heading = useMemo(() => {
    if (showDismissed) {
      return `${dismissed.length} kept out of sync`;
    }
    if (loading) return "Loading library…";
    if (importingCount !== null) {
      return `Importing ${importingCount} file${importingCount === 1 ? "" : "s"}…`;
    }
    if (activeBoard) {
      return `${sortedAssets.length} on ${activeBoard.name}`;
    }
    if (searchFilter) {
      return activeColorQuery
        ? `${sortedAssets.length} matching ${activeColorQuery}`
        : `${sortedAssets.length} matching “${searchFilter}”`;
    }
    return `${sortedAssets.length} reference${sortedAssets.length === 1 ? "" : "s"}`;
  }, [
    loading,
    importingCount,
    activeBoard,
    activeColorQuery,
    searchFilter,
    sortedAssets.length,
    showDismissed,
    dismissed.length,
  ]);

  return (
    <div className={`app${dragging ? " app--dragging" : ""}`}>
      {mobileNavOpen && (
        <button
          type="button"
          className="railScrim"
          aria-label="Close navigation"
          onClick={() => setMobileNavOpen(false)}
        />
      )}
      <aside
        className={`rail${mobileNavOpen ? " rail--open" : ""}`}
        aria-label="Library navigation"
      >
        <div className="rail__brand">Burrow</div>
        <button
          type="button"
          className="rail__close"
          aria-label="Close navigation"
          onClick={() => setMobileNavOpen(false)}
        >
          ×
        </button>

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

        {/* Where the library actually lives. Worth a permanent line: it is the
            folder to back up, and it is not somewhere anyone would guess. */}
        {root && (
          <div className="rail__foot">
            <button
              type="button"
              className="rail__folderButton"
              onClick={async () => {
                try {
                  await openPath(await xDownloadsFolder());
                } catch (error) {
                  window.alert(`Could not open the X video folder: ${String(error)}`);
                }
              }}
            >
              Open X video files
            </button>
            <span className="rail__path" title={root}>
              {root}
            </span>
          </div>
        )}
      </aside>

      <div className="main">
        <header className="bar">
          <button
            type="button"
            className="mobileNavToggle"
            aria-label="Open navigation"
            aria-expanded={mobileNavOpen}
            onClick={() => setMobileNavOpen(true)}
          >
            ☰
          </button>
          <div className="bar__identity">
            <h1>
              {showDismissed ? "Dismissed" : activeBoard ? activeBoard.name : "Burrow"}
            </h1>
            <span className="bar__count">{heading}</span>
          </div>

          <div className="bar__actions">
            {!showDismissed && (
              <button type="button" className="bar__sync" onClick={() => void pickMedia()}>
                Add files
              </button>
            )}
            {!showDismissed && (
              <label className="bar__sourceFilter">
                <span>Show</span>
                <select
                  value={sourceFilter}
                  onChange={(event) => {
                    setSourceFilter(event.target.value as SourceFilter);
                    clearSelection();
                  }}
                  aria-label="Filter references by source"
                >
                  <option value="all">All sources</option>
                  <option value="local">My files</option>
                  <option value="x">Synced from X</option>
                  <option value="links">Other links</option>
                </select>
              </label>
            )}
            {!showDismissed && (
              <div
                className="bar__sort"
                title={
                  sortKey === "xSaved"
                    ? "X provides saved order, but not the exact date or time you bookmarked a post."
                    : "Sort the references in this view"
                }
              >
                <label>
                  <span>Sort</span>
                  <select
                    value={sortKey}
                    onChange={(event) => {
                      const next = event.target.value as SortKey;
                      setSortKey(next);
                      setSortDirection(defaultDirection(next));
                    }}
                    aria-label="Sort references by"
                  >
                    <option value="added">Date added to Burrow</option>
                    <option value="name">Name</option>
                    <option value="posted">Date posted on X</option>
                    <option value="xSaved">Saved on X (order)</option>
                    <option value="kind">Type</option>
                    <option value="size">File size</option>
                    <option value="duration">Video duration</option>
                  </select>
                </label>
                <button
                  type="button"
                  onClick={() =>
                    setSortDirection((value) => (value === "asc" ? "desc" : "asc"))
                  }
                  aria-label={`Reverse sort: currently ${directionLabel(sortKey, sortDirection)}`}
                  title="Reverse sort order"
                >
                  {directionLabel(sortKey, sortDirection)} {sortDirection === "asc" ? "↑" : "↓"}
                </button>
              </div>
            )}
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
                  (DEMO_MODE
                    ? new Promise<string[]>((resolve) =>
                        window.setTimeout(
                          () => resolve(["a16z", "Design references", "Future of media"]),
                          650,
                        ),
                      )
                    : xFolders())
                    .then(setFolders)
                    .catch((e) => setFoldersError(String(e)))
                    .finally(() => setFoldersLoading(false));
                }
              }}
              aria-expanded={syncOpen}
            >
              {syncing
                ? `Syncing ${syncBatch}…`
                : `Sync from X · ${syncDownload ? "full media" : "thumbnails"}`}
            </button>
            <button
              type="button"
              className="bar__theme"
              onClick={() => setTheme((current) => (current === "light" ? "dark" : "light"))}
              aria-label={`Switch to ${theme === "light" ? "dark" : "light"} mode`}
              title={`Switch to ${theme === "light" ? "dark" : "light"} mode`}
            >
              {theme === "light" ? "Dark" : "Light"}
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
              const query = searchQuery.trim();
              if (!query) return;
              // Search spans the whole library. A valid hex colour is routed
              // through perceptual palette matching by the same backend query.
              setActiveBoardId(null);
              setSearchFilter(query);
              clearSelection();
            }}
          >
            <input
              type="search"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search names, notes, sources, or #colour…"
              aria-label="Search references"
              spellCheck={false}
            />
            <button type="submit" disabled={!searchQuery.trim()}>Find</button>
            {searchFilter && (
              <button
                type="button"
                className="bar__clear"
                onClick={() => {
                  setSearchFilter(null);
                  setSearchQuery("");
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
              <strong>{DEMO_MODE ? "Connect X · demo" : "Connect your X account"}</strong>
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

            {DEMO_MODE ? (
              <>
                <ol className="connect__steps">
                  <li>Choose the demo account prepared for this recording.</li>
                  <li>Burrow will show the same connection and sync flow without contacting X.</li>
                </ol>
                <div className="connect__fields">
                  <label>
                    <span>Demo account</span>
                    <input value="@a16z-demo" readOnly aria-label="Demo X account" />
                  </label>
                  <button
                    type="button"
                    className="sync__go"
                    onClick={() => void connectX()}
                    disabled={xChecking}
                  >
                    {xChecking ? "Connecting…" : "Connect demo account"}
                  </button>
                </div>
                <p className="connect__note">
                  Demo mode is local-only. No X credentials are used and no request is sent.
                </p>
              </>
            ) : (
              <>
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
              </>
            )}
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

        {activeColorQuery && (
          <div className="filter">
            <span className="filter__chip" style={{ background: activeColorQuery }} />
            <span>
              within {COLOR_TOLERANCE} OkLab of <code>{activeColorQuery}</code>
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
            {notice.captured ? (
              <>
                Snapshot saved: <strong>{notice.captured}</strong>
                {notice.duplicates > 0 && <> · already in library</>}
              </>
            ) : notice.deleted !== undefined ? (
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
        ) : !loading && sortedAssets.length === 0 ? (
          <div className="empty">
            <p className="empty__headline">
              {assets.length > 0 && sourceFilter !== "all"
                ? "Nothing from this source is in this view."
                : activeBoard
                ? "This board is empty."
                : searchFilter
                  ? activeColorQuery
                    ? "Nothing matches that colour."
                    : `No references match “${searchFilter}”.`
                  : "Drop images or video here, or paste a link."}
            </p>
            <p className="empty__detail">
              {assets.length > 0 && sourceFilter !== "all"
                ? "Choose another source above, or add something new."
                : activeBoard
                ? "Go to All references, select some tiles, and add them here."
                : searchFilter
                  ? activeColorQuery
                    ? "Try a different hue — the tolerance is deliberately tight."
                    : "Search checks names, notes, sources, file types, and other reference details. Try a shorter fragment."
                  : "Drag files or folders from Explorer, or paste a YouTube, Spotify, Apple Music, or media link. Linked references store a thumbnail and open back to their source."}
            </p>
            {!activeBoard && !searchFilter && (
              <button type="button" className="empty__picker" onClick={() => void pickMedia()}>
                {DEMO_MODE ? "Choose demo files…" : "Choose images or video…"}
              </button>
            )}
            {root && !searchFilter && !activeBoard && (
              <code className="empty__path">{root}</code>
            )}
          </div>
        ) : (
          <main className="grid">
            {sortedAssets.map((asset, assetIndex) => {
              const isSelected = selected.has(asset.id);
              const sourceUrl = asset.sourceUrl;
              const source = sourceLabel(sourceUrl);
              const isMusic = isMusicSource(sourceUrl);
              return (
                <figure
                  className={`tile${isSelected ? " tile--selected" : ""}`}
                  key={asset.id}
                >
                  <div className="tile__media">
                    {asset.kind === "image" ? (
                      <button
                        type="button"
                        className="tile__preview"
                        onClick={(event) => openViewer(event, asset)}
                        aria-label={`View ${asset.originalName ?? "image"} larger`}
                        title="Click to view · Ctrl-click to select · Shift-click for a range"
                      >
                        <img
                          src={thumbUrl(asset)}
                          alt={asset.originalName ?? asset.hash}
                          loading="lazy"
                          width={asset.width}
                          height={asset.height}
                        />
                      </button>
                    ) : (
                      <img
                        src={thumbUrl(asset)}
                        alt={asset.originalName ?? asset.hash}
                        loading="lazy"
                        width={asset.width}
                        height={asset.height}
                      />
                    )}
                    {asset.kind === "video" && (
                      <button
                        type="button"
                        className="tile__play"
                        onClick={(event) => openViewer(event, asset)}
                        aria-label={`Play ${asset.originalName ?? "video"}`}
                        title="Click to play · Ctrl-click to select · Shift-click for a range"
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
                            : isMusic
                              ? "Save this reference's cover artwork to the library."
                              : source === "X" && asset.kind === "video"
                                ? "Thumbnail only — download the X video to play it."
                                : "Linked — click to save a local copy."
                        }
                        aria-label={`${isMusic ? "Save artwork for" : "Download"} ${asset.originalName ?? "reference"}`}
                      >
                        {/* A corner flag rather than an icon: whether the bytes
                            are here changes what pressing play will do, so it
                            says so in words. */}
                        {downloading.has(asset.id)
                          ? "Saving…"
                          : isMusic
                            ? "Artwork ↓"
                            : "Linked ↓"}
                      </button>
                    )}
                    {/* Above .tile__play, which covers the whole media box. */}
                    <button
                      type="button"
                      className={`tile__select${isSelected ? " tile__select--on" : ""}`}
                      onClick={(event) => selectFromPointer(event, asset.id)}
                      aria-pressed={isSelected}
                      aria-label={isSelected ? "Deselect" : "Select"}
                      title="Select · Shift-click for a range"
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
                    <div className="tile__nameRow">
                      <span className="tile__name">
                        {asset.originalName ?? asset.hash.slice(0, 12)}
                      </span>
                      {source && sourceUrl && (
                        <button
                          type="button"
                          className="tile__source"
                          onClick={() => void openUrl(sourceUrl)}
                          title={`Open in ${source}`}
                        >
                          {source} ↗
                        </button>
                      )}
                    </div>
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
                    {asset.postedAt && (
                      <span className="tile__date">
                        Posted {formatSourceDate(asset.postedAt)}
                      </span>
                    )}
                    {sortKey === "added" && (
                      <span className="tile__sortValue">
                        Added {formatImportedDate(asset.importedAt)}
                      </span>
                    )}
                    {sortKey === "xSaved" && asset.xBookmarkSortIndex && (
                      <span className="tile__sortValue">
                        Saved on X · {assetIndex + 1} in this view
                      </span>
                    )}
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
                          setSearchFilter(s.hex);
                          setSearchQuery(s.hex);
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

      {viewing && (
        <div
          className="player"
          role="dialog"
          aria-modal="true"
          aria-label={viewing.originalName ?? "Reference viewer"}
          onClick={() => void closeViewer()}
        >
          <div
            className="player__frame"
            ref={viewerFrame}
            onClick={(event) => event.stopPropagation()}
          >
            {sortedAssets.length > 1 && (
              <button
                type="button"
                className="player__nav player__nav--previous"
                onClick={() => stepViewer(-1)}
                aria-label="Previous reference"
                title="Previous (Left arrow)"
              >
                ‹
              </button>
            )}
            {viewing.kind === "image" ? (
              <div className="player__imageStage">
                <img
                  className="player__image"
                  src={viewing.state === "local" ? blobUrl(viewing) : thumbUrl(viewing)}
                  alt={viewing.originalName ?? "Reference"}
                />
              </div>
            ) : isPlayableInline(viewing) &&
            !playbackFailed &&
            playbackUrl(viewing, videoQualityUrl) ? (
              <video
                // X links use Burrow's range-aware protocol; other links stream
                // from their host and local videos play off disk.
                crossOrigin={viewing.state === "linked" ? "anonymous" : undefined}
                src={playbackUrl(viewing, videoQualityUrl) ?? undefined}
                ref={viewerVideo}
                poster={thumbUrl(viewing)}
                controls
                autoPlay
                onError={() => setPlaybackFailed(true)}
              />
            ) : (
              <div className="player__fallback">
                {viewing.state === "linked" && (
                  <img
                    className="player__poster"
                    src={thumbUrl(viewing)}
                    alt=""
                    aria-hidden="true"
                  />
                )}
                <p>
                  {viewing.state === "linked" && playbackFailed
                      ? "Burrow couldn't stream this video from its source. The link may have expired."
                    : `This one won't play in the app${
                        playbackFailed ? " — the codec isn't supported here." : "."
                      }`}
                </p>
                <p className="player__fallbackDetail">
                  {viewing.mime} · {viewing.ext.toUpperCase()}
                </p>
                {viewing.state === "linked" ? (
                  <button
                    type="button"
                    disabled={downloading.has(viewing.id)}
                    onClick={() => void downloadAndPlay(viewing)}
                  >
                    {downloading.has(viewing.id) ? "Downloading…" : "Download & play"}
                  </button>
                ) : (
                  <button type="button" onClick={() => void openPath(viewing.blobPath)}>
                    Open in default player
                  </button>
                )}
              </div>
            )}
            {sortedAssets.length > 1 && (
              <button
                type="button"
                className="player__nav player__nav--next"
                onClick={() => stepViewer(1)}
                aria-label="Next reference"
                title="Next (Right arrow)"
              >
                ›
              </button>
            )}
            <div className="player__meta">
              <span>{viewing.originalName ?? viewing.hash.slice(0, 12)}</span>
              <small>
                {Math.max(
                  sortedAssets.findIndex((asset) => asset.id === viewing.id) + 1,
                  1,
                )}{" "}
                / {sortedAssets.length}
              </small>
              {viewing.kind === "image" && viewing.state === "linked" && (
                <button
                  type="button"
                  className="player__downloadOriginal"
                  disabled={downloading.has(viewing.id)}
                  onClick={() => void downloadAndPlay(viewing)}
                  title="Save the original image locally and show it at full resolution"
                >
                  {downloading.has(viewing.id) ? "Downloading…" : "Get original"}
                </button>
              )}
              {viewing.kind === "video" &&
                viewing.state === "linked" &&
                sourceLabel(viewing.sourceUrl) === "X" && (
                  <select
                    className="player__quality"
                    value={videoQualityUrl ?? ""}
                    disabled={qualitiesLoading || videoQualities.length === 0}
                    onChange={(event) => {
                      setPlaybackFailed(false);
                      setVideoQualityUrl(event.target.value || null);
                    }}
                    aria-label="Streaming quality"
                    title={
                      qualityError ??
                      (qualitiesLoading
                        ? "Finding available X video qualities…"
                        : "Change streaming quality")
                    }
                  >
                    {qualitiesLoading ? (
                      <option value="">Quality…</option>
                    ) : videoQualities.length === 0 ? (
                      <option value="">Auto quality</option>
                    ) : (
                      videoQualities.map((quality, index) => (
                        <option value={quality.url} key={quality.url}>
                          {quality.label}{index === 0 ? " · Best" : ""}
                        </option>
                      ))
                    )}
                  </select>
                )}
              {viewing.kind === "video" && (
                <button
                  type="button"
                  className="player__snapshot"
                  disabled={
                    !isPlayableInline(viewing) ||
                    playbackFailed ||
                    capturingFrame
                  }
                  onClick={() => void takeVideoSnapshot()}
                  aria-keyshortcuts="Shift+S"
                  title="Save the current video frame (Shift + S)"
                >
                  {capturingFrame ? "Saving…" : "Snapshot · Shift+S"}
                </button>
              )}
              <button
                type="button"
                className="player__fullscreen"
                onClick={() => void toggleViewerFullscreen()}
                aria-label={viewerFullscreen ? "Exit full screen" : "View full screen"}
                title={viewerFullscreen ? "Exit full screen" : "View full screen"}
              >
                {viewerFullscreen ? "Exit full screen" : "Full screen"}
              </button>
              <button
                type="button"
                className="player__delete"
                onClick={() => void deleteViewedAsset()}
                title="Permanently delete this reference"
              >
                Delete
              </button>
              <button
                type="button"
                onClick={() => void closeViewer()}
                aria-label="Close"
                title="Close (Esc)"
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
