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

export interface Asset {
  id: number;
  hash: string;
  kind: MediaKind;
  /** Milliseconds; null for images. */
  durationMs: number | null;
  ext: string;
  mime: string;
  width: number;
  height: number;
  bytes: number;
  originalName: string | null;
  sourceUrl: string | null;
  importedAt: number;
  swatches: Swatch[];
  /** Absolute path; run through convertFileSrc before use in an <img>. */
  thumbPath: string;
  /** Absolute path to the stored original, for video playback. */
  blobPath: string;
}

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
