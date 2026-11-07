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
  // v1.2 memory policy (live-appliable; defaults in the engine).
  mem_hot_budget_mb: number;
  mem_staging_budget_mb: number;
  mem_staging_ttl_secs: number;
  mem_promote_hits: number;
  mem_promote_window_min: number;
  mem_hot_threshold: number;
  mem_decay_half_life_h: number;
  mem_preload_on_start: boolean;
  // v1.2 dedup gear (Task 7): how aggressively dedup runs. Applies to the
  // NEXT analysis run (the engine reads it when a run starts).
  dedup_gear: "gentle" | "standard" | "turbo";
}

/// Mirror of `synso_core::TierStats` (the settings dashboard's gauges).
export interface TierStats {
  hot_bytes: number;
  hot_entries: number;
  hot_hits: number;
  hot_misses: number;
  staging_bytes: number;
  staging_entries: number;
  staging_hits: number;
  evictions: number;
  promotions: number;
}

/// Mirror of `synso_core::FileDetail` (`file_detail`'s payload): the
/// pilot dimension — per-file data the search core does not hold.
export interface FileDetail {
  id: number;
  ctime: number | null;
  atime: number | null;
  hash_first: number | null;
  hash_full: number | null;
}

/// Mirror of `synso_core::VolumeInfo`.
export interface VolumeInfo {
  path: string;
  label: string;
  total_bytes: number;
  available_bytes: number;
}

/// Mirror of `synso_core::VolumeOverlay` — the physical-capacity figures
/// the capacity map overlays at a pane root that names a whole volume.
export interface VolumeOverlay {
  used_bytes: number;
  total_bytes: number;
  available_bytes: number;
  indexed_bytes: number;
  unindexed_bytes: number;
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
  phase:
    | "idle"
    | "grouping"
    | "hashing_first"
    | "sampling"
    | "hashing_full"
    | "done"
    | "cancelled";
  done: number;
  total: number;
}

/// Payload of the `dedup-finished` event.
export interface DedupFinishedPayload {
  groups: number;
}

/// Payload of the `nav-tab` event (the tray's 设置 item switches the
/// dashboard's tab — see crates/synso-app/src/tray.rs).
export interface NavTabPayload {
  tab: string;
}

/// Mirror of `synso_core::Hit` (one search-result row).
export interface Hit {
  id: number;
  path: string;
  name: string;
  size: number;
  mtime: number;
  ext: string;
  is_dir: boolean;
}

/// Mirror of `synso_core::SearchResult` (`search`'s payload): page of hits
/// plus the full match count.
export interface SearchResult {
  hits: Hit[];
  total: number;
}

/// Mirror of `synso_core::ParsedQuery` (`parse_preview`'s payload): the
/// modifiers the engine recognized, for the quick window's chips. `null`
/// fields are absent modifiers.
export interface ParsedQuery {
  text: string;
  ext: string | null;
  min_size: number | null;
  max_size: number | null;
  path_prefix: string | null;
}

/// Payload of `quick_info`: the global shortcut's spelling and whether its
/// startup registration succeeded (the dashboard toasts when it did not).
export interface QuickInfo {
  shortcut: string;
  registered: boolean;
  error: string | null;
}

/// Mirror of `synso_core::DedupPhase` (serializes snake_case over IPC).
export type DedupPhase =
  | "idle"
  | "grouping"
  | "hashing_first"
  | "sampling"
  | "hashing_full"
  | "done"
  | "cancelled";

/// Mirror of `synso_core::DedupState` (`dedup_state`'s payload): phase of
/// the state machine, `done`/`total` of the CURRENT stage (at `done` they
/// equal this run's hash computations), the last completed run's group
/// count and the files skipped unreadable / as cloud placeholders.
/// `alive_groups`/`wasted_bytes` are the run-completion aggregates (a
/// cancelled run keeps the previous run's values; 0/0 before the first
/// completed run): the groups that still materialize (>= 2 live members)
/// and the bytes deleting one redundant copy of each would reclaim.
export interface DedupState {
  phase: DedupPhase;
  done: number;
  total: number;
  groups: number;
  errors: number;
  alive_groups: number;
  wasted_bytes: number;
}

/// Mirror of `synso_core::DupeGroup` (`dedup_groups`' payload): one
/// content-identical group, size-descending page order, members stable by
/// files-row id. `hash` is a u64 identity — never treated as a JS number.
export interface DupeGroup {
  hash: number;
  size: number;
  members: Hit[];
}

/// Mirror of `synso_core::DedupPage` (`dedup_groups`' payload): the
/// page's materialized groups plus `total_alive`, the count of groups
/// that still materialize (>= 2 live members) across ALL pages, scanned
/// against the CURRENT hot index — the full alive-filtered total a paged
/// frontend needs without paging to the end.
export interface DedupPage {
  groups: DupeGroup[];
  total_alive: number;
}

/// Mirror of `synso_core::SizeNode` (`size_tree`'s payload): one node of
/// the directory-size tree. `size` sums ALL descendant file bytes (a file
/// node: its own size), `file_count` counts descendant files. Paths are
/// index-relative in the `Hit::path` spelling (`""` for the virtual
/// "(all)" root); children are size-descending, truncated per level with
/// the overflow merged into a trailing synthetic "(other)" node.
export interface SizeNode {
  path: string;
  name: string;
  size: number;
  file_count: number;
  children: SizeNode[];
}
