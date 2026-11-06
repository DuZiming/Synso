//! Double-Ctrl quick-search summon (v1.2 user request): pressing the
//! Ctrl key TWICE within [`TAP_WINDOW`] summons the quick window — the
//! classic launcher gesture. A low-level keyboard hook (rdev) feeds a
//! tiny state machine; the fire signal crosses to a normal thread via a
//! channel so the hook callback never blocks.
//!
//! The configured combo shortcut (default `Ctrl+;`) stays registered as
//! the always-working fallback; double-Ctrl is an additional, always-on
//! trigger.

use std::time::{Duration, Instant};

use rdev::{Event, EventType, Key};
use tauri::Manager;

use crate::quick;

/// Two Ctrl presses inside this window count as a double tap.
const TAP_WINDOW: Duration = Duration::from_millis(500);

/// The double-tap state machine, pure and clock-injected for tests.
pub(crate) struct DoubleTap {
    last_ctrl: Option<Instant>,
}

impl DoubleTap {
    pub(crate) fn new() -> DoubleTap {
        DoubleTap { last_ctrl: None }
    }

    /// Feed one key PRESS; returns true when this press completes a
    /// double tap. Any other key press resets the run (Ctrl, X, Ctrl
    /// must NOT fire).
    pub(crate) fn feed(&mut self, key: TapKey, now: Instant) -> bool {
        match key {
            TapKey::Ctrl => match self.last_ctrl {
                Some(at) if now.duration_since(at) <= TAP_WINDOW => {
                    self.last_ctrl = None;
                    true
                }
                _ => {
                    self.last_ctrl = Some(now);
                    false
                }
            },
            TapKey::Other => {
                self.last_ctrl = None;
                false
            }
        }
    }
}

/// The key classes the detector distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TapKey {
    Ctrl,
    Other,
}

/// Spawns the hook thread (never returns; one per process).
///
/// WINDOWS ONLY (macOS port, 2026-11-06): rdev's macOS event-tap callback
/// calls HIToolbox's TIS APIs from a non-main dispatch context — the
/// GCD queue assertion kills the process with SIGTRAP on the FIRST key
/// press (crash reports `string_from_code` → `TSMGetInputSourceProperty`
/// → `dispatch_assert_queue_fail`). Until rdev fixes that, non-Windows
/// builds skip the hook entirely; the configured global combo
/// (default `Ctrl+;`, the always-registered tauri-plugin-global-shortcut
/// path) remains the summon on every platform. The `DoubleTap` state
/// machine and its tests stay platform-independent.
#[cfg(windows)]
pub(crate) fn spawn(app: tauri::AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("synso-double-ctrl".into())
        .spawn(move || {
            let mut detector = DoubleTap::new();
            let mut ctrl_down = false; // ignore OS auto-repeat: only PRESS transitions
            if let Err(err) = rdev::listen(move |event: Event| {
                if let EventType::KeyPress(key) = event.event_type {
                    let tap_key = match key {
                        Key::ControlLeft | Key::ControlRight => {
                            if ctrl_down {
                                return; // auto-repeat while held
                            }
                            ctrl_down = true;
                            TapKey::Ctrl
                        }
                        _ => {
                            ctrl_down = false;
                            TapKey::Other
                        }
                    };
                    if detector.feed(tap_key, Instant::now()) {
                        let _ = tx.send(());
                    }
                } else if let EventType::KeyRelease(key) = event.event_type {
                    if matches!(key, Key::ControlLeft | Key::ControlRight) {
                        ctrl_down = false;
                    }
                }
            }) {
                log::warn!("synso: double-Ctrl hook stopped: {err:?}");
            }
        })
        .inspect_err(|err| log::warn!("synso: double-Ctrl thread failed to spawn: {err}"))
        .ok();
    // The consumer: a normal thread calling into Tauri (never the hook).
    std::thread::Builder::new()
        .name("synso-double-ctrl-fire".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                // Only summon when the quick window is hidden — a double
                // Ctrl while it is open (e.g. typing) must not re-show it.
                let Some(win) = app.get_webview_window(quick::LABEL) else {
                    continue;
                };
                if !win.is_visible().unwrap_or(true) {
                    quick::show(&app);
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_ctrl_within_window_fires() {
        let t0 = Instant::now();
        let mut d = DoubleTap::new();
        assert!(!d.feed(TapKey::Ctrl, t0));
        assert!(d.feed(TapKey::Ctrl, t0 + Duration::from_millis(300)));
    }

    #[test]
    fn slow_second_tap_does_not_fire() {
        let t0 = Instant::now();
        let mut d = DoubleTap::new();
        assert!(!d.feed(TapKey::Ctrl, t0));
        assert!(
            !d.feed(TapKey::Ctrl, t0 + Duration::from_millis(900)),
            "outside the 500ms window this is a fresh first tap"
        );
        // ...and the NEXT quick tap completes a new double.
        assert!(d.feed(TapKey::Ctrl, t0 + Duration::from_millis(1100)));
    }

    #[test]
    fn any_other_key_breaks_the_run() {
        let t0 = Instant::now();
        let mut d = DoubleTap::new();
        assert!(!d.feed(TapKey::Ctrl, t0));
        assert!(!d.feed(TapKey::Other, t0 + Duration::from_millis(100)));
        assert!(
            !d.feed(TapKey::Ctrl, t0 + Duration::from_millis(200)),
            "Ctrl, X, Ctrl is not a double tap"
        );
    }

    #[test]
    fn third_tap_after_fire_is_a_fresh_start() {
        let t0 = Instant::now();
        let mut d = DoubleTap::new();
        assert!(!d.feed(TapKey::Ctrl, t0));
        assert!(d.feed(TapKey::Ctrl, t0 + Duration::from_millis(200)));
        assert!(
            !d.feed(TapKey::Ctrl, t0 + Duration::from_millis(300)),
            "the tap that fired resets the run"
        );
    }
}
