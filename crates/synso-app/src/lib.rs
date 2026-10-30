//! Synso desktop app: the Tauri v2 shell over the [`synso_core`] engine.
//!
//! Assembly (see `state.rs` for the engine options and the dev fixture
//! override): `run` opens the engine at `<app_data_dir>/synso.db`, starts
//! the explicit filesystem watcher, subscribes to the engine's event
//! channel and spawns the forwarder thread that re-emits engine events to
//! the webview (`events.rs`). The webview talks to the engine only through
//! the IPC commands in `commands.rs` — the app depends on the engine; the
//! engine never depends on Tauri.
//!
//! Desktop shell (Task 19): a file logger (`logging.rs`) captures every
//! `log` line — the engine's errors included — under
//! `<app_data_dir>/logs/`; the built-in tray (`tray.rs`) owns the
//! open/quick/rescan/settings/quit menu, and closing the main window
//! hides to the tray instead of exiting; OS autostart mirrors
//! `Settings.autostart` at startup; a second launch hands over to the
//! running instance (single-instance plugin) instead of starting a second
//! engine.

mod commands;
mod events;
mod logging;
mod quick;
mod state;
mod tray;

use tauri::Manager;
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Single instance FIRST (plugin docs): the second process must be
        // waved off before any other plugin does work. The existing app
        // shows + focuses its main window (visible again if hidden).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main(app);
        }))
        // Open/reveal (Task 15) goes through the official opener plugin —
        // default-app open and Explorer select, cross-platform.
        .plugin(tauri_plugin_opener::init())
        // OS launch-at-startup registration (Task 19); the value comes
        // from settings in `setup` below (and the settings panel's live
        // switch through the `autostart_set` command).
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        // Native dialogs (Task 19): the mid-scan quit confirmation.
        .plugin(tauri_plugin_dialog::init())
        // Global shortcut for the quick window; registration itself
        // happens in `quick::init` from the user's settings spelling,
        // and live re-registration in `quick::set_shortcut`.
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // Logging FIRST, so every later line — engine open included —
            // lands in the file. Never fatal: an unopenable log degrades
            // to the stdout mirror (debug builds).
            let data_dir = app.path().app_data_dir()?;
            logging::init(&data_dir.join("logs"));

            // DB path = app_data_dir/synso.db, directory created on demand.
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

            // Tray: icon + the five-item menu (best-effort — a failed
            // tray leaves the app running window-only, logged loudly).
            if let Err(err) = tray::init(app.handle()) {
                log::error!("synso: tray init failed (app continues without it): {err}");
            }

            // Closing the main window hides to the tray — never exits
            // (the brief: 关闭主窗 = 隐藏至托盘). The only real exit is the
            // tray menu's 退出 (`tray::quit`, with the mid-scan confirm).
            if let Some(win) = app.get_webview_window(tray::MAIN_WINDOW_LABEL) {
                let hide_target = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = hide_target.hide();
                    }
                });
            }

            // Autostart reflection (ruling 3): align the OS registration
            // with the persisted setting (default on since the wizard).
            // Best-effort: a registry failure logs and moves on.
            {
                let want = app.state::<state::AppState>().engine.settings().autostart;
                let launcher = app.autolaunch();
                let have = launcher.is_enabled().unwrap_or(false);
                if want != have {
                    let outcome = if want {
                        launcher.enable()
                    } else {
                        launcher.disable()
                    };
                    if let Err(err) = outcome {
                        log::warn!("synso: aligning OS autostart to {want} failed: {err}");
                    } else {
                        log::info!("synso: OS autostart aligned to {want}");
                    }
                }
            }

            #[cfg(debug_assertions)]
            dev_autoscan(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::engine_stats,
            commands::start_full_scan,
            commands::settings_get,
            commands::settings_set,
            commands::set_shortcut,
            commands::autostart_set,
            commands::list_volumes,
            commands::search,
            commands::parse_preview,
            commands::open_path,
            commands::reveal_path,
            commands::quick_show,
            commands::quick_hide,
            commands::quick_info,
            commands::quick_show_query,
            commands::dedup_start,
            commands::dedup_state,
            commands::dedup_groups,
            commands::delete_to_trash,
            commands::size_tree,
            commands::volume_overlay,
            commands::reveal_index_path,
            commands::coll_create,
            commands::coll_rename,
            commands::coll_delete,
            commands::coll_list,
            commands::coll_add,
            commands::coll_remove,
            commands::coll_members,
            commands::file_id_of,
            commands::pick_folder
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
