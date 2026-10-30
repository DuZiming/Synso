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
// Sync is safe: a hot-index read lock held for microseconds (the loader
// swaps in a fully built index, it never holds the lock while building).
#[tauri::command]
pub fn engine_stats(state: State<'_, AppState>) -> EngineStats {
    state.engine.stats()
}

/// Starts a full scan on the engine's background thread. One scan at a
/// time: a call while a scan runs is a no-op. Progress arrives as the
/// `scan-progress` / `scan-finished` / `scan-failed` events (see
/// `events.rs`).
// Sync is safe: claims the scan slot via compare-exchange and spawns the
// walk thread — no store lock is taken on the caller's thread.
#[tauri::command]
pub fn start_full_scan(state: State<'_, AppState>) {
    state.engine.start_full_scan();
}

/// The persisted user settings.
///
/// Async (thread-pool, not the main thread — see `size_tree`): the read
/// takes the store mutex, which a running scan holds for its whole walk,
/// and Tauri runs sync commands on the main thread — a sync `settings_get`
/// would freeze EVERY window for the scan's duration. Returns a `Result`
/// because Tauri requires that of async commands with reference inputs;
/// `Ok` serializes to exactly the `Settings` JSON the webview expects.
#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.engine.settings())
}

/// Persists the user settings; errors surface to the webview as a string.
/// Two live side effects:
/// - a changed index scope (volumes/exclusions) is applied to the ENGINE
///   via `set_scan_options` BEFORE persisting — that call validates the
///   patterns and swaps the live scope (purging out-of-scope roots), so
///   a bad glob can never reach the store and the SettingsView's
///   「需要重新扫描后生效」 note is literally true (the rescan then
///   reconciles the surviving roots' row-level drift). The scope goes
///   through `state::effective_scope`, so the dev fixture override
///   (SYNSO_DEV_ROOT) keeps precedence over the user's volumes exactly
///   like the startup wiring;
/// - a changed `lang` re-labels the tray menu live (see `tray.rs`).
///
/// Async (thread-pool, not the main thread — see `size_tree`): one call
/// takes the store mutex up to three times (settings read, then
/// `set_scan_options` — which serializes against an in-flight scan's
/// whole walk — then the persist), and it is reachable during a scan from
/// the SettingsView save and the wizard's completion — the exact freeze
/// class the rest of this file's conversions fix.
#[tauri::command]
pub async fn settings_set(
    state: State<'_, AppState>,
    app: AppHandle,
    settings: Settings,
) -> Result<(), String> {
    let old = state.engine.settings();
    if old.volumes != settings.volumes || old.exclusions != settings.exclusions {
        let (volumes, exclusions) = crate::state::effective_scope(&settings);
        state
            .engine
            .set_scan_options(volumes, exclusions)
            .map_err(|err| err.to_string())?;
    }
    state
        .engine
        .set_settings(&settings)
        .map_err(|err| err.to_string())?;
    crate::tray::relabel(&app, &settings.lang);
    Ok(())
}

// ---------------------- Task 19: tray / autostart / shortcut --------------

/// Re-registers the global quick-search shortcut LIVE (the settings
/// panel's 应用 button): unregister old, register new. A conflict or a
/// bad spelling rolls back to the old combo and returns the plugin's
/// error message for display. On success the new spelling is persisted,
/// so it survives a restart.
/// Async (thread-pool, not the main thread — see `size_tree`): the
/// settings read/write below takes the store mutex, which a running scan
/// holds for its whole walk — a sync command would freeze EVERY window
/// for the scan's duration (the shortcut re-registration itself touches
/// no engine state).
#[tauri::command]
pub async fn set_shortcut(
    app: AppHandle,
    state: State<'_, AppState>,
    combo: String,
) -> Result<(), String> {
    crate::quick::set_shortcut(&app, &combo)?;
    let mut settings = state.engine.settings();
    settings.shortcut = combo;
    state
        .engine
        .set_settings(&settings)
        .map_err(|err| err.to_string())
}

/// Live autostart toggle (the settings panel's switch): updates the OS
/// launch-at-startup registration first, then persists
/// `Settings.autostart` — an OS failure must not persist a lie.
/// Async (thread-pool, not the main thread — see `size_tree`): the
/// settings read/write below takes the store mutex, which a running scan
/// holds for its whole walk — a sync command would freeze EVERY window
/// for the scan's duration (the OS toggle itself touches no engine
/// state).
#[tauri::command]
pub async fn autostart_set(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    let outcome = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    outcome.map_err(|err| err.to_string())?;
    let mut settings = state.engine.settings();
    settings.autostart = enabled;
    state
        .engine
        .set_settings(&settings)
        .map_err(|err| err.to_string())
}

/// The fixed local volumes, for the first-run wizard's volume picker.
/// Async (thread-pool): sysinfo's disk refresh can take 1–3s, and a sync
/// command runs on the MAIN thread — the settings page's volume list
/// would freeze every window while it enumerates.
#[tauri::command]
pub async fn list_volumes() -> Vec<VolumeInfo> {
    synso_core::list_volumes()
}

// ---------------------- Task 15: quick search window ----------------------

/// Everything-style search: page `[offset, offset + limit)` of the ranked
/// matches. Query modifiers (`ext:` / `size:` / `path:` / `coll:`) are
/// parsed inside the engine, exactly as the dashboard path would. On a
/// cold index the hot side is empty, so this returns an empty page — the
/// quick window says "warming up" via `engine_stats`'s `warm` flag then.
///
/// Async (thread-pool, not the main thread — see `size_tree`): a query
/// with a `coll:` modifier resolves collection names under the store
/// mutex, which a running scan holds for its whole walk — typing
/// `coll:x` during a scan would otherwise freeze EVERY window. Returns a
/// `Result` because Tauri requires that of async commands with reference
/// inputs; `Ok` serializes to exactly the `SearchResult` JSON.
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    q: String,
    offset: u32,
    limit: u32,
) -> Result<SearchResult, String> {
    Ok(state.engine.search(&q, offset, limit))
}

/// The engine's own parse of the query, for the quick window's modifier
/// chips — the SAME parser the search path runs, so the chips can never
/// disagree with what was actually searched.
// Sync is safe: pure text parsing, no locks.
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

/// Shows + focuses the quick window with the query PRE-FILLED — the
/// collections view's 「在快速搜索中查看」 button hands over `coll:<name>`.
/// The same show logic the dashboard button (`quick_show`) uses, plus one
/// `quick-query` event `emit_to` the quick webview (the pre-fill hook: the
/// window's page is mounted from startup, its listener applies the text
/// whenever it lands — see `quick::show_with_query`). snake_case payload
/// key, kebab-case event name, per the events.rs conventions.
#[tauri::command]
pub fn quick_show_query(app: AppHandle, q: String) {
    quick::show_with_query(&app, &q);
}

// ---------------------- Task 16: dedup dashboard ----------------------

/// Starts a dedup run (or, while one is already going, is a harmless
/// no-op — the engine keeps exactly one run at a time). Progress arrives
/// as the `dedup-progress` / `dedup-finished` events (see `events.rs`).
/// The view's real "boost" semantic is calling this NOW instead of
/// waiting for the engine's 30s post-scan auto-start; persisted hashes
/// make every later run skip I/O on already-hashed files.
// Sync is safe: an atomic claim plus a thread spawn, no locks held.
#[tauri::command]
pub fn dedup_start(state: State<'_, AppState>) {
    state.engine.dedup_start();
}

/// Snapshot of the dedup state machine: phase, the current stage's
/// `done`/`total`, the last completed run's group count and the skipped
/// (unreadable / cloud-placeholder) file count.
// Sync is safe: a copy out of the small bookkeeping mutex.
#[tauri::command]
pub fn dedup_state(state: State<'_, AppState>) -> DedupState {
    state.engine.dedup_state()
}

/// One page `[offset, offset + limit)` of the last completed run's dupe
/// groups, size-descending, members materialized as search-style hits.
/// Files deleted since the run drop out; a group below two surviving
/// members dissolves.
// Sync is safe: an Arc snapshot of the stored groups plus a hot-index
// read lock — the store mutex is never taken (v1.1 audit).
#[tauri::command]
pub fn dedup_groups(state: State<'_, AppState>, offset: u32, limit: u32) -> Vec<DupeGroup> {
    state.engine.dedup_groups(offset, limit)
}

/// Moves the entries behind `ids` to the OS recycle bin — the engine's
/// ONLY deletion path, never a permanent delete. Returns how many paths
/// actually landed (dead ids and already-gone paths are skipped); errors
/// carry the engine message verbatim.
///
/// Async (thread-pool, not the main thread — see `size_tree`): deletion
/// takes the store mutex, which a running scan holds for its whole walk —
/// a sync command would freeze EVERY window for the scan's duration.
#[tauri::command]
pub async fn delete_to_trash(state: State<'_, AppState>, ids: Vec<i64>) -> Result<usize, String> {
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
    include_files: bool,
) -> Result<Option<SizeNode>, String> {
    Ok(if include_files {
        state.engine.size_tree(&root, depth, max_children)
    } else {
        state.engine.size_tree_dirs(&root, depth, max_children)
    })
}

/// Physical-capacity overlay for a pane root that names exactly one
/// volume (`None` at every other level — see `Engine::volume_overlay`).
/// Async like `size_tree`: it walks the hot index for the indexed sum
/// and queries sysinfo for the volume's totals. The `Result` wrapper is
/// Tauri's requirement for async commands with reference-carrying
/// returns; `Ok` serializes to exactly the `Option<VolumeOverlay>` JSON.
#[tauri::command]
pub async fn volume_overlay(
    state: State<'_, AppState>,
    root: String,
) -> Result<Option<synso_core::VolumeOverlay>, String> {
    Ok(state.engine.volume_overlay(&root))
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

/// Resolves an INDEX path (the `Hit::path` spelling, directories included)
/// to its `files` row id — the bridge that lets the flame chart (whose
/// nodes carry paths, not ids) add its node — directory OR file, the node
/// itself, not its children — to a collection. An unresolvable path is an
/// error the view toasts verbatim.
#[tauri::command]
pub async fn file_id_of(state: State<'_, AppState>, path: String) -> Result<i64, String> {
    state
        .engine
        .file_id_of(&path)
        .ok_or_else(|| format!("not in the index: \"{path}\""))
}

/// Opens the native folder picker and returns the chosen path (None on
/// cancel). Sync ON PURPOSE: sync commands run on the main thread, where a
/// modal native dialog is the standard pattern (it pumps the message loop).
/// No engine state, no locks — nothing to keep off the main thread.
#[tauri::command]
pub fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|f| f.as_path().map(|p| p.to_string_lossy().into_owned()))
}
