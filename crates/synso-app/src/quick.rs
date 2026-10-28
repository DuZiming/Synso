//! Quick search window lifecycle (Task 15).
//!
//! The window itself is declared in `tauri.conf.json` (frameless, always
//! on top, out of the taskbar, starts hidden); this module owns everything
//! runtime: placement (top-center of the monitor, top edge at
//! [`TOP_FRACTION`] of its height), hide-on-focus-loss, the global
//! shortcut's show/hide toggle, and the command backends `quick_show` /
//! `quick_hide` / `quick_info` (see `commands.rs`).
//!
//! Shortcut: `settings.shortcut` (default `"Ctrl+;"`) is registered once
//! at startup — re-registration on a later settings change is Task 19. A
//! parse or registration failure (taken by another app, garbage spelling)
//! never fails the launch: it is logged, kept in the managed
//! [`QuickState`], and surfaced by the dashboard through `quick_info`,
//! whose "quick search" button stays as the always-working fallback.

use tauri::{AppHandle, Manager, PhysicalPosition, WebviewWindow, WindowEvent};

/// Fraction of the monitor height the window's top edge sits at (the
/// Everything-style near-top placement; horizontal centering is exact).
const TOP_FRACTION: f64 = 0.18;

/// The `tauri.conf.json` label of the quick window.
pub const LABEL: &str = "quick";

/// Global-shortcut registration outcome, kept as managed state so
/// `quick_info` can report it to the dashboard at any time.
pub struct QuickState {
    pub shortcut: String,
    pub registered: bool,
    pub error: Option<String>,
}

/// `quick_info`'s payload — the serde view of [`QuickState`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuickInfo {
    pub shortcut: String,
    pub registered: bool,
    pub error: Option<String>,
}

impl QuickState {
    pub fn info(&self) -> QuickInfo {
        QuickInfo {
            shortcut: self.shortcut.clone(),
            registered: self.registered,
            error: self.error.clone(),
        }
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
    app.manage(QuickState {
        shortcut,
        registered,
        error,
    });
}

/// Registers `spelling` with the show/hide toggle as its handler. Both a
/// bad spelling (`settings.shortcut` is user-editable text) and an OS-side
/// rejection (another app owns it) surface as the error string.
fn register_shortcut(app: &AppHandle, spelling: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

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
