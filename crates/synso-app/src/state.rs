//! App state: the engine handle shared by every Tauri command, plus the
//! engine options the app starts with.
//!
//! DB layout: one SQLite database at `app_data_dir/synso.db` (the caller
//! creates the directory; see `lib.rs`). Engine options come from the
//! PERSISTED settings (read through a throwaway store connection before
//! `Engine::open`) — the user's chosen volumes/exclusions drive the
//! FIRST scan instead of the defaults (fix round 1 for Task 19:
//! `Settings.volumes`/`exclusions` used to be dead writes). Scope rules:
//!
//! - Debug builds honor `SYNSO_DEV_ROOT`: when set to a directory, it
//!   overrides the VOLUMES only (the fixture contract — `cargo tauri
//!   dev` indexes just that directory even when settings name volumes);
//!   the user's exclusions still apply. [`effective_scope`] is the ONE
//!   derivation, shared by the startup wiring and the settings
//!   propagation in `commands.rs`.
//! - Release builds use the settings verbatim: empty `volumes` = all
//!   fixed local volumes (the engine default), so a fresh install with
//!   no settings yet scans everything — exactly what the first-run
//!   wizard then narrows.

use std::path::Path;
use std::sync::Arc;

use synso_core::{Engine, EngineOptions, Settings};

/// Tauri-managed state: one engine for the whole process. Commands reach
/// it as `State<AppState>`.
pub struct AppState {
    pub engine: Arc<Engine>,
}

impl AppState {
    /// Opens (creating if needed) the index database at `db_path` with
    /// the scope from the persisted settings (see [`effective_scope`]).
    pub fn open(db_path: &Path) -> synso_core::Result<Self> {
        let settings = synso_core::load_persisted(db_path)?;
        let engine = Arc::new(Engine::open(db_path, engine_options(&settings))?);
        Ok(AppState { engine })
    }
}

/// The scan scope the engine should run with for `settings` — shared by
/// the startup wiring ([`engine_options`]) and the live propagation in
/// `commands::settings_set`, so restart and live change can never
/// disagree about what SYNSO_DEV_ROOT means.
pub fn effective_scope(settings: &Settings) -> (Vec<String>, Vec<String>) {
    #[cfg(debug_assertions)]
    if let Ok(root) = std::env::var("SYNSO_DEV_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return (vec![root.to_owned()], settings.exclusions.clone());
        }
    }
    (settings.volumes.clone(), settings.exclusions.clone())
}

/// The engine options for [`AppState::open`] — [`effective_scope`] as
/// [`EngineOptions`].
fn engine_options(settings: &Settings) -> EngineOptions {
    let (volumes, exclusions) = effective_scope(settings);
    EngineOptions {
        volumes,
        exclusions,
    }
}
