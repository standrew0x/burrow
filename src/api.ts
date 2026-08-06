import { convertFileSrc, invoke } from "@tauri-apps/api/core";

import type {
  Asset,
  Board,
  ColorMatch,
  DeleteReport,
  DownloadReport,
  ImportReport,
  SyncOptions,
  SyncReport,
  XStatus,
} from "./types";

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
 * Where to play this reference from.
 *
 * A linked reference streams from the host that holds it; a local one plays off
 * disk. The remote case is why `media-src` in tauri.conf.json carries `https:`
 * — a remote video cannot play without its origin being allowed, and the set of
 * origins is whatever the user pastes or bookmarks, so it cannot be listed
 * ahead of time.
 *
 * `img-src` is deliberately NOT widened to match. Thumbnails are fetched once
 * at import and cached locally, so browsing the grid contacts nobody; only
 * pressing play on a linked video reveals an IP address to its host.
 */
export const playbackUrl = (asset: Asset) =>
  asset.state === "linked" ? asset.remoteUrl : blobUrl(asset);

/**
 * WebView2 plays mp4 and webm; everything else is stored and openable but will
 * not render in a `<video>` element. Container is necessary but not sufficient
 * — an HEVC or AV1 mp4 still fails, which the player falls back from.
 */
export const isPlayableInline = (asset: Asset) =>
  asset.mime === "video/mp4" || asset.mime === "video/webm";

// --- boards ---

export const listBoards = () => invoke<Board[]>("list_boards");

/** Get-or-create: an existing name (case-insensitive) returns that board. */
export const createBoard = (name: string) => invoke<Board>("create_board", { name });

export const renameBoard = (id: number, name: string) =>
  invoke<Board>("rename_board", { id, name });

/** Deletes the board only — never the references on it. */
export const deleteBoard = (id: number) => invoke<void>("delete_board", { id });

export const addToBoard = (boardId: number, assetIds: number[]) =>
  invoke<number>("add_to_board", { boardId, assetIds });

export const removeFromBoard = (boardId: number, assetIds: number[]) =>
  invoke<number>("remove_from_board", { boardId, assetIds });

export const listBoardAssets = (boardId: number, limit?: number, offset?: number) =>
  invoke<Asset[]>("list_board_assets", { boardId, limit, offset });

export const moveToBoard = (fromBoard: number, toBoard: number, assetIds: number[]) =>
  invoke<number>("move_to_board", { fromBoard, toBoard, assetIds });

/**
 * Writes or clears a reference's note. Blank input clears it; the stored value
 * comes back, so the caller never has to guess what normalisation did.
 */
export const setNote = (assetId: number, note: string) =>
  invoke<string | null>("set_note", { assetId, note });

/** References whose note contains `query`, case-insensitively. */
export const searchNotes = (query: string, limit?: number) =>
  invoke<Asset[]>("search_notes", { query, limit });

/** Permanent: removes the rows and unlinks the stored files. */
export const deleteAssets = (assetIds: number[]) =>
  invoke<DeleteReport>("delete_assets", { assetIds });

/**
 * Adds references from pasted URLs. Only a preview image is fetched; the media
 * stays on its host until downloaded.
 */
export const addLinks = (urls: string[]) =>
  invoke<ImportReport>("add_links", { urls });

/** Fetches the media behind linked references, making them local. */
export const downloadAssets = (assetIds: number[]) =>
  invoke<DownloadReport>("download_assets", { assetIds });

/** Pulls images and videos from X bookmarks. */
export const syncFromX = (opts: SyncOptions) =>
  // Spread into a plain record: invoke wants an index signature, which a named
  // interface does not satisfy.
  invoke<SyncReport>("sync_from_x", { ...opts });

/** Bookmark folder names, so the UI can offer them rather than hardcode one. */
export const xFolders = () => invoke<string[]>("x_folders");

export const xStatus = () => invoke<XStatus>("x_status");

/** Stores the cookies and immediately verifies them against X. */
export const saveXSession = (authToken: string, ct0: string) =>
  invoke<XStatus>("save_x_session", { authToken, ct0 });

export const clearXSession = () => invoke<XStatus>("clear_x_session");
