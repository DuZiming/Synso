//! The IPC surface the webview calls. Thin wrappers over the engine
//! facade — no business logic lives here.

use synso_core::{EngineStats, Settings, VolumeInfo};
use tauri::State;

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
