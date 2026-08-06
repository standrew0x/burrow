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
 * `linked` means only a thumbnail was stored; the media plays by streaming from
 * `remoteUrl` and can be downloaded later.
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

/** `[assetId, done, total]` — payload of the `download-progress` event. */
export type DownloadProgress = [number, number, number];

export interface FailedImport {
  path: string;
  reason: string;
}

export interface ImportReport {
  imported: Asset[];
  duplicates: number;
  failed: FailedImport[];
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
  /** Rows removed whose files could not be unlinked — wasted disk, not a broken tile. */
  orphanedFiles: string[];
}

export type SyncKinds = "all" | "images" | "videos";

export interface SyncOptions {
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
  images: number;
  videos: number;
  failed: FailedImport[];
}

export interface XStatus {
  connected: boolean;
  hasSession: boolean;
  /** Why, when not connected. */
  detail: string;
}
