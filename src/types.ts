/** Mirrors the Rust structs in src-tauri/src/ingest.rs and color.rs.
 *  Those derive `#[serde(rename_all = "camelCase")]`, so keys are camelCase. */

export interface Swatch {
  /** OkLab lightness, 0..1 */
  l: number;
  a: number;
  b: number;
  /** `#rrggbb` */
  hex: string;
  /** Share of sampled pixels, 0..1 */
  weight: number;
}

export type MediaKind = "image" | "video";

/**
 * Whether the library holds this reference's bytes.
 *
 * `linked` means only a thumbnail was stored. Burrow proxies X videos in small
 * byte ranges so they can play and seek without first saving the whole file.
 */
export type AssetState = "local" | "linked";

export interface Asset {
  id: number;
  hash: string;
  kind: MediaKind;
  state: AssetState;
  /** Milliseconds. Null for images, and for links nothing has read yet. */
  durationMs: number | null;
  ext: string;
  mime: string;
  width: number;
  height: number;
  /** Size on disk — zero while linked. */
  bytes: number;
  originalName: string | null;
  /** The page this came from, for "open original". */
  sourceUrl: string | null;
  /** The media file on the remote host; present only while linked. */
  remoteUrl: string | null;
  /** The user's own note. Null when unset — never an empty string. */
  note: string | null;
  /** Source creation time; for X this is a UTC ISO timestamp. */
  postedAt: string | null;
  /** Opaque X timeline position. Sortable as saved order, not an exact date. */
  xBookmarkSortIndex: string | null;
  importedAt: number;
  swatches: Swatch[];
  /** Absolute path; run through convertFileSrc before use in an <img>. */
  thumbPath: string;
  /** Absolute path to the stored original. Meaningless while linked. */
  blobPath: string;
}

export interface DownloadReport {
  downloaded: Asset[];
  /** Links whose bytes were already in the library; the row was merged away. */
  deduplicated: number;
  bytesWritten: number;
  failed: FailedImport[];
}

/** A still decoded from a video and saved back into the Burrow library. */
export interface VideoSnapshot {
  asset: Asset;
  duplicate: boolean;
  capturedAtMs: number;
}

/** One of X's actual MP4 encodes for a linked video. */
export interface XVideoQuality {
  label: string;
  bitrate: number;
  width: number | null;
  height: number | null;
  url: string;
}

/** `[assetId, done, total]` — payload of the `download-progress` event. */
export type DownloadProgress = [number, number, number];

export interface FailedImport {
  path: string;
  reason: string;
}

export interface ImportReport {
  imported: Asset[];
  duplicates: number;
  /** Skipped because they had been deleted from the library before. */
  dismissed: number;
  failed: FailedImport[];
}

/** A reference deleted on purpose, kept out of future syncs. */
export interface Dismissed {
  remoteUrl: string;
  pageUrl: string | null;
  title: string | null;
  dismissedAt: number;
}

export interface ColorMatch {
  asset: Asset;
  /** OkLab distance from the query colour to the closest swatch. */
  distance: number;
}

export interface Board {
  id: number;
  name: string;
  createdAt: number;
  itemCount: number;
  /** Thumbnail of the newest item; null for an empty board. */
  coverThumbPath: string | null;
}

export interface DeleteReport {
  deleted: number;
  bytesFreed: number;
  /** How many were remembered so a later sync will not offer them again. */
  dismissed: number;
  /** Rows removed whose files could not be unlinked — wasted disk, not a broken tile. */
  orphanedFiles: string[];
}

export type SyncKinds = "all" | "images" | "videos";

export interface SyncOptions {
  /** How many media items to take. 0 means "as many as there are". */
  limit?: number;
  /** Omit for every bookmark; set to sync one folder. */
  folder?: string;
  /** Inclusive YYYY-MM-DD bounds. */
  from?: string;
  to?: string;
  kinds?: SyncKinds;
  /**
   * Pull the actual media rather than just posters. Off by default: a poster
   * measured 16KB against a 171MB video, so downloading a whole timeline costs
   * gigabytes to show pictures the grid already has.
   */
  download?: boolean;
}

export interface SyncReport {
  /** "all bookmarks" or the folder name. */
  source: string;
  /** Media items the window offered, before download. */
  found: number;
  downloaded: number;
  imported: number;
  duplicates: number;
  /** Skipped because they were deleted from the library before. */
  dismissed: number;
  images: number;
  videos: number;
  /**
   * Why the walk ended, in a sentence.
   *
   * A count alone cannot distinguish "that is all your bookmarks hold" from
   * "there is more, ask for more", and mistaking the second for the first is
   * what makes a working sync feel like it is losing things.
   */
  stoppedBecause: string;
  /** Whether a larger number would return more. */
  moreAvailable: boolean;
  pages: number;
  /** Posts examined, including ones carrying no media. */
  postsScanned: number;
  failed: FailedImport[];
}

export interface XStatus {
  connected: boolean;
  hasSession: boolean;
  /** Why, when not connected. */
  detail: string;
}
