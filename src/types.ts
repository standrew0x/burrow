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

export interface Asset {
  id: number;
  hash: string;
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
