//! Synso desktop app: the Tauri v2 shell over the [`synso_core`] engine.
//!
//! Assembly (see `state.rs` for the engine options and the dev fixture
//! override): `run` opens the engine at `<app_data_dir>/synso.db`, starts
//! the explicit filesystem watcher, subscribes to the engine's event
//! channel and spawns the forwarder thread that re-emits engine events to
//! the webview (`events.rs`). The webview talks to the engine only through
//! the IPC commands in `commands.rs` — the app depends on the engine; the
//! engine never depends on Tauri.

mod commands;
mod events;
mod quick;
mod state;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Open/reveal (Task 15) goes through the official opener plugin —
        // default-app open and Explorer select, cross-platform.
        .plugin(tauri_plugin_opener::init())
        // Global shortcut for the quick window; registration itself
        // happens in `quick::init` from the user's settings spelling.
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // DB path = app_data_dir/synso.db, directory created on demand.
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("synso.db");

            let app_state = state::AppState::open(&db_path)?;
            // Subscribe BEFORE anything can fire, so no event is missed.
            let engine_events = app_state.engine.subscribe();
            // The watcher is EXPLICIT in the engine: the app wants live
            // updates, so start it right away. Failure is logged, not
            // fatal — Synso stays usable without FsChanged refreshes.
            if let Err(err) = app_state.engine.start_watcher() {
                log::warn!("synso: failed to start the filesystem watcher: {err}");
            }
            app.manage(app_state);
            events::spawn_forwarder(app.handle().clone(), engine_events);

            // Quick window: placement, focus-loss hide, global shortcut
            // (reads settings, so after `app.manage`).
            quick::init(app.handle());

            #[cfg(debug_assertions)]
            dev_autoscan(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::engine_stats,
            commands::start_full_scan,
            commands::settings_get,
            commands::settings_set,
            commands::list_volumes,
            commands::search,
            commands::parse_preview,
            commands::open_path,
            commands::reveal_path,
            commands::quick_show,
            commands::quick_hide,
            commands::quick_info,
            commands::dedup_start,
            commands::dedup_state,
            commands::dedup_groups,
            commands::delete_to_trash,
            commands::size_tree,
            commands::reveal_index_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Dev convenience (debug builds only): with `SYNSO_DEV_ROOT` set, run one
/// full scan right after launch so `tauri dev` immediately shows real
/// numbers for the fixture directory. Release builds never auto-scan.
#[cfg(debug_assertions)]
fn dev_autoscan(app: &tauri::App) {
    let fixture = std::env::var("SYNSO_DEV_ROOT").is_ok_and(|v| !v.trim().is_empty());
    if fixture {
        if let Some(state) = app.try_state::<state::AppState>() {
            state.engine.start_full_scan();
        }
    }
}
