// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Dev fixture root (debug builds ONLY): set `SYNSO_DEV_ROOT` to a small
// directory and `tauri dev` will index just that instead of every fixed
// volume, and auto-start one scan at launch. Release builds ignore the
// variable entirely (see `state::engine_options`).
fn main() {
    synso_app_lib::run()
}
