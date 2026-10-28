//! The IPC surface the webview calls. Thin wrappers over the engine
//! facade — no business logic lives here.

use std::path::PathBuf;

use synso_core::{
    DedupState, DupeGroup, EngineStats, Hit, ParsedQuery, SearchResult, Settings, SizeNode,
    VolumeInfo,
};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::quick;
use crate::state::AppState;

/// Current index snapshot (files / dirs / warm / interned bytes) for the
/// dashboard stats tile.
#[tauri::command]
pub fn engine_stats(state: State<'_, AppState>) -> EngineStats {
    state.engine.stats()
}

/// Starts a full scan on the engine's background thread. One scan at a
/// time: a call while a scan runs is a no-op. Progress arrives as the
/// `scan-progress` / `scan-finished` / `scan-failed` events (see
/// `events.rs`).
#[tauri::command]
pub fn start_full_scan(state: State<'_, AppState>) {
    state.engine.start_full_scan();
}

/// The persisted user settings.
#[tauri::command]
pub fn settings_get(state: State<'_, AppState>) -> Settings {
    state.engine.settings()
}

/// Persists the user settings; errors surface to the webview as a string.
#[tauri::command]
pub fn settings_set(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    state
        .engine
        .set_settings(&settings)
        .map_err(|err| err.to_string())
}

/// The fixed local volumes, for the first-run wizard's volume picker.
#[tauri::command]
pub fn list_volumes() -> Vec<VolumeInfo> {
    synso_core::list_volumes()
}

// ---------------------- Task 15: quick search window ----------------------

/// Everything-style search: page `[offset, offset + limit)` of the ranked
/// matches. Query modifiers (`ext:` / `size:` / `path:` / `coll:`) are
/// parsed inside the engine, exactly as the dashboard path would. On a
/// cold index the hot side is empty, so this returns an empty page — the
/// quick window says "warming up" via `engine_stats`'s `warm` flag then.
#[tauri::command]
pub fn search(state: State<'_, AppState>, q: String, offset: u32, limit: u32) -> SearchResult {
    state.engine.search(&q, offset, limit)
}

/// The engine's own parse of the query, for the quick window's modifier
/// chips — the SAME parser the search path runs, so the chips can never
/// disagree with what was actually searched.
#[tauri::command]
pub fn parse_preview(q: String) -> ParsedQuery {
    synso_core::parse_query(&q)
}

/// id → live hit → absolute disk path, the resolution shared by
/// `open_path` / `reveal_path`. A dead or unknown id (deleted since the
/// last scan, or garbage from the webview) and a path whose root segment
/// matches no scan root are errors the webview surfaces verbatim.
fn absolute_path_of(state: &AppState, id: i64) -> Result<PathBuf, String> {
    let Hit { path, .. } = state
        .engine
        .hit_by_id(id)
        .ok_or_else(|| format!("not in the index (deleted or unknown id {id})"))?;
    state
        .engine
        .resolve_absolute(&path)
        .ok_or_else(|| format!("no scan root resolves \"{path}\""))
}

/// Opens the entry behind `id` with its system default application.
#[tauri::command]
pub fn open_path(state: State<'_, AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    let abs = absolute_path_of(&state, id)?;
    app.opener()
        .open_path(abs.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|err| err.to_string())
}

/// Reveals the entry behind `id` in the OS file manager (Windows: Explorer
/// with the item selected) — via `tauri-plugin-opener`, never a hand-built
/// shell-out.
#[tauri::command]
pub fn reveal_path(state: State<'_, AppState>, app: AppHandle, id: i64) -> Result<(), String> {
    let abs = absolute_path_of(&state, id)?;
    app.opener()
        .reveal_item_in_dir(&abs)
        .map_err(|err| err.to_string())
}

/// Shows + focuses the quick window: the dashboard's fallback button (for
/// when the global shortcut is taken) and the manual entry point.
#[tauri::command]
pub fn quick_show(app: AppHandle) {
    quick::show(&app);
}

/// Hides the quick window — the frontend's Esc handler.
#[tauri::command]
pub fn quick_hide(app: AppHandle) {
    quick::hide(&app);
}

/// Global-shortcut registration info, for the dashboard's fallback toast.
#[tauri::command]
pub fn quick_info(state: State<'_, quick::QuickState>) -> quick::QuickInfo {
    state.info()
}

// ---------------------- Task 16: dedup dashboard ----------------------

/// Starts a dedup run (or, while one is already going, is a harmless
/// no-op — the engine keeps exactly one run at a time). Progress arrives
/// as the `dedup-progress` / `dedup-finished` events (see `events.rs`).
/// The view's real "boost" semantic is calling this NOW instead of
/// waiting for the engine's 30s post-scan auto-start; persisted hashes
/// make every later run skip I/O on already-hashed files.
#[tauri::command]
pub fn dedup_start(state: State<'_, AppState>) {
    state.engine.dedup_start();
}

/// Snapshot of the dedup state machine: phase, the current stage's
/// `done`/`total`, the last completed run's group count and the skipped
/// (unreadable / cloud-placeholder) file count.
#[tauri::command]
pub fn dedup_state(state: State<'_, AppState>) -> DedupState {
    state.engine.dedup_state()
}

/// One page `[offset, offset + limit)` of the last completed run's dupe
/// groups, size-descending, members materialized as search-style hits.
/// Files deleted since the run drop out; a group below two surviving
/// members dissolves.
#[tauri::command]
pub fn dedup_groups(state: State<'_, AppState>, offset: u32, limit: u32) -> Vec<DupeGroup> {
    state.engine.dedup_groups(offset, limit)
}

/// Moves the entries behind `ids` to the OS recycle bin — the engine's
/// ONLY deletion path, never a permanent delete. Returns how many paths
/// actually landed (dead ids and already-gone paths are skipped); errors
/// carry the engine message verbatim.
#[tauri::command]
pub fn delete_to_trash(state: State<'_, AppState>, ids: Vec<i64>) -> Result<usize, String> {
    state
        .engine
        .delete_to_trash(&ids)
        .map_err(|err| err.to_string())
}

// ---------------------- Task 17: flame chart ----------------------

/// Directory-size tree behind `root` for the flame chart — an index path
/// in the `Hit::path` spelling ("" is the virtual root over the scan
/// roots). `depth` caps materialized child levels, `max_children`
/// truncates each level with the overflow merged into a trailing
/// "(other)" node; the UI calls this with (path, 2, 40)-shaped arguments
/// and drills by re-rooting at a clicked node's path. An unknown root
/// (including a synthetic "(other)" path, which names no real dir) is
/// `None`, which the view renders as "path not found".
///
/// Async (thread-pool, not the main thread): the call takes the hot
/// index's read lock for a full O(dirs + files) aggregation walk, and on
/// a live tree the watcher's write-lock batches can delay that wait — a
/// sync command would freeze the whole window while blocked (observed in
/// the Task 17 verification on C:\Users\admin\AppData\Local and worked
/// around here; Task 20 owns the deeper lock/l latency question).
#[tauri::command]
pub async fn size_tree(
    state: State<'_, AppState>,
    root: String,
    depth: u8,
    max_children: u8,
) -> Result<Option<SizeNode>, String> {
    Ok(state.engine.size_tree(&root, depth, max_children))
}

/// Reveals an INDEX path (the `Hit::path` spelling — flame-chart nodes
/// carry paths, not row ids, so this is the path-based sibling of
/// `reveal_path`) in the OS file manager: resolve to the absolute disk
/// path through the SAME engine machinery the id-based reveal uses, then
/// `tauri-plugin-opener`'s select-in-Explorer (works for dirs and files).
/// A path no scan root resolves (synthetic "(all)"/"(other)" nodes,
/// garbage from the webview) is an error the view surfaces verbatim.
#[tauri::command]
pub fn reveal_index_path(
    state: State<'_, AppState>,
    app: AppHandle,
    path: String,
) -> Result<(), String> {
    let abs = state
        .engine
        .resolve_absolute(&path)
        .ok_or_else(|| format!("no scan root resolves \"{path}\""))?;
    app.opener()
        .reveal_item_in_dir(&abs)
        .map_err(|err| err.to_string())
}
