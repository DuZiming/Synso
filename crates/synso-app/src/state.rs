//! App state: the engine handle shared by every Tauri command, plus the
//! engine options the app starts with.
//!
//! DB layout: one SQLite database at `app_data_dir/synso.db` (the caller
//! creates the directory; see `lib.rs`). Engine options:
//!
//! - Debug builds honor `SYNSO_DEV_ROOT`: when set to a directory, the
//!   engine indexes ONLY that directory, so `cargo tauri dev` never
//!   crawls the whole machine unless the developer asks for it.
//! - Release builds ignore the variable and use the engine default
//!   (empty roots = all fixed local volumes). The first-run wizard wires
//!   the user's persistent `Settings::volumes` choice in a later task.

use std::path::Path;
use std::sync::Arc;

use synso_core::{Engine, EngineOptions};

/// Tauri-managed state: one engine for the whole process. Commands reach
/// it as `State<AppState>`.
pub struct AppState {
    pub engine: Arc<Engine>,
}

impl AppState {
    /// Opens (creating if needed) the index database at `db_path` with the
    /// options from [`engine_options`].
    pub fn open(db_path: &Path) -> synso_core::Result<Self> {
        let engine = Arc::new(Engine::open(db_path, engine_options())?);
        Ok(AppState { engine })
    }
}

/// The scan roots the engine starts with — the dev fixture override in
/// debug builds, the all-fixed-volumes default otherwise (see module docs).
fn engine_options() -> EngineOptions {
    #[cfg(debug_assertions)]
    if let Ok(root) = std::env::var("SYNSO_DEV_ROOT") {
        let root = root.trim();
        if !root.is_empty() {
            return EngineOptions {
                volumes: vec![root.to_owned()],
                exclusions: Vec::new(),
            };
        }
    }
    EngineOptions::default()
}
