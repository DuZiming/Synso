//! EngineEvent forwarding: a background thread drains the engine's
//! subscriber channel and re-emits every event to the webview via
//! `AppHandle::emit`.
//!
//! Naming conventions (Task 13 ruling, documented here once): event NAMES
//! are kebab-case (`scan-progress`, `scan-finished`, `scan-failed`,
//! `dedup-progress`, `dedup-finished`, `fs-changed`); every payload uses
//! **snake_case keys**, matching the facade types' Rust field names — the
//! same convention `engine_stats` & co. return over IPC, so the frontend
//! maps Rust data 1:1 instead of tracking two spellings. Dedup phases
//! serialize as snake_case strings ("idle", "grouping", "hashing_first",
//! "hashing_full", "done").

use crossbeam_channel::Receiver;
use serde::Serialize;
use synso_core::{DedupPhase, EngineEvent};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum PhasePayload {
    Idle,
    Grouping,
    HashingFirst,
    HashingFull,
    Done,
}

impl From<DedupPhase> for PhasePayload {
    fn from(phase: DedupPhase) -> Self {
        match phase {
            DedupPhase::Idle => PhasePayload::Idle,
            DedupPhase::Grouping => PhasePayload::Grouping,
            DedupPhase::HashingFirst => PhasePayload::HashingFirst,
            DedupPhase::HashingFull => PhasePayload::HashingFull,
            DedupPhase::Done => PhasePayload::Done,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ScanProgressPayload {
    done: u64,
    total: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ScanFinishedPayload {
    files: u64,
    dirs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ScanFailedPayload {
    errors: u64,
    message: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct DedupProgressPayload {
    phase: PhasePayload,
    done: u64,
    total: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct DedupFinishedPayload {
    groups: u64,
}

/// Spawns the forwarder thread: `events` is drained until the engine
/// handle (and thus the channel's sender side) is dropped. A webview
/// that already navigated away makes `emit` fail; that batch is logged
/// and dropped, never fatal.
pub fn spawn_forwarder(app: AppHandle, events: Receiver<EngineEvent>) {
    let spawned = std::thread::Builder::new()
        .name("synso-event-forwarder".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                let outcome = match &event {
                    EngineEvent::ScanProgress { done, total } => app.emit(
                        "scan-progress",
                        ScanProgressPayload {
                            done: *done,
                            total: *total,
                        },
                    ),
                    EngineEvent::ScanFinished { files, dirs } => app.emit(
                        "scan-finished",
                        ScanFinishedPayload {
                            files: *files,
                            dirs: *dirs,
                        },
                    ),
                    EngineEvent::ScanFailed { errors, message } => app.emit(
                        "scan-failed",
                        ScanFailedPayload {
                            errors: *errors,
                            message: message.clone(),
                        },
                    ),
                    // Pure refresh hint, no payload.
                    EngineEvent::FsChanged => app.emit("fs-changed", ()),
                    EngineEvent::DedupProgress { phase, done, total } => app.emit(
                        "dedup-progress",
                        DedupProgressPayload {
                            phase: (*phase).into(),
                            done: *done,
                            total: *total,
                        },
                    ),
                    EngineEvent::DedupFinished { groups } => {
                        app.emit("dedup-finished", DedupFinishedPayload { groups: *groups })
                    }
                };
                if let Err(err) = outcome {
                    log::warn!("synso: failed to emit {event:?} to the webview: {err}");
                }
            }
        });
    if let Err(err) = spawned {
        log::error!("synso: failed to spawn the event forwarder: {err}");
    }
}
