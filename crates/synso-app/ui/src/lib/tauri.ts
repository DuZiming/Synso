// Typed wrappers around the Tauri IPC commands (see
// crates/synso-app/src/commands.rs).

import { invoke } from "@tauri-apps/api/core";
import type {
  DedupState,
  DupeGroup,
  EngineStats,
  ParsedQuery,
  QuickInfo,
  SearchResult,
  Settings,
  SizeNode,
  VolumeInfo,
} from "./types";

export function engineStats(): Promise<EngineStats> {
  return invoke("engine_stats");
}

export function startFullScan(): Promise<void> {
  return invoke("start_full_scan");
}

export function settingsGet(): Promise<Settings> {
  return invoke("settings_get");
}

export function settingsSet(settings: Settings): Promise<void> {
  return invoke("settings_set", { settings });
}

export function listVolumes(): Promise<VolumeInfo[]> {
  return invoke("list_volumes");
}

// ------------------ Task 15: quick search window -------------------------

/** Everything-style search: page `[offset, offset + limit)` of the ranked
 * matches. Modifiers are parsed engine-side; a cold index returns an empty
 * page (check `engineStats().warm` for the "warming up" hint). */
export function search(
  q: string,
  offset: number,
  limit: number,
): Promise<SearchResult> {
  return invoke("search", { q, offset, limit });
}

/** The engine's own parse of the query, for the quick window's chips. */
export function parsePreview(q: string): Promise<ParsedQuery> {
  return invoke("parse_preview", { q });
}

/** Opens the entry behind `id` with its system default app. */
export function openPath(id: number): Promise<void> {
  return invoke("open_path", { id });
}

/** Reveals the entry behind `id` in the OS file manager. */
export function revealPath(id: number): Promise<void> {
  return invoke("reveal_path", { id });
}

/** Shows + focuses the quick window (the dashboard's fallback button). */
export function quickShow(): Promise<void> {
  return invoke("quick_show");
}

/** Hides the quick window (the frontend's Esc handler). */
export function quickHide(): Promise<void> {
  return invoke("quick_hide");
}

/** Global-shortcut registration info (the dashboard's fallback toast). */
export function quickInfo(): Promise<QuickInfo> {
  return invoke("quick_info");
}

// ------------------ Task 16: dedup dashboard -----------------------------

/** Starts a dedup run (a no-op while one is already going). The real
 * "boost" is calling this NOW instead of waiting for the engine's 30s
 * post-scan auto-start; persisted hashes speed up later runs. */
export function dedupStart(): Promise<void> {
  return invoke("dedup_start");
}

/** Snapshot of the dedup state machine (phase + progress + group count). */
export function dedupState(): Promise<DedupState> {
  return invoke("dedup_state");
}

/** One page `[offset, offset + limit)` of the last run's dupe groups,
 * size-descending. Deleted files drop out; groups below 2 members
 * dissolve. */
export function dedupGroups(
  offset: number,
  limit: number,
): Promise<DupeGroup[]> {
  return invoke("dedup_groups", { offset, limit });
}

/** Moves the entries behind `ids` to the recycle bin (never a permanent
 * delete); resolves with how many paths actually landed. */
export function deleteToTrash(ids: number[]): Promise<number> {
  return invoke("delete_to_trash", { ids });
}

// ------------------ Task 17: flame chart ---------------------------------

/** Directory-size tree behind `root` (an index path in the Hit::path
 * spelling; "" is the virtual root over the scan roots). Resolves `null`
 * for an unknown root — including the synthetic "(other)" nodes, which
 * name no real directory — and rejects with the engine error string if
 * the IPC fails (e.g. a poisoned index). Async server-side (the full
 * index aggregation must not run on the main thread). The flame view
 * calls this with (path, 2, 40) and drills by re-rooting at a clicked
 * node's path. */
export function sizeTree(
  root: string,
  depth: number,
  maxChildren: number,
): Promise<SizeNode | null> {
  return invoke("size_tree", { root, depth, maxChildren });
}

/** Reveals an INDEX path in the OS file manager (Explorer with the item
 * selected) — the path-based sibling of `revealPath`, since flame-chart
 * nodes carry paths rather than row ids. Errors (synthetic or unknown
 * paths) surface verbatim for the view to toast. */
export function revealIndexPath(path: string): Promise<void> {
  return invoke("reveal_index_path", { path });
}
