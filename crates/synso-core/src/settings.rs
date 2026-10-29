//! User settings persisted in the store's `meta` table (key/value TEXT).
//!
//! One meta row per field, under `settings.*` keys. A missing key means
//! "use the default" — a fresh database simply yields [`Settings::default`].
//! List fields (`volumes`, `exclusions`) are stored newline-joined: LF
//! cannot appear in a Windows path, and a glob pattern containing a line
//! break is pathological enough to document away (both list kinds are
//! edited by the settings UI, not typed by hand).
//!
//! Reads degrade to defaults on store errors: settings are UI
//! convenience, never worth failing a startup over. Writes surface errors
//! to the caller (`Engine::set_settings`).

use std::path::Path;

use crate::store::Store;
use crate::Result;

/// Meta-table key prefix for every settings field.
const KEY_PREFIX: &str = "settings.";

/// `settings.lang` default: follow the OS locale until the user picks.
pub const DEFAULT_LANG: &str = "auto";

/// `settings.shortcut` default: the design's global hotkey spelling.
pub const DEFAULT_SHORTCUT: &str = "Ctrl+;";

/// User-visible settings (the brief's five fields plus the first-run
/// wizard's completion flag), backed by the `meta` table via
/// [`Engine::settings`] / [`Engine::set_settings`].
///
/// [`Engine::settings`]: crate::Engine::settings
/// [`Engine::set_settings`]: crate::Engine::set_settings
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Settings {
    /// UI language: `"auto"` (follow the OS locale), `"zh"` or `"en"`.
    pub lang: String,
    /// Launch Synso on system startup.
    pub autostart: bool,
    /// Globset exclusion patterns (see `crate::scanner::ScanOptions`).
    pub exclusions: Vec<String>,
    /// Volumes to index (mount paths); empty = all fixed local volumes.
    pub volumes: Vec<String>,
    /// Global hotkey spelling, e.g. `"Ctrl+;"`.
    pub shortcut: String,
    /// The first-run wizard has completed: the app opens on the dashboard
    /// instead of the wizard (the wizard writes `true` on its final step).
    pub first_run_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: DEFAULT_LANG.to_owned(),
            autostart: true,
            exclusions: Vec::new(),
            volumes: Vec::new(),
            shortcut: DEFAULT_SHORTCUT.to_owned(),
            first_run_done: false,
        }
    }
}

/// Reads the settings from `store`; every missing (or unreadable) field
/// falls back to its default, so a fresh database yields
/// [`Settings::default`].
pub fn load(store: &Store) -> Settings {
    Settings {
        lang: read(store, "lang").unwrap_or_else(|| DEFAULT_LANG.to_owned()),
        autostart: read(store, "autostart")
            .and_then(|v| match v.as_str() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            })
            .unwrap_or(true),
        exclusions: read_list(store, "exclusions"),
        volumes: read_list(store, "volumes"),
        shortcut: read(store, "shortcut").unwrap_or_else(|| DEFAULT_SHORTCUT.to_owned()),
        first_run_done: read(store, "first_run_done")
            .and_then(|v| match v.as_str() {
                "0" => Some(false),
                "1" => Some(true),
                _ => None,
            })
            .unwrap_or(false),
    }
}

/// Reads the persisted settings straight from the database at `db_path`
/// (a throwaway store connection). The startup hook for embedders that
/// must know the scope BEFORE [`crate::Engine::open`]: the app passes
/// `Settings.volumes` / `Settings.exclusions` into `EngineOptions`, so
/// the user's chosen scope drives the FIRST scan instead of the defaults.
/// A missing database is created (exactly what `Engine::open` would do),
/// missing fields read as defaults, and a store error is an `Err` — the
/// engine open right after would fail the same way.
pub fn load_persisted(db_path: &Path) -> Result<Settings> {
    let store = Store::open(db_path)?;
    Ok(load(&store))
}

/// Writes every field of `settings` to `store` as its `settings.*` meta
/// row. Store errors surface to the caller.
pub fn save(store: &Store, settings: &Settings) -> Result<()> {
    store.set_meta(&format!("{KEY_PREFIX}lang"), &settings.lang)?;
    store.set_meta(
        &format!("{KEY_PREFIX}autostart"),
        if settings.autostart { "1" } else { "0" },
    )?;
    write_list(store, "exclusions", &settings.exclusions)?;
    write_list(store, "volumes", &settings.volumes)?;
    store.set_meta(&format!("{KEY_PREFIX}shortcut"), &settings.shortcut)?;
    store.set_meta(
        &format!("{KEY_PREFIX}first_run_done"),
        if settings.first_run_done { "1" } else { "0" },
    )?;
    Ok(())
}

/// One text field; a store error counts as "unset" (defaults win — see
/// the module docs).
fn read(store: &Store, field: &str) -> Option<String> {
    let key = format!("{KEY_PREFIX}{field}");
    match store.get_meta(&key) {
        Ok(value) => value.filter(|v| !v.is_empty()),
        Err(err) => {
            log::warn!("synso: reading settings.{field} failed, using the default: {err}");
            None
        }
    }
}

/// One list field, newline-joined in the meta row; missing or unreadable
/// rows read as empty.
fn read_list(store: &Store, field: &str) -> Vec<String> {
    read(store, field)
        .map(|joined| {
            if joined.is_empty() {
                Vec::new()
            } else {
                joined.split('\n').map(str::to_owned).collect()
            }
        })
        .unwrap_or_default()
}

/// Writes one list field newline-joined (an empty list writes the empty
/// string, not an absent row, so an explicit "cleared" survives a reload).
fn write_list(store: &Store, field: &str, list: &[String]) -> Result<()> {
    store.set_meta(&format!("{KEY_PREFIX}{field}"), &list.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineOptions};
    use std::path::Path;

    /// THE core-side settings contract (the brief's Step 2): a fresh
    /// database loads the documented defaults, a save followed by a reload
    /// (both through the Engine facade, i.e. through the real meta table)
    /// returns the saved values unchanged.
    #[test]
    fn settings_roundtrip_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");

        // Fresh DB: the documented defaults, verbatim.
        let first = open_engine(&db_path);
        assert_eq!(first.settings(), Settings::default());
        assert_eq!(first.settings().lang, "auto");
        assert!(first.settings().autostart);
        assert!(first.settings().exclusions.is_empty());
        assert!(first.settings().volumes.is_empty());
        assert_eq!(first.settings().shortcut, "Ctrl+;");
        assert!(!first.settings().first_run_done);

        // Save a modified set.
        let modified = Settings {
            lang: "zh".to_owned(),
            autostart: false,
            exclusions: vec!["*.tmp".to_owned(), "**/node_modules/**".to_owned()],
            volumes: vec!["C:\\".to_owned()],
            shortcut: "Ctrl+Alt+S".to_owned(),
            first_run_done: true,
        };
        first.set_settings(&modified).unwrap();
        assert_eq!(first.settings(), modified, "read-back after save");

        // Reload through a NEW engine over the SAME database: persistence,
        // not just the in-memory value.
        drop(first);
        let second = open_engine(&db_path);
        assert_eq!(second.settings(), modified);

        // Saving the defaults back restores them exactly.
        second.set_settings(&Settings::default()).unwrap();
        assert_eq!(second.settings(), Settings::default());
    }

    /// Partially-written state (only some fields present, as when a future
    /// schema adds fields) loads missing fields as defaults.
    #[test]
    fn partial_settings_fall_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("index.db")).unwrap();

        // Write ONE field through the store's raw meta accessor.
        store.set_meta("settings.lang", "en").unwrap();

        let loaded = load(&store);
        assert_eq!(loaded.lang, "en");
        assert_eq!(loaded.shortcut, "Ctrl+;", "unset field keeps its default");
        assert!(loaded.autostart, "unset boolean keeps its default");
        assert!(
            !loaded.first_run_done,
            "unset boolean keeps its default (wizard still pending)"
        );
    }

    /// An empty list round-trips as an explicit empty row (not a missing
    /// one), so clearing a list survives a reload.
    #[test]
    fn empty_lists_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("index.db")).unwrap();

        let settings = Settings {
            exclusions: Vec::new(),
            volumes: vec!["C:\\".to_owned()],
            ..Settings::default()
        };
        save(&store, &settings).unwrap();

        // Simulate a stale non-empty row being cleared: overwrite with the
        // empty list and reload.
        let mut cleared = settings.clone();
        cleared.volumes = Vec::new();
        save(&store, &cleared).unwrap();
        assert_eq!(load(&store), cleared);
    }

    fn open_engine(db_path: &Path) -> Engine {
        Engine::open(db_path, EngineOptions::default()).unwrap()
    }
}
