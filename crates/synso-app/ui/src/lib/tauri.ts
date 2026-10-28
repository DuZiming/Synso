// Typed wrappers around the Tauri IPC commands (see
// crates/synso-app/src/commands.rs).

import { invoke } from "@tauri-apps/api/core";
import type {
  EngineStats,
  ParsedQuery,
  QuickInfo,
  SearchResult,
  Settings,
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
