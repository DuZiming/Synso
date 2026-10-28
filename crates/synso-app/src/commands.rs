//! The IPC surface the webview calls. Thin wrappers over the engine
//! facade — no business logic lives here.

use std::path::PathBuf;

use synso_core::{EngineStats, Hit, ParsedQuery, SearchResult, Settings, VolumeInfo};
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
