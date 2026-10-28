// Typed wrappers around the Tauri IPC commands (see
// crates/synso-app/src/commands.rs).

import { invoke } from "@tauri-apps/api/core";
import type { EngineStats, Settings, VolumeInfo } from "./types";

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
