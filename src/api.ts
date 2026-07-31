import { convertFileSrc, invoke } from "@tauri-apps/api/core";

import type { Asset, ColorMatch, ImportReport } from "./types";

export const listAssets = (limit?: number, offset?: number) =>
  invoke<Asset[]>("list_assets", { limit, offset });

export const importPaths = (paths: string[]) =>
  invoke<ImportReport>("import_paths", { paths });

export const searchByColor = (hex: string, tolerance?: number, limit?: number) =>
  invoke<ColorMatch[]>("search_by_color", { hex, tolerance, limit });

export const libraryRoot = () => invoke<string>("library_root");

/**
 * Turns an absolute thumbnail path into a URL the webview can load.
 *
 * On Windows this becomes `http://asset.localhost/<encoded path>`. It only
 * resolves if `app.security.assetProtocol` is enabled and the path falls inside
 * its configured scope -- otherwise the request is refused and the tile renders
 * as a broken image with nothing in the console to explain it.
 */
export const thumbUrl = (asset: Asset) => convertFileSrc(asset.thumbPath);

/** Playable URL for a stored video original. Same scope caveat as thumbUrl. */
export const blobUrl = (asset: Asset) => convertFileSrc(asset.blobPath);

/**
 * WebView2 plays mp4 and webm; everything else is stored and openable but will
 * not render in a `<video>` element. Container is necessary but not sufficient
 * — an HEVC or AV1 mp4 still fails, which the player falls back from.
 */
export const isPlayableInline = (asset: Asset) =>
  asset.mime === "video/mp4" || asset.mime === "video/webm";
