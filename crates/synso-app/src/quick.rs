//! Quick search window lifecycle (Task 15, live re-registration Task 19).
//!
//! The window itself is declared in `tauri.conf.json` (frameless, always
//! on top, out of the taskbar, starts hidden); this module owns everything
//! runtime: placement (top-center of the monitor, top edge at
//! [`TOP_FRACTION`] of its height), hide-on-focus-loss, the global
//! shortcut's show/hide toggle, live re-registration ([`set_shortcut`],
//! the settings panel's 应用 button), and the command backends
//! `quick_show` / `quick_hide` / `quick_info` (see `commands.rs`).
//!
//! Shortcut: `settings.shortcut` (default `"Ctrl+;"`) is registered at
//! startup and re-registered live by [`set_shortcut`]. A parse or
//! registration failure (taken by another app, garbage spelling) never
//! fails the launch or leaves the user shortcutless: it is logged, kept
//! in the managed [`QuickState`], surfaced by the dashboard through
//! `quick_info` — and a failed re-registration rolls back to the previous
//! combo.

use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow, WindowEvent};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

/// Fraction of the monitor height the window's top edge sits at (the
/// Everything-style near-top placement; horizontal centering is exact).
const TOP_FRACTION: f64 = 0.18;

/// The `tauri.conf.json` label of the quick window.
pub const LABEL: &str = "quick";

/// The pre-fill event the quick window's page listens for (kebab-case
/// name; snake_case payload key — see `events.rs` conventions). Emitted by
/// [`show_with_query`] and only consumed there.
pub const QUERY_EVENT: &str = "quick-query";

/// [`QUERY_EVENT`]'s payload: the query text to pre-fill.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuickQueryPayload {
    pub q: String,
}

/// Global-shortcut registration outcome, kept as managed state so
/// `quick_info` can report it to the dashboard at any time and
/// [`set_shortcut`] can roll a failed re-registration back. Interior
/// mutability: the registration is updated live from the `set_shortcut`
/// command.
pub struct QuickState {
    inner: Mutex<QuickInner>,
}

#[derive(Debug, Clone)]
struct QuickInner {
    shortcut: String,
    registered: bool,
    error: Option<String>,
}

/// `quick_info`'s payload — the serde view of [`QuickState`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuickInfo {
    pub shortcut: String,
    pub registered: bool,
    pub error: Option<String>,
}

impl QuickState {
    fn new(shortcut: String, registered: bool, error: Option<String>) -> Self {
        QuickState {
            inner: Mutex::new(QuickInner {
                shortcut,
                registered,
                error,
            }),
        }
    }

    pub fn info(&self) -> QuickInfo {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        QuickInfo {
            shortcut: inner.shortcut.clone(),
            registered: inner.registered,
            error: inner.error.clone(),
        }
    }

    fn replace(&self, shortcut: String, registered: bool, error: Option<String>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.shortcut = shortcut;
        inner.registered = registered;
        inner.error = error;
    }
}

/// Startup wiring: place the window, hide it on focus loss, register the
/// global shortcut from settings. Orderly degradation throughout — a
/// missing window or a taken shortcut only removes one entry point.
pub fn init(app: &AppHandle) {
    let shortcut = app
        .try_state::<crate::state::AppState>()
        .map(|state| state.engine.settings().shortcut)
        .unwrap_or_default();

    if let Some(win) = app.get_webview_window(LABEL) {
        place_near_top(&win);
        let handle = app.clone();
        win.on_window_event(move |event| {
            if let WindowEvent::Focused(false) = event {
                hide(&handle);
            }
        });
    } else {
        log::warn!("synso: quick window {LABEL:?} missing from tauri.conf.json");
    }

    let (registered, error) = match register_shortcut(app, &shortcut) {
        Ok(()) => (true, None),
        Err(err) => {
            log::warn!(
                "synso: global shortcut {shortcut:?} unavailable ({err}); \
                 the dashboard's quick-search button still opens the window"
            );
            (false, Some(err))
        }
    };
    app.manage(QuickState::new(shortcut, registered, error));
}

/// Re-registers the global shortcut LIVE (the settings panel's 应用
/// button, Task 19): unregisters the old combo, registers the new one.
/// A failure (conflict or bad spelling) rolls back — the old combo is
/// re-registered and the managed state keeps reporting it — and the
/// error string goes back to the caller for display. Persisting the new
/// spelling is the command's job (`commands::set_shortcut`), so a
/// registration that succeeded but a save that failed stays honest.
///
/// Re-registering the same spelling is allowed (unregister first, then
/// register again) — an idempotent "apply".
pub fn set_shortcut(app: &AppHandle, combo: &str) -> Result<(), String> {
    let state = app.state::<QuickState>();
    let (old, old_registered) = {
        let inner = state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (inner.shortcut.clone(), inner.registered)
    };

    if old_registered {
        // A no-op unregister (the plugin never had it) is harmless — its
        // only cost is the log line.
        if let Err(err) = app.global_shortcut().unregister(old.as_str()) {
            log::warn!("synso: unregistering old shortcut {old:?} failed: {err}");
        }
    }

    match register_shortcut(app, combo) {
        Ok(()) => {
            state.replace(combo.to_owned(), true, None);
            log::info!("synso: global shortcut re-registered: {old:?} -> {combo:?}");
            Ok(())
        }
        Err(err) => {
            log::warn!(
                "synso: re-registering global shortcut {combo:?} failed ({err}); \
                 rolling back to {old:?}"
            );
            let (registered, error) = if old_registered {
                match register_shortcut(app, &old) {
                    Ok(()) => (true, None),
                    Err(rollback) => (false, Some(rollback)),
                }
            } else {
                // The old combo was already unregistered at startup; the
                // state keeps reporting exactly that.
                (false, None)
            };
            state.replace(old, registered, error);
            Err(err)
        }
    }
}

/// Registers `spelling` with the show/hide toggle as its handler. Both a
/// bad spelling (`settings.shortcut` is user-editable text) and an OS-side
/// rejection (another app owns it) surface as the error string.
fn register_shortcut(app: &AppHandle, spelling: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::ShortcutState;

    app.global_shortcut()
        .on_shortcut(spelling, |app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                toggle(app);
            }
        })
        .map_err(|err| err.to_string())
}

/// Shows + focuses the window, refreshing the placement first (monitors
/// change; a popup lives where its user is). Already visible → re-focus.
pub fn show(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(LABEL) {
        place_near_top(&win);
        if let Err(err) = win.show() {
            log::warn!("synso: quick window show failed: {err}");
            return;
        }
        let _ = win.set_focus();
    }
}

/// Hides the window — the resting state between queries.
pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.hide();
    }
}

/// Shows + focuses the window AND pre-fills its query: the exact [`show`]
/// logic first (placement refresh, show, focus — the dashboard button's
/// path), then one [`QUERY_EVENT`] `emit_to` the quick webview. The page
/// is mounted from startup, so the listener applies the text whenever it
/// lands; a failed emit (window gone) is logged, not fatal — the window
/// still showed with its previous query.
pub fn show_with_query(app: &AppHandle, q: &str) {
    show(app);
    if let Err(err) = app.emit_to(LABEL, QUERY_EVENT, QuickQueryPayload { q: q.to_owned() }) {
        log::warn!("synso: quick query pre-fill failed: {err}");
    }
}

/// The global shortcut's action: hidden → show + focus, visible → hide.
pub fn toggle(app: &AppHandle) {
    let visible = app
        .get_webview_window(LABEL)
        .and_then(|win| win.is_visible().ok())
        .unwrap_or(false);
    if visible {
        hide(app);
    } else {
        show(app);
    }
}

/// Centers the window horizontally on its monitor with the top edge at
/// [`TOP_FRACTION`] of the monitor height. Physical pixels throughout (a
/// hidden window already has its config size, scaled), so DPI needs no
/// extra handling; the monitor under the cursor wins over the primary one
/// so the popup follows the user across screens. Best-effort: no monitor,
/// no placement.
fn place_near_top(win: &WebviewWindow) {
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten());
    let (Some(monitor), Ok(size)) = (monitor, win.outer_size()) else {
        return;
    };
    let mon = monitor.size();
    let pos = monitor.position();
    let x = pos.x + ((mon.width as f64 - size.width as f64) / 2.0).round() as i32;
    let y = pos.y + (mon.height as f64 * TOP_FRACTION).round() as i32;
    let _ = win.set_position(PhysicalPosition::new(x, y));
}
