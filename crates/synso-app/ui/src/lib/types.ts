// IPC payload types — 1:1 mirrors of the synso-core facade types and the
// event payloads documented in crates/synso-app/src/events.rs. All IPC
// JSON uses snake_case keys (the Rust field spellings).

/// Mirror of `synso_core::EngineStats`.
export interface EngineStats {
  files: number;
  dirs: number;
  warm: boolean;
  interned_bytes: number;
}

/// Mirror of `synso_core::Settings`.
export interface Settings {
  lang: string; // "auto" | "zh" | "en"
  autostart: boolean;
  exclusions: string[];
  volumes: string[];
  shortcut: string;
  first_run_done: boolean;
}

/// Mirror of `synso_core::VolumeInfo`.
export interface VolumeInfo {
  path: string;
  label: string;
  total_bytes: number;
}

/// Payload of the `scan-progress` event.
export interface ScanProgressPayload {
  done: number;
  total: number;
}

/// Payload of the `scan-finished` event.
export interface ScanFinishedPayload {
  files: number;
  dirs: number;
}

/// Payload of the `scan-failed` event.
export interface ScanFailedPayload {
  errors: number;
  message: string;
}

/// Payload of the `dedup-progress` event.
export interface DedupProgressPayload {
  phase: "idle" | "grouping" | "hashing_first" | "hashing_full" | "done";
  done: number;
  total: number;
}

/// Payload of the `dedup-finished` event.
export interface DedupFinishedPayload {
  groups: number;
}
