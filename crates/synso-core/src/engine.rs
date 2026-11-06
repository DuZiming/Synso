//! Engine facade: owns the SQLite store and the hot index, runs full scans
//! on a dedicated background thread, and fans progress/finish events out
//! to every subscriber.
//!
//! Concurrency shape: [`Store`] sits behind `Arc<Mutex<_>>` (rusqlite
//! connections are not `Sync`); the hot index behind `Arc<RwLock<_>>` so
//! [`Engine::stats`] can read it while a scan runs. A scan thread purges
//! each root's index subtree (rescan reconciliation: files deleted while
//! the app was closed must not linger), walks the roots, reloads the hot
//! index wholesale and only then emits [`EngineEvent::ScanFinished`]. The
//! hot index is always swapped while the store mutex is held, so a scan's
//! fresh reload can never be overwritten by a stale loader snapshot. One
//! scan at a time: [`Engine::start_full_scan`] is a no-op while a scan is
//! running; the slot is released by a drop guard, so even a panicked scan
//! cannot wedge the engine.
//!
//! Failure semantics: a scan that aborts with a store error emits
//! [`EngineEvent::ScanFailed`] instead of `ScanFinished` and does NOT swap
//! the hot index — searches keep the last known-good snapshot while the
//! store stays partial (its roots were already purged and only partially
//! re-walked); the store self-heals on the next successful rescan, which
//! purges and rebuilds each root from disk truth.
//!
//! Lock poisoning: every engine lock guards data whose invariants survive
//! a panic — the SQLite store (each upsert commits atomically, so a
//! half-finished scan leaves committed batches the next rescan reconciles),
//! the hot index (a rebuildable cache) and the subscriber list (a plain
//! `Vec`). Locks are therefore taken poison-tolerantly (see
//! [`lock_unpoisoned`]), which also keeps this crate free of
//! `unwrap`/`expect` in library code.
//!
//! Live updates (Task 8): [`Engine::start_watcher`] starts the filesystem
//! watcher (see [`crate::watcher`]) on the effective scan roots. Its
//! debounced batches apply under the same store mutex with the same
//! store -> hot ordering as a scan, so watcher edits and scan reloads
//! serialize; events that arrive while a scan holds the mutex queue in the
//! watcher's channel and apply right after — nothing is dropped.
//!
//! Dedup (Task 9, four-gate pipeline since the Task 7 swap):
//! [`Engine::dedup_start`] runs the staged pipeline (see [`crate::dedup`])
//! on its own thread — one run at a time, the slot claimed by CAS and
//! released by the same drop-guard pattern as the scan slot. Hashing runs
//! entirely OUTSIDE the store lock; only the candidate load and short
//! batched hash persists take it. The run is cancellable
//! ([`Engine::dedup_cancel`]): the pipeline stops at its next checkpoint,
//! keeps what it computed, reports `Cancelled` — and the engine then keeps
//! serving the last completed run's groups. The settings' `dedup_gear`
//! ([`crate::settings::DedupGear`]) picks the gates' worker count, with a
//! below-normal thread priority for the Gentle gear. Dedup is
//! MANUAL-only (user requirement): scans never kick off hashing — the
//! dedup view's 启动分析 button is the only trigger.
//! [`Engine::resolve_absolute`] maps index-relative paths back to real
//! disk paths — hashing needs them, and trash (Task 10) and reveal
//! (Task 15) reuse the same mapping.
//!
//! Heat persistence (v1.4): the tier cache's heat map is snapshotted
//! into the store's `heat` table every [`HEAT_PERSIST_INTERVAL`] by the
//! `synso-heat-persist` thread, after every rescan, and best-effort on
//! the quit path ([`Engine::persist_heat_on_exit`] — a mid-scan exit
//! SKIPS rather than blocking the main thread on a busy store lock).
//! At startup the `synso-heat-preheat` thread replays that snapshot
//! into the fresh tier cache with wall-clock offline decay applied, so
//! yesterday's hot files do not re-earn their heat from zero.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::dedup::{self, DedupPage, DedupState, DupeGroup, StoredGroup};
use crate::dimensions::{ColdReader, FileDetail};
use crate::hotindex::HotIndex;
use crate::model::{FileId, ParsedQuery};
use crate::query::{self, SearchResult};
use crate::scanner::{self, effective_roots, root_segment, ScanOptions};
use crate::settings::{self, DedupGear, Settings};
use crate::sizeagg::{self, SizeNode};
use crate::store::Store;
use crate::tiers::{self, TierCache};
use crate::trash;
use crate::volumes::{match_volume, VolumeInfo};
use crate::watcher::{self, Watcher};
use crate::Result;

/// The Gentle gear's thread hint (Task 7): the dedup thread itself runs
/// below normal priority, so hashing stays out of everything else's way.
/// Scope: this covers the orchestration thread (candidate load, gate①'s
/// histogram, the merges/persists between gates). The gates' internal
/// worker threads (dedup.rs's scope threads) do NOT inherit it — Windows
/// priorities are per-thread and std spawns them at normal priority — an
/// accepted limitation: the heavy streaming reads those workers do remain
/// I/O-bound, and lowering them would need priority plumbing through
/// every gate spawn.
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};

/// What the engine scans and what it skips. Empty `volumes` means "all
/// fixed local volumes" (`crate::volumes::fixed_volumes`); `exclusions`
/// are globset patterns as documented on `crate::scanner::ScanOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineOptions {
    pub volumes: Vec<String>,
    pub exclusions: Vec<String>,
}

/// Phase of a dedup run (Task 9). The engine walks `Idle -> Grouping ->
/// HashingFirst -> Sampling -> HashingFull -> Done` (intermediate stages
/// are skipped when they have no work); `Grouping` reappears as the
/// in-progress phase of every new run. `Sampling` is gate③'s phase
/// between the first probe and the full read. A cancelled run
/// ([`Engine::dedup_cancel`]) ends at `Cancelled` instead of `Done` —
/// its final progress report re-carries the last `(done, total)`, no
/// `DedupFinished` follows, and the served groups stay the last completed
/// run's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
// JSON bridges (the app's IPC) spell phases snake_case — the same
// convention the app's `dedup-progress` event payload uses — so a
// frontend sees one spelling everywhere.
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DedupPhase {
    #[default]
    Idle,
    Grouping,
    HashingFirst,
    /// Gate③'s sampling pass (pipeline rewrite, Task 6).
    Sampling,
    HashingFull,
    Done,
    /// A cancelled run's final state: the run stopped at a checkpoint and
    /// the last completed run's groups stay on duty.
    Cancelled,
}

/// Event broadcast to every subscriber that registered before the event
/// fired.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EngineEvent {
    /// Forwarded scanner progress (files done so far). `total` is always 0
    /// here: the walk total is unknown while walking.
    ScanProgress { done: u64, total: u64 },
    /// Emitted after a scan finished AND the hot index was reloaded from
    /// the store, so subscribers can trust `stats()` from this point on.
    ScanFinished { files: u64, dirs: u64 },
    /// A scan aborted with a store error; no `ScanFinished` follows. The
    /// hot index was NOT swapped (searches keep the last known-good
    /// snapshot) and the store stays partial until the next successful
    /// rescan. `errors` is 0 for now — the walker aborts on the first
    /// store error without returning partial counts — and `message`
    /// carries the cause.
    ScanFailed { errors: u64, message: String },
    /// The filesystem watcher applied a debounced batch of store + hot-index
    /// updates (Task 8). Pure UI refresh hint: the index is already
    /// consistent when this fires, so subscribers can just re-query. Carries
    /// no payload — polls of `stats()`/`search()` are the source of truth.
    FsChanged,
    /// Dedup pipeline progress (Task 9): `done`/`total` count the records
    /// of the current stage, emitted at every stage change and every 500
    /// records within one. The final event carries `phase: Done` with
    /// `done == total ==` the hash computations the run performed — or,
    /// for a cancelled run ([`Engine::dedup_cancel`]), ONE final event
    /// with `phase: Cancelled` re-carrying the last `(done, total)`
    /// (no `DedupFinished` follows; the served groups stay the previous
    /// run's).
    DedupProgress {
        phase: DedupPhase,
        done: u64,
        total: u64,
    },
    /// A dedup run finished; `groups` is its group count. Fired after the
    /// state machine reached `Done`, so [`Engine::dedup_groups`] already
    /// serves the fresh groups when this arrives.
    DedupFinished { groups: u64 },
}

/// Point-in-time engine state, read from the hot index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EngineStats {
    pub files: u64,
    pub dirs: u64,
    pub warm: bool,
    pub interned_bytes: u64,
}

/// Physical-capacity figures for a pane root that names exactly one
/// volume (see [`Engine::volume_overlay`]); the capacity map draws its
/// root ring against these so the proportions are the drive's real ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VolumeOverlay {
    /// `total - available` as the OS reports it — what Explorer would
    /// call the drive's used space.
    pub used_bytes: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// The subtree total the hot index holds for this root.
    pub indexed_bytes: u64,
    /// `used - indexed`, clamped at zero: bytes on the drive the index
    /// does not account for (excluded paths, recycle bin, accounting
    /// skew).
    pub unindexed_bytes: u64,
}

/// Facade owning the store and the hot index (see module docs). `Engine`
/// is `Send + Sync`, so the app layer can hold it in an `Arc`.
pub struct Engine {
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    /// What the engine scans and skips. Behind a mutex so
    /// [`Engine::set_scan_options`] can swap the scope live (the scan
    /// thread clones it at spawn, so a running scan keeps its own
    /// snapshot — same discipline as the watcher restart).
    scan_opts: Mutex<ScanOptions>,
    scanning: Arc<AtomicBool>,
    /// Database location, kept so the watcher (Task 8) can filter out the
    /// store's own writes.
    db_path: std::path::PathBuf,
    /// Started lazily by [`Engine::start_watcher`]; dropping the engine
    /// drops the handle, which stops and joins the debounce thread.
    watcher: Mutex<Option<Watcher>>,
    /// Dedup state machine + worker plumbing (Task 9). Threads and the
    /// auto-start timer own `Arc`s of this, never of the engine.
    dedup: Arc<DedupShared>,
    /// In-memory copy of the persisted settings, preheated in [`open`]
    /// and refreshed by [`Engine::set_settings`]. `settings()` serves
    /// this — NEVER the store mutex, which the hot-index load and full
    /// scans hold for their whole duration (a store-lock read froze the
    /// app's main thread at startup: the v1 "not responding" bug).
    settings_cache: Mutex<Settings>,
    /// The dimension cache (v1.2): hot → staging tiers keyed by file id.
    /// `None` until first use so opening an engine costs nothing extra.
    tiers: Arc<Mutex<TierCache<FileId, FileDetail>>>,
    /// The dedicated read-only WAL connection for cold dimension reads —
    /// deliberately NOT behind the store mutex (see `dimensions` docs).
    cold: std::sync::Mutex<ColdReader>,
    /// The send half of the persist thread's keepalive channel. Held
    /// (never sent on) purely so the channel stays connected while the
    /// engine lives: dropping the engine drops the sender, the thread's
    /// `recv_timeout` turns `Disconnected`, and it exits without a
    /// join. Behind a `Mutex` because `mpsc::Sender` is `Send` but not
    /// `Sync`, and the engine must stay `Sync`.
    // Keepalive-only field: the sender's Drop IS its function, so no
    // code ever reads it — unlike the two allows Task 3 removed (real
    // APIs that gained callers), this one has no reader by design.
    #[allow(dead_code)]
    heat_keepalive: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

/// State shared between the engine, the dedup worker thread and the
/// auto-start timer (Task 9).
struct DedupShared {
    /// One dedup run at a time: CAS claim, released by a drop guard.
    running: AtomicBool,
    /// Set by [`Engine::dedup_cancel`] while a run is going; the gates
    /// check it at file boundaries (gate④ also at every 64KB read step)
    /// and the run stops at the next checkpoint, reporting `Cancelled`
    /// with an empty group list. Cleared by [`launch_dedup`] before each
    /// spawn, so a cancelled predecessor cannot pre-arm its successor.
    cancel: AtomicBool,
    /// The state machine behind `dedup_state()` plus the last completed
    /// run's groups.
    book: Mutex<DedupBookkeeping>,
    /// Store access for the snapshot read and short persist batches.
    store: Arc<Mutex<Store>>,
    /// Effective scan roots (plain spellings), precomputed at open so
    /// path resolution never re-enumerates volumes per hashed file.
    /// Behind an RwLock so [`Engine::set_scan_options`] keeps the mapping
    /// in step with the live scope (a stale entry would leave search hits
    /// that resolve/open/trash cannot map back to disk); a dedup run
    /// snapshots it once at start.
    roots: RwLock<Vec<String>>,
    /// Event fan-out (DedupProgress / DedupFinished).
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
}

/// The bookkeeping behind [`Engine::dedup_state`] / [`Engine::dedup_groups`].
struct DedupBookkeeping {
    state: DedupState,
    groups: Arc<Vec<StoredGroup>>,
}

impl Engine {
    /// Opens (creating if needed) the index database at `db_path` and
    /// starts the background full load of the hot index; `stats().warm`
    /// flips to `true` when that load completes.
    pub fn open(db_path: &Path, opts: EngineOptions) -> Result<Engine> {
        let store = Arc::new(Mutex::new(Store::open(db_path)?));
        // Preheat the settings cache BEFORE the hot-index load spawns:
        // at this instant the store mutex is uncontended (the load and
        // scans are what hold it for seconds later), so this read costs
        // milliseconds — and every later `settings()` is lock-free.
        let initial_settings = settings::load(&lock_unpoisoned(&store));
        // Task 9: one-time purge of already-indexed recycle-bin rows,
        // under the store lock and BEFORE the hot-index load spawns below,
        // so purged rows never materialize in searches. Failure keeps the
        // `recycle_purge_done` marker unset — the next launch retries
        // (the purge is idempotent); meanwhile the matcher's hard rule
        // and the dedup candidate gate keep recycle-bin content out of
        // new results.
        {
            let mut store = lock_unpoisoned(&store);
            purge_recycle_bin_once(&mut store);
        }
        let hot = Arc::new(RwLock::new(HotIndex::cold()));
        let scan_opts = ScanOptions {
            roots: opts.volumes,
            exclusions: opts.exclusions,
        };
        let subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let dedup = Arc::new(DedupShared {
            running: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            book: Mutex::new(DedupBookkeeping {
                state: DedupState::default(),
                groups: Arc::new(Vec::new()),
            }),
            store: Arc::clone(&store),
            roots: RwLock::new(effective_roots(&scan_opts.roots)),
            subscribers: Arc::clone(&subscribers),
        });
        // The persist thread's keepalive: the sender lives in the
        // engine (heat_keepalive), the receiver parks in recv_timeout —
        // engine drop disconnects the pair and the thread exits.
        let (heat_tx, heat_rx) = std::sync::mpsc::channel::<()>();
        let engine = Engine {
            store: Arc::clone(&store),
            hot: hot.clone(),
            subscribers,
            scan_opts: Mutex::new(scan_opts),
            scanning: Arc::new(AtomicBool::new(false)),
            db_path: db_path.to_path_buf(),
            watcher: Mutex::new(None),
            dedup,
            settings_cache: Mutex::new(initial_settings.clone()),
            tiers: Arc::new(Mutex::new(TierCache::new(initial_settings.memory_policy()))),
            cold: std::sync::Mutex::new(ColdReader::open(db_path)?),
            heat_keepalive: Mutex::new(Some(heat_tx)),
        };
        if let Err(err) = thread::Builder::new()
            .name("synso-hot-load".into())
            .spawn(move || reload_hot_index(&store, &hot))
        {
            log::error!("synso: failed to spawn the hot-index loader: {err}");
        }
        // Heat persistence (v1.4), after the hot-index loader: replay
        // the persisted snapshot into the fresh tier cache, then keep
        // snapshotting the heat map periodically. Both threads own Arc
        // clones (never the engine), open their own ColdReader
        // connections, and die with the engine — the preheat thread by
        // simply finishing, the persist thread via the keepalive
        // disconnect.
        let preheat_tiers = Arc::clone(&engine.tiers);
        let preheat_db = engine.db_path.clone();
        if let Err(err) = thread::Builder::new()
            .name("synso-heat-preheat".into())
            .spawn(move || preheat_thread_side(&preheat_tiers, &preheat_db))
        {
            log::error!("synso: failed to spawn the heat-preheat thread: {err}");
        }
        let persist_tiers = Arc::clone(&engine.tiers);
        let persist_store = Arc::clone(&engine.store);
        if let Err(err) = thread::Builder::new()
            .name("synso-heat-persist".into())
            .spawn(move || {
                loop {
                    match heat_rx.recv_timeout(HEAT_PERSIST_INTERVAL) {
                        // The keepalive exists to hold the connection
                        // open, never to signal; a wake just restarts
                        // the wait.
                        Ok(()) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let mut store = lock_unpoisoned(&persist_store);
                            persist_heat_into(&mut store, &persist_tiers);
                        }
                        // Engine dropped (the keepalive sender with it).
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
        {
            log::error!("synso: failed to spawn the heat-persist thread: {err}");
        }
        Ok(engine)
    }

    /// Registers a new unbounded event receiver. Events fired before
    /// subscribing are not delivered.
    pub fn subscribe(&self) -> Receiver<EngineEvent> {
        let (tx, rx) = unbounded();
        lock_unpoisoned(&self.subscribers).push(tx);
        rx
    }

    /// Starts a full scan on a background thread. One scan at a time:
    /// calling this while a scan is running is a no-op. Per root, the
    /// root's index subtree is purged before the walk (rescan
    /// reconciliation). On success the hot index is reloaded wholesale
    /// from the store and `ScanFinished` is emitted; on failure
    /// `ScanFailed` is emitted instead and the hot index keeps serving the
    /// last known-good snapshot.
    pub fn start_full_scan(&self) {
        // Claim the scan slot first: the compare-exchange makes a
        // concurrent second call a no-op.
        if self
            .scanning
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // v1.2→v1.4: a companion listener flushes the dimension caches when
        // the scan lands (every walked row was re-stamped, so cached
        // details are suspect), reinserts the ids the flushed hot tier
        // held WITH their scores (a rescan no longer zeroes heat), and
        // persists the fresh snapshot (a rescan is one of heat
        // persistence's triggers). Subscribed BEFORE the scan spawns
        // (no missed terminal event); exits after the first one.
        let flush_rx = self.subscribe();
        let flush_tiers = Arc::clone(&self.tiers);
        let flush_store = Arc::clone(&self.store);
        let flush_db = self.db_path.clone();
        let _ = thread::Builder::new()
            .name("synso-dim-flush".into())
            .spawn(move || {
                loop {
                    // Blocking recv — the review-fix: recv_timeout conflated
                    // Timeout with Disconnected, so a scan silent >600s
                    // (stalled root mid-walk) silently killed the flush.
                    // recv() only returns Err on a real disconnect (every
                    // sender dropped with the engine), so the companion
                    // always outlives the scan it watches.
                    match flush_rx.recv() {
                        Ok(EngineEvent::ScanFinished { .. })
                        | Ok(EngineEvent::ScanFailed { .. }) => break,
                        Ok(_) => continue,
                        Err(_) => return, // sender gone: engine dropped
                    }
                }
                rescan_flush_thread_side(&flush_tiers, &flush_store, &flush_db);
            });
        let spawn = thread::Builder::new().name("synso-scan".into()).spawn({
            let store = self.store.clone();
            let hot = self.hot.clone();
            let subscribers = self.subscribers.clone();
            let scanning = self.scanning.clone();
            // One snapshot of the LIVE scope (see `set_scan_options`): a
            // scan already walking keeps its own opts, so a scope swap
            // mid-scan takes effect on the next scan, not this one.
            let opts = lock_unpoisoned(&self.scan_opts).clone();
            let dedup = Arc::clone(&self.dedup);
            move || scan_cycle(store, hot, subscribers, scanning, opts, dedup)
        });
        if let Err(err) = spawn {
            log::error!("synso: failed to spawn the scan thread: {err}");
            self.scanning.store(false, Ordering::Release);
        }
    }

    /// Whether a full scan is currently running. The app shell's quit
    /// flow uses it to stand a confirmation in front of interrupting a
    /// scan (Task 19's tray 退出 dialog).
    pub fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::Acquire)
    }

    /// Starts the filesystem watcher (see `crate::watcher`): recursive
    /// `notify` watches on the effective scan roots, events merged into
    /// 500ms quiet-window batches, each batch applied incrementally to the
    /// store and the hot index, then [`EngineEvent::FsChanged`]
    /// broadcast. A watcher already running makes this a no-op. The
    /// watcher is NOT started by [`Engine::open`] on purpose: scans,
    /// searches and events stay fully deterministic for embedders that
    /// never need live updates.
    pub fn start_watcher(&self) -> Result<()> {
        let mut slot = lock_unpoisoned(&self.watcher);
        if slot.is_some() {
            return Ok(());
        }
        let (roots, exclusions) = {
            let opts = lock_unpoisoned(&self.scan_opts);
            (effective_roots(&opts.roots), opts.exclusions.clone())
        };
        // Capture ONLY the tiers Arc (the engine itself is not Sync —
        // the cold reader's connection isn't shareable, and the closure
        // doesn't need it).
        let tiers_for_watcher = Arc::clone(&self.tiers);
        let watcher = Watcher::start(
            &roots,
            &exclusions,
            &self.db_path,
            Arc::clone(&self.store),
            Arc::clone(&self.hot),
            Arc::clone(&self.subscribers),
            Arc::new(move |ids: &[FileId]| {
                let mut tiers = lock_unpoisoned(&tiers_for_watcher);
                for id in ids {
                    tiers.invalidate(id);
                }
            }),
        )?;
        *slot = Some(watcher);
        Ok(())
    }

    /// Replaces the scan scope LIVE — the engine half of making
    /// `Settings.volumes` / `Settings.exclusions` real (the app shell's
    /// settings flow calls this on a scope change; `set_settings` only
    /// persists). `volumes` empty = all fixed local volumes (the standing
    /// semantics); `exclusions` are the scan/watcher glob patterns.
    ///
    /// Under the store lock: swap the options, purge the index subtrees
    /// of roots that LEFT the scope (out-of-scope rows must not linger in
    /// search — the same per-root purge a scan performs for its roots),
    /// reload the hot index wholesale (the scan's own swap discipline),
    /// and update the dedup root mapping. A watcher that was running
    /// restarts on the new roots + matcher; an engine that never started
    /// one stays watcher-less (the explicit-start contract). A bad
    /// pattern fails BEFORE any mutation (the matcher compiles first), so
    /// the caller can surface it and keep the previous scope untouched.
    ///
    /// Blocking: the store lock serializes against an in-flight scan's
    /// whole walk (exactly like a watcher batch), so this returns once
    /// any running scan finishes; the newly added roots' rows appear on
    /// the NEXT scan of them. Removed-root rows are gone immediately.
    pub fn set_scan_options(&self, volumes: Vec<String>, exclusions: Vec<String>) -> Result<()> {
        let new_opts = ScanOptions {
            roots: volumes,
            exclusions,
        };
        // Validate before mutating: a bad glob is a caller error.
        scanner::build_matcher(&new_opts.exclusions)?;

        // Stop the old watcher BEFORE the store lock: its apply thread
        // takes that lock, so joining while holding it could deadlock.
        // The taken `Watcher` drops (stop + join) right away, lock-free.
        let had_watcher = lock_unpoisoned(&self.watcher).take().is_some();

        let removed = {
            let mut scan_opts = lock_unpoisoned(&self.scan_opts);
            let old_effective = effective_roots(&scan_opts.roots);
            let new_effective = effective_roots(&new_opts.roots);
            let removed: Vec<String> = old_effective
                .iter()
                .filter(|root| !new_effective.contains(root))
                .cloned()
                .collect();
            let mut store = lock_unpoisoned(&self.store);
            for root in &removed {
                let segment = root_segment(Path::new(root));
                if let Err(err) = store.delete_subtree(&[segment.as_str()]) {
                    log::error!("synso: failed to purge the out-of-scope root {root:?}: {err}");
                }
            }
            // Keep the dedup path mapping in step (see DedupShared::roots).
            *write_unpoisoned(&self.dedup.roots) = new_effective;
            *scan_opts = new_opts;
            reload_hot_index_locked(&store, &self.hot);
            removed
        };
        if !removed.is_empty() {
            log::info!(
                "synso: scan scope changed; purged {} out-of-scope root(s)",
                removed.len()
            );
        }

        if had_watcher {
            self.start_watcher()?;
        }
        Ok(())
    }

    /// Snapshot of hot-index state.
    pub fn stats(&self) -> EngineStats {
        let hot = read_unpoisoned(&self.hot);
        EngineStats {
            files: hot.file_count(),
            dirs: hot.dir_count(),
            warm: hot.is_warm(),
            interned_bytes: hot.interned_bytes(),
        }
    }

    /// Per-structure memory breakdown of the hot index (v1.1 Task 6
    /// diagnostics — see [`crate::hotindex::HotIndex::mem_breakdown`]).
    pub fn mem_breakdown(&self) -> crate::hotindex::MemBreakdown {
        read_unpoisoned(&self.hot).mem_breakdown()
    }

    // ---------------- v1.2: dimension tiers ------------------------------

    /// The pilot dimension: per-file detail (ctime/atime/dedup hashes)
    /// through the tier ladder — hot → staging → the read-only WAL
    /// connection — cached per the live memory policy. Unknown ids yield
    /// no row (deleted or never indexed).
    pub fn file_detail(&self, id: FileId) -> Option<FileDetail> {
        {
            let mut tiers = lock_unpoisoned(&self.tiers);
            if let Some(hit) = tiers.get_hot(&id) {
                return Some(hit.clone());
            }
            if let Some(hit) = tiers.get_staging(&id) {
                // A staging hit builds heat; crossing the threshold moves
                // the staged value into hot (no refetch).
                tiers.record_hit(&id);
                tiers.promote_from_staging(&id);
                return Some(hit);
            }
        }
        // Cold: read storage on the dedicated connection (never the store
        // mutex), then place into staging.
        let fetched = lock_unpoisoned(&self.cold)
            .file_details(&[id])
            .ok()?
            .pop()?;
        let mut tiers = lock_unpoisoned(&self.tiers);
        tiers.record_hit(&id);
        tiers.put_staging(id, fetched.clone(), fetched.cached_bytes());
        Some(fetched)
    }

    /// A page of details (≤100 ids per call by convention). Uncached ids
    /// are fetched in one batched cold read; every returned row lands in
    /// staging.
    pub fn file_details(&self, ids: &[FileId]) -> Vec<FileDetail> {
        let mut result: Vec<FileDetail> = Vec::with_capacity(ids.len());
        let mut misses: Vec<FileId> = Vec::new();
        {
            let mut tiers = lock_unpoisoned(&self.tiers);
            for &id in ids {
                if let Some(hit) = tiers.get_hot(&id) {
                    result.push(hit.clone());
                } else if let Some(hit) = tiers.get_staging(&id) {
                    tiers.record_hit(&id);
                    result.push(hit);
                } else {
                    misses.push(id);
                }
            }
        }
        if !misses.is_empty() {
            if let Ok(fetched) = lock_unpoisoned(&self.cold).file_details(&misses) {
                let mut tiers = lock_unpoisoned(&self.tiers);
                for detail in &fetched {
                    tiers.record_hit(&detail.id);
                    tiers.put_staging(detail.id, detail.clone(), detail.cached_bytes());
                }
                result.extend(fetched);
            }
        }
        result
    }

    /// Tier observability for the settings dashboard and the memlog line.
    pub fn tier_stats(&self) -> tiers::TierStats {
        lock_unpoisoned(&self.tiers).stats()
    }

    /// Live-applies the memory policy (settings UI path).
    pub fn apply_memory_policy(&self, policy: tiers::MemoryPolicy) {
        lock_unpoisoned(&self.tiers).set_policy(policy);
    }

    /// Drops the given ids' tier entries (watcher invalidation hook: an
    /// updated row's cached detail is stale by definition).
    pub fn invalidate_dimensions(&self, ids: &[FileId]) {
        let mut tiers = lock_unpoisoned(&self.tiers);
        for id in ids {
            tiers.invalidate(id);
        }
    }

    /// The synchronous/manual-entry twin of [`rescan_flush_thread_side`]
    /// — the same flush → reinsert → persist sequence, run through the
    /// engine's own locks; tests and any embedder driving a flush by
    /// hand call this, while the real rescan completion runs the
    /// companion-thread side. Flushes every dimension cache entry and,
    /// when preheating is on, reloads the hot tier with the ids the
    /// flushed hot tier held — each WITH its score (a flush no longer
    /// zeroes heat) — then persists the fresh snapshot. When the
    /// carry-back cold read fails, the persist is SKIPPED: overwriting
    /// the last good snapshot with an empty table would lose heat the
    /// next session could still restore.
    pub fn flush_dimensions_and_preheat(&self) {
        let Some(scored) = flush_snapshot(&mut lock_unpoisoned(&self.tiers)) else {
            return; // preload off: nothing to carry, nothing to persist
        };
        if !scored.is_empty() {
            let cold = lock_unpoisoned(&self.cold);
            let mut tiers = lock_unpoisoned(&self.tiers);
            if !reinsert_with_scores(&mut tiers, &cold, &scored) {
                return; // failed carry: persisting would wipe the table
            }
        }
        self.persist_heat();
    }

    /// Snapshots the tier cache's heat into the store's `heat` table —
    /// the periodic thread's and the rescan flush's shared path. Takes
    /// the store's BLOCKING lock (callers on the main thread at quit
    /// time use [`Engine::persist_heat_on_exit`] instead); everything
    /// inside is best-effort, errors only logged.
    pub fn persist_heat(&self) {
        let mut store = lock_unpoisoned(&self.store);
        persist_heat_into(&mut store, &self.tiers);
    }

    /// The quit-path persist: strictly best-effort. A mid-scan quit
    /// finds the store mutex held for the scan's whole walk — blocking
    /// the main thread there would hang the exit, so a busy OR poisoned
    /// lock skips the snapshot outright (the table keeps its last
    /// periodic/rescan snapshot; at worst one interval of heat is
    /// re-earned next session).
    pub fn persist_heat_on_exit(&self) {
        match self.store.try_lock() {
            Ok(mut store) => persist_heat_into(&mut store, &self.tiers),
            Err(std::sync::TryLockError::WouldBlock) => {
                log::debug!("synso: exit heat persist skipped — the store lock is busy");
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                log::debug!("synso: exit heat persist skipped — the store lock is poisoned");
            }
        }
    }

    // ---------------- Task 13: user settings -----------------------------

    /// The persisted user settings (see [`crate::settings`]), served from
    /// the in-engine cache — this call NEVER touches the store mutex, so
    /// it stays fast while a hot-index load or full scan holds it (the
    /// app's main thread reads the shortcut at startup through
    /// `quick::init`). The cache is preheated in [`Engine::open`] and
    /// refreshed by [`Engine::set_settings`]; missing fields read as
    /// their documented defaults.
    pub fn settings(&self) -> Settings {
        lock_unpoisoned(&self.settings_cache).clone()
    }

    /// Persists `settings` to the store's meta table and refreshes the
    /// in-memory cache in the same step (see [`Engine::settings`]), so
    /// the two can never disagree. Store errors surface to the caller
    /// and leave the cache untouched.
    pub fn set_settings(&self, settings: &Settings) -> Result<()> {
        settings::save(&lock_unpoisoned(&self.store), settings)?;
        *lock_unpoisoned(&self.settings_cache) = settings.clone();
        // The memory policy is live-appliable — new budgets evict to size
        // immediately (v1.2 settings UI path).
        lock_unpoisoned(&self.tiers).set_policy(settings.memory_policy());
        Ok(())
    }

    /// Case-insensitive substring search over the hot index's name arena
    /// (see `crate::query` for the full contract). `text` is first
    /// parsed by [`crate::parse_query`]: Everything-style modifier
    /// tokens (`ext:` / `size:` / `path:`) become dimension
    /// filters applied before ranking, and the remaining words are free
    /// text that must ALL match. Ranking: prefix > word-start >
    /// substring, a within-class boost for mtimes within the last 30
    /// days, then shorter path, then lexicographic path; an empty `text`
    /// lists the most recent entries by mtime (ties: path asc). `hits`
    /// covers `[offset, offset + limit)` of that ordering, `total` is the
    /// full match count. A cold index returns an empty result — this
    /// never panics.
    pub fn search(&self, text: &str, offset: u32, limit: u32) -> SearchResult {
        let parsed = query::parse_query(text);
        self.search_parsed(&parsed, offset, limit)
    }

    /// [`Engine::search`] over an already-parsed query (see
    /// [`crate::parse_query`]). The parsed dimension filters
    /// (`ext:` / `size:` / `path:`) apply as fast predicates
    /// BEFORE ranking and classification; ranking itself is unchanged.
    pub fn search_parsed(&self, q: &ParsedQuery, offset: u32, limit: u32) -> SearchResult {
        let hot = read_unpoisoned(&self.hot);
        query::run_parsed(&hot, q, offset, limit, unix_now())
    }

    // ---------------- Task 9: dedup state machine ------------------------

    /// Starts a dedup run on a background thread; a no-op while one is
    /// already running. The pipeline (see `crate::dedup`) is the four-gate
    /// chain — size, first-4KiB probe, sampling, full certification — over
    /// the streamed candidate snapshot; persisted hashes are reused, so
    /// repeat runs are incremental. The settings' `dedup_gear` picks the
    /// gates' worker count (and the Gentle gear's thread priority).
    /// `Idle -> Grouping` happens synchronously here; subscribers receive
    /// [`EngineEvent::DedupProgress`] / [`EngineEvent::DedupFinished`] and
    /// pollers read [`Engine::dedup_state`]. A run can be stopped with
    /// [`Engine::dedup_cancel`].
    pub fn dedup_start(&self) {
        launch_dedup(&self.dedup, Arc::clone(&self.hot), self.settings().dedup_gear);
    }

    /// Asks the running dedup run to stop at its next checkpoint (the
    /// gates check at file boundaries; gate④ additionally at every 64KB
    /// read step, so even a giant file aborts sub-second). The run keeps
    /// every hash it already computed, reports `Cancelled` as its final
    /// progress phase, and broadcasts NO `DedupFinished` — the engine
    /// keeps serving the last completed run's groups. Idempotent, and a
    /// no-op when no run is going: a cancel pressed while idle must not
    /// pre-arm the NEXT run ([`launch_dedup`] additionally clears the
    /// flag before each spawn, so the two guards agree).
    pub fn dedup_cancel(&self) {
        if !self.dedup.running.load(Ordering::Acquire) {
            return;
        }
        self.dedup.cancel.store(true, Ordering::Release);
    }

    /// Snapshot of the dedup state machine. While a run is going,
    /// `done`/`total` mirror the latest progress event; at
    /// [`DedupPhase::Done`] they equal the hash computations this run
    /// performed (0 = fully incremental); at [`DedupPhase::Cancelled`]
    /// they re-carry the last in-run report. `groups` is the last
    /// COMPLETED run's group count (a cancelled run keeps the previous
    /// one); `errors` counts files skipped because they could not be
    /// read, were locked by another handle, or are cloud placeholders —
    /// all "marked skipped, not grouped" per the design. The completion
    /// aggregates `alive_groups`/`wasted_bytes` are computed ONCE at that
    /// completion instant (an alive-scan over the fresh groups against
    /// the hot index of the moment) and cached here — this call stays a
    /// plain copy; a cancelled run carries the previous run's values and
    /// a fresh engine reports 0/0.
    pub fn dedup_state(&self) -> DedupState {
        lock_unpoisoned(&self.dedup.book).state
    }

    /// The last completed run's groups as a [`DedupPage`]: the
    /// `[offset, offset + limit)` slice of the size-descending order,
    /// members materialized as search-style [`crate::Hit`]s from the hot
    /// index, plus `total_alive` — the count of groups that still
    /// materialize (>= 2 surviving members) across ALL pages, scanned
    /// live against the current hot index (a cancelled run keeps serving
    /// the previous run's groups; files deleted since any run drop out).
    /// A group falling below two surviving members dissolves (its slot in
    /// the ordering disappears, the other groups keep their relative
    /// order) — the same rule the run-completion aggregates in
    /// [`Engine::dedup_state`] apply at their completion instant.
    pub fn dedup_groups(&self, offset: u32, limit: u32) -> DedupPage {
        // Snapshot the stored groups (cheap Arc clone) and release the
        // bookkeeping lock before touching the hot index — the engine's
        // locks never nest.
        let groups = Arc::clone(&lock_unpoisoned(&self.dedup.book).groups);
        let hot = read_unpoisoned(&self.hot);
        // First pass: alive-member count per group (`slot_of_id` presence,
        // the exact rule the materialization below applies). This is the
        // total's O(groups) scan the plan budgets per page-granularity
        // call — and it doubles as the dissolved-group filter, so skipped
        // groups never materialize their members at all.
        let alive_counts: Vec<u32> = groups
            .iter()
            .map(|group| {
                group
                    .members
                    .iter()
                    .filter(|id| hot.slot_of_id(**id).is_some())
                    .count() as u32
            })
            .collect();
        let total_alive = alive_counts.iter().filter(|&&alive| alive >= 2).count() as u32;
        let mut page = Vec::new();
        if limit > 0 {
            let mut skipped: u32 = 0;
            for (group, &alive) in groups.iter().zip(&alive_counts) {
                if alive < 2 {
                    continue;
                }
                if skipped < offset {
                    skipped += 1;
                    continue;
                }
                page.push(DupeGroup {
                    hash: group.hash,
                    size: group.size,
                    members: group
                        .members
                        .iter()
                        .filter_map(|id| hot.slot_of_id(*id).map(|slot| query::hit_of(&hot, slot)))
                        .collect(),
                });
                if page.len() == limit as usize {
                    break;
                }
            }
        }
        DedupPage {
            groups: page,
            total_alive,
        }
    }

    /// The absolute disk path of an index-relative path (root segment
    /// first, `MAIN_SEPARATOR`-joined — the spelling of
    /// [`crate::Hit::path`]), resolved by matching the path's first
    /// segment against the engine's effective scan roots. `None` when no
    /// root's segment matches. Hashing (Task 9), trash (Task 10) and
    /// reveal (Task 15) all key on this: index paths deliberately carry no
    /// drive prefix, so the engine owns the root mapping. Public: its
    /// consumers straddle the crate and app layers, like
    /// `crate::scanner::is_placeholder_attr`.
    pub fn resolve_absolute(&self, index_path: &str) -> Option<PathBuf> {
        dedup::resolve_absolute_path(&read_unpoisoned(&self.dedup.roots), index_path)
    }

    /// The live hot-index hit behind a `files` row id — the id → hit step
    /// of the reveal/open pipeline (Task 15). `None` when the id is dead
    /// (deleted) or unknown, mirroring the skip rule `delete_to_trash`
    /// applies; unlike trash, the app layer surfaces that as an error to
    /// the user. Pairs with [`Engine::resolve_absolute`], which turns the
    /// hit's index-relative path into the real disk path.
    pub fn hit_by_id(&self, id: FileId) -> Option<crate::Hit> {
        let hot = read_unpoisoned(&self.hot);
        hot.slot_of_id(id).map(|slot| query::hit_of(&hot, slot))
    }

    /// The `files` row id the index-relative `path` names (the `Hit::path`
    /// spelling; both separators accepted, directories included) — a
    /// generic path → id bridge (flame-chart nodes carry paths rather
    /// than row ids). `None` when the path matches nothing (unknown path,
    /// or the virtual root `""` — it names no single entry).
    pub fn file_id_of(&self, path: &str) -> Option<FileId> {
        let hot = read_unpoisoned(&self.hot);
        hot.id_of_path(path)
    }

    // ---------------- Task 11: directory size aggregation ----------------

    /// Directory-size tree for the flame chart (Task 17): the subtree
    /// behind `root` — an index-relative path in the `Hit::path` spelling
    /// — with every node's `size` summing its descendant files' bytes and
    /// `file_count` counting them (files included as leaf nodes). `""` is
    /// the virtual root: name "(all)", one child per scan-root segment. A
    /// path that names a FILE yields that file as a leaf; an unknown root
    /// yields `None`. Each level's `children` are sorted by size
    /// descending (ties: name ascending) and truncated to `max_children`,
    /// the overflow merged into one trailing "(other)" node (which counts
    /// against the budget); `depth` caps how many levels of children are
    /// materialized (0 = none). The UI calls this with (path, 2, 40)-
    /// shaped arguments. See `crate::sizeagg` for the full contract.
    ///
    /// Cost: O(dirs + files) time and scratch memory per call — one full
    /// post-order aggregation walk over the hot index under its read
    /// lock, deliberately NOT cached (invalidation would cost more than
    /// the walk; Task 20 measures).
    pub fn size_tree(&self, root: &str, depth: u8, max_children: u8) -> Option<SizeNode> {
        let hot = read_unpoisoned(&self.hot);
        sizeagg::build_tree(&hot, root, depth, max_children)
    }

    /// Dir-only `size_tree` for the flame chart: file leaves are never
    /// materialized (their bytes still shape every dir's aggregate, which
    /// is the taper the chart draws).
    pub fn size_tree_dirs(&self, root: &str, depth: u8, max_children: u8) -> Option<SizeNode> {
        let hot = read_unpoisoned(&self.hot);
        sizeagg::build_tree_dirs(&hot, root, depth, max_children)
    }

    /// Physical-capacity overlay for a pane root that names exactly one
    /// volume (see [`crate::volumes::match_volume`]); `None` for every
    /// other level. `used = total - available` straight from the OS, so
    /// the capacity map's ring can be honest about what Explorer reports,
    /// with the indexed-vs-physical gap as the「未索引」block.
    pub fn volume_overlay(&self, root: &str) -> Option<VolumeOverlay> {
        let volumes = crate::volumes::fixed_volumes();
        self.volume_overlay_in(root, &volumes)
    }

    /// [`Engine::volume_overlay`] over a caller-supplied volume list —
    /// the seam that keeps the arithmetic unit-testable without touching
    /// the host's real disks.
    fn volume_overlay_in(&self, root: &str, volumes: &[VolumeInfo]) -> Option<VolumeOverlay> {
        let volume = match_volume(root, volumes)?;
        let used = volume.total_bytes.saturating_sub(volume.available_bytes);
        // depth 0 = the root node alone, carrying the subtree total.
        let indexed = self
            .size_tree_dirs(root, 0, 0)
            .map_or(0u64, |node| node.size);
        Some(VolumeOverlay {
            used_bytes: used,
            total_bytes: volume.total_bytes,
            available_bytes: volume.available_bytes,
            indexed_bytes: indexed,
            // Sparse files / accounting skew can push the indexed sum
            // past physical usage — the chart must never draw a negative
            // block.
            unindexed_bytes: used.saturating_sub(indexed),
        })
    }

    // ---------------- Task 10: recycle-bin-only deletion -----------------

    /// Moves the entries behind `ids` to the OS trash — Synso's ONLY
    /// deletion path, and never a permanent delete (see `crate::trash`).
    /// Per id: the live hot-index slot is resolved (a dead or unknown id is
    /// skipped and counted as neither success nor error), the real disk
    /// path comes from [`Engine::resolve_absolute`], and a path already
    /// gone from disk is skipped the same way. Everything else is moved in
    /// ONE trash batch (`trash::delete_paths_to_trash`); each path
    /// actually gone from disk afterwards is then synced out of the index
    /// with the watcher's removal mechanics
    /// (`crate::watcher::remove_path`: the row located by (dir, name)
    /// plus a subtree purge on the store, the slot removal plus subtree
    /// sweep on the hot index) under one store -> hot lock pair. A path
    /// the trash batch refused is logged and keeps its index rows; the
    /// next rescan heals any drift. Returns the success count.
    pub fn delete_to_trash(&self, ids: &[FileId]) -> Result<usize> {
        // Phase 1: one hot-index read snapshot -> one candidate per live
        // id. The (dir chain, name) key and the index-relative spelling are
        // copied out so the lock can drop before any I/O. The store lock is
        // NOT taken here — locks never span the trash batch's I/O.
        let mut candidates: Vec<Candidate> = Vec::with_capacity(ids.len());
        {
            let hot = read_unpoisoned(&self.hot);
            for &id in ids {
                let Some(slot) = hot.slot_of_id(id) else {
                    continue; // dead/unknown id: skipped, not counted
                };
                candidates.push(Candidate {
                    segments: hot.dir_chain(slot).into_iter().map(String::from).collect(),
                    name: hot.name_original(slot).to_string(),
                    index_path: hot.path_of(slot),
                });
            }
        }
        // Phase 2: resolve disk paths and check existence OUTSIDE every
        // lock (the watcher batch's stat pass). An unresolvable root and a
        // path already gone from disk are both plain skips.
        let planned: Vec<PlannedDelete> = candidates
            .into_iter()
            .filter_map(
                |candidate| match self.resolve_absolute(&candidate.index_path) {
                    Some(abs) if trash::present(&abs) => Some(PlannedDelete {
                        abs,
                        segments: candidate.segments,
                        name: candidate.name,
                    }),
                    // Already gone from disk: nothing left to trash.
                    Some(_) => None,
                    None => {
                        log::warn!(
                            "synso: trash skipped {:?}: no scan root resolves it",
                            candidate.index_path
                        );
                        None
                    }
                },
            )
            .collect();
        if planned.is_empty() {
            return Ok(0);
        }

        // ONE batch for the whole slice; per-path refusals are the pure
        // layer's concern (logged there, counted from disk state here).
        let paths: Vec<&Path> = planned.iter().map(|entry| entry.abs.as_path()).collect();
        trash::delete_paths_to_trash(&paths)?;

        // Sync the paths that actually left the disk, under one
        // store -> hot lock pair (the watcher batch's discipline; no I/O
        // happens under these locks — the trash already did). The path is
        // re-checked rather than trusted: a backend that lost items of a
        // failing batch must not have its index rows deleted.
        let mut deleted = 0usize;
        {
            let mut store = lock_unpoisoned(&self.store);
            let mut hot = write_unpoisoned(&self.hot);
            for entry in &planned {
                if trash::present(&entry.abs) {
                    log::warn!(
                        "synso: trash refused {:?}; its index rows stay until the next rescan",
                        entry.abs
                    );
                    continue;
                }
                let segments: Vec<&str> = entry.segments.iter().map(String::as_str).collect();
                watcher::remove_path(&mut store, &mut hot, &segments, &entry.name, &entry.abs);
                deleted += 1;
            }
            // Trash deletions strand arena bytes like any other churn —
            // the same compaction check the watcher's batches run.
            hot.maybe_compact_arena();
        }
        Ok(deleted)
    }
}

/// Phase-1 candidate: one live id's (dir chain, name) key and its
/// index-relative path, copied out of a single hot-index snapshot before
/// the disk-resolution pass decides whether it can be trashed at all.
struct Candidate {
    segments: Vec<String>,
    name: String,
    index_path: String,
}

/// One id [`Engine::delete_to_trash`] resolved far enough to attempt: the
/// absolute disk path (still present at plan time), plus the key the index
/// removal locates the row and slot by.
struct PlannedDelete {
    abs: PathBuf,
    segments: Vec<String>,
    name: String,
}

/// Sends `event` to every live subscriber, dropping disconnected ones.
/// `pub(crate)`: the watcher (Task 8) broadcasts [`EngineEvent::FsChanged`]
/// through the same list.
pub(crate) fn broadcast(subscribers: &Arc<Mutex<Vec<Sender<EngineEvent>>>>, event: EngineEvent) {
    lock_unpoisoned(subscribers).retain(|sender| sender.send(event.clone()).is_ok());
}

/// Current Unix-epoch seconds (0 if the clock reads pre-epoch); the query
/// time the search recency window is measured against. `pub(crate)`:
/// the facade's standalone `search_index` uses the same query clock.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// Poison-tolerant locking for the engine's three locks (see the module
/// docs for why each lock's invariants survive a panic). Also the reason
/// this crate has no `unwrap`/`expect` in library code. `pub(crate)`: the
/// dedup pipeline (Task 9) takes the same short store locks.
pub(crate) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `lock_unpoisoned`, for read sides of the hot-index RwLock.
fn read_unpoisoned<T>(rwlock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    rwlock
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `lock_unpoisoned`, for write sides of the hot-index RwLock.
fn write_unpoisoned<T>(rwlock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rwlock
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Releases a one-at-a-time flag (the scan slot or the dedup slot) on ANY
/// exit from its worker — normal return or panic unwind — so a failed or
/// panicked run can never leave its trigger a silent no-op forever.
struct SlotGuard<'a>(&'a AtomicBool);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One full scan cycle on its own thread: purge each root's subtree, walk,
/// reload the hot index, broadcast the outcome. The scan slot is released
/// by [`SlotGuard`] on every exit path, panic included. Dedup is
/// MANUAL-only (user requirement): scans never kick off hashing — the
/// dedup view's 启动分析 button is the only trigger.
#[allow(clippy::too_many_arguments)]
fn scan_cycle(
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    scanning: Arc<AtomicBool>,
    opts: ScanOptions,
    _dedup: Arc<DedupShared>,
) {
    let _slot = SlotGuard(&scanning);
    let outcome = {
        let mut guard = lock_unpoisoned(&store);
        // Generation-based rescan (replaces the purge-and-reinsert that
        // needed a full in-memory hash snapshot — the ~0.5GB-at-1M-files
        // transient): bump the generation, walk (every upsert re-stamps
        // its row), then on SUCCESS sweep rows still on an older
        // generation — the paths the walk did not touch are the ones that
        // vanished while the app was closed. Unchanged rows are UPDATED
        // in place, so their persisted hashes survive by construction.
        let generation = guard.next_scan_generation();
        let scanned = scanner::scan(&mut guard, &opts, &|done| {
            broadcast(&subscribers, EngineEvent::ScanProgress { done, total: 0 });
        });
        scanned.map(|stats| {
            if let Err(err) = guard.sweep_stale(generation) {
                // Not fatal: the sweep is reconciliation; a failed one
                // leaves vanished rows indexed until the next rescan.
                log::warn!("synso: stale-generation sweep failed: {err}");
            }
            // Success only: swap in the fresh snapshot while still holding
            // the store lock (see module docs), so subscribers reacting to
            // ScanFinished already see fresh stats. On failure the hot
            // index keeps serving the last known-good snapshot.
            reload_hot_index_locked(&guard, &hot);
            (stats.files, stats.dirs)
        })
    };
    match outcome {
        Ok((files, dirs)) => {
            broadcast(&subscribers, EngineEvent::ScanFinished { files, dirs });
        }
        // The failure description travels in the event (no ScanFinished is
        // emitted); nothing else to do — the store stays partial until the
        // next successful rescan rebuilds it.
        Err(err) => {
            broadcast(
                &subscribers,
                EngineEvent::ScanFailed {
                    errors: 0,
                    message: err.to_string(),
                },
            );
        }
    }
}

/// Claims the dedup slot, resets the state machine to `Grouping` (the
/// `Idle -> Grouping` transition `dedup_start` promises — synchronously,
/// so pollers never miss it) and spawns the worker thread at the gear's
/// worker count. `hot` travels along because the cycle's Done path needs
/// a hot-index read for the run-completion aggregates. A no-op when a run
/// is already going. Returns whether a run was started.
fn launch_dedup(
    dedup: &Arc<DedupShared>,
    hot: Arc<RwLock<HotIndex>>,
    gear: DedupGear,
) -> bool {
    // Clear any stale cancel BEFORE the claim: a cancelled predecessor
    // leaves the flag set, and it must not pre-arm its successor.
    // `dedup_cancel` is a no-op while idle, so nothing can set the flag
    // between this store and a successful claim — a cancel arriving after
    // the claim is honored by the new run, one arriving before it is
    // (correctly) dropped.
    dedup.cancel.store(false, Ordering::Release);
    if dedup
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    {
        let mut book = lock_unpoisoned(&dedup.book);
        // The completion aggregates carry the previous run's values
        // through this reset: a run that ends CANCELLED keeps serving the
        // previous groups, and its final report must not zero their
        // aggregates (a COMPLETING run recomputes both fresh).
        let (alive_groups, wasted_bytes) = (book.state.alive_groups, book.state.wasted_bytes);
        book.state = DedupState {
            phase: DedupPhase::Grouping,
            alive_groups,
            wasted_bytes,
            ..DedupState::default()
        };
        // `groups` keeps the previous run's list: dedup_groups goes on
        // serving it until the new run finishes.
    }
    let workers = gear.workers();
    let gentle = gear == DedupGear::Gentle;
    let spawn = thread::Builder::new().name("synso-dedup".into()).spawn({
        let dedup = Arc::clone(dedup);
        move || dedup_cycle(dedup, hot, workers, gentle)
    });
    match spawn {
        Ok(_) => true,
        Err(err) => {
            log::error!("synso: failed to spawn the dedup thread: {err}");
            dedup.running.store(false, Ordering::Release);
            lock_unpoisoned(&dedup.book).state.phase = DedupPhase::Idle;
            false
        }
    }
}

/// One dedup cycle on its own thread: run the four-gate pipeline against
/// the store (hashing happens outside the lock — only the candidate load
/// and short persist batches take it), fold the outcome into the state
/// machine, then broadcast the final progress and — for a completed run —
/// `DedupFinished`. The slot is released by [`SlotGuard`] on every exit
/// path, panic included. The cycle is eprintln-free by design: every skip
/// is counted in [`DedupState::errors`] instead, and store failures merely
/// degrade the run — they never panic it.
///
/// `workers` is the gates' worker-pool size ([`DedupGear::workers`]);
/// `gentle` drops THIS thread below normal priority on Windows — see the
/// import note for why the gates' internal workers stay normal-priority
/// (accepted limitation). `hot` is the engine's hot index, read once at
/// Done for the run-completion aggregates ([`alive_aggregates`]).
fn dedup_cycle(
    dedup: Arc<DedupShared>,
    hot: Arc<RwLock<HotIndex>>,
    workers: usize,
    gentle: bool,
) {
    let _slot = SlotGuard(&dedup.running);
    // Gentle gear: below-normal priority for the whole run on this thread
    // (best-effort — a failure would only mean normal priority; there is
    // nothing to report). One unsafe call: Win32 thread priority.
    #[cfg(windows)]
    if gentle {
        // SAFETY: GetCurrentThread returns the (always valid) pseudo
        // handle of the calling thread; SetThreadPriority only touches
        // that thread's scheduler priority.
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
    #[cfg(not(windows))]
    let _ = gentle; // no thread-priority knobs off Windows (mac port)
    let subscribers = Arc::clone(&dedup.subscribers);
    let progress = |phase: DedupPhase, done: u64, total: u64| {
        {
            let mut book = lock_unpoisoned(&dedup.book);
            book.state.phase = phase;
            book.state.done = done;
            book.state.total = total;
        }
        // A cancelled run's FINAL report is broadcast by the cycle itself,
        // AFTER it folds the book (see the cancel branch) — exactly one
        // event, and only once the state it describes is fully written.
        if phase != DedupPhase::Cancelled {
            broadcast(
                &subscribers,
                EngineEvent::DedupProgress { phase, done, total },
            );
        }
    };
    let reader = dedup::RealFileReader;
    // One snapshot of the live root mapping for this run (the scope can
    // change mid-run; the next run picks it up). `now` seeds the mtime
    // grace window.
    let roots = read_unpoisoned(&dedup.roots).clone();
    let outcome = dedup::run_gates(
        &dedup.store,
        &roots,
        &reader,
        workers,
        &dedup.cancel,
        unix_now(),
        &progress,
    );
    let c = &outcome.counters;
    // ONE summary line per run (the spec's per-gate accounting), on both
    // exits — completion and cancellation.
    let tally = format!(
        "size_gate {}f/{}B head_gate {}f/{}B sample_gate {}f/{}B skips errors {} locked {} fresh {} placeholders {} recycle {} hashed first {} full {}",
        c.size_gate.files,
        c.size_gate.bytes,
        c.head_gate.files,
        c.head_gate.bytes,
        c.sample_gate.files,
        c.sample_gate.bytes,
        c.errors,
        c.locked,
        c.fresh,
        c.placeholders,
        c.recycle,
        outcome.hashed_first,
        outcome.hashed_full,
    );
    if outcome.cancelled {
        log::info!("synso: dedup run cancelled: {tally}");
        // Fold the book FIRST (only the partial skip count is new: the
        // closure already wrote the Cancelled phase and the last in-run
        // counters; `groups` is restored to the still-served list's count
        // — the launch reset zeroed it, and DedupState.groups is
        // documented as the last COMPLETED run's group count, which a
        // cancel does not displace), THEN broadcast the run's ONE final
        // DedupProgress{Cancelled, done, total} — after the state it
        // describes is fully written, so a subscriber reacting to it sees
        // the settled book. NO DedupFinished follows: the UI must not
        // reload groups on cancel.
        let final_report = {
            let mut book = lock_unpoisoned(&dedup.book);
            book.state.errors = c.errors + c.locked + c.placeholders;
            book.state.groups = book.groups.len() as u64;
            EngineEvent::DedupProgress {
                phase: book.state.phase,
                done: book.state.done,
                total: book.state.total,
            }
        };
        broadcast(&subscribers, final_report);
        return;
    }
    let groups = outcome.groups.len() as u64;
    log::info!("synso: dedup run finished: {groups} group(s); {tally}");
    let hashed = outcome.hashed_first + outcome.hashed_full;
    // The run-completion aggregates: one alive-scan over the fresh groups
    // against the CURRENT hot index, computed OUTSIDE the bookkeeping
    // lock (locks never nest) and cached in the state machine, so
    // `dedup_state()` stays a plain copy.
    let (alive_groups, wasted_bytes) = {
        let hot = read_unpoisoned(&hot);
        alive_aggregates(&hot, &outcome.groups)
    };
    {
        let mut book = lock_unpoisoned(&dedup.book);
        book.state = DedupState {
            phase: DedupPhase::Done,
            done: hashed,
            total: hashed,
            groups,
            // All skip kinds share the public counter (the design's
            // "marked skipped, not grouped" — unreadable, locked by
            // another handle, or a cloud placeholder); the outcome
            // counters keep them apart (fresh/recycle stay internal).
            errors: c.errors + c.locked + c.placeholders,
            alive_groups,
            wasted_bytes,
        };
        book.groups = Arc::new(outcome.groups);
    }
    broadcast(&subscribers, EngineEvent::DedupFinished { groups });
}

/// The run-completion aggregates (Task 8): alive-member counts per group
/// against the CURRENT hot index (`slot_of_id` presence — the exact rule
/// [`Engine::dedup_groups`] materializes by), `alive_groups` counting the
/// groups that still materialize (>= 2 live members) and `wasted_bytes`
/// summing `size × (alive − 1)` over exactly those. Free function so the
/// cycle computes it with no engine lock held — the hot read must never
/// nest inside the bookkeeping lock.
fn alive_aggregates(hot: &HotIndex, groups: &[StoredGroup]) -> (u64, u64) {
    let mut alive_groups = 0u64;
    let mut wasted_bytes = 0u64;
    for group in groups {
        let alive = group
            .members
            .iter()
            .filter(|id| hot.slot_of_id(**id).is_some())
            .count();
        if alive >= 2 {
            alive_groups += 1;
            wasted_bytes += group.size * (alive as u64 - 1);
        }
    }
    (alive_groups, wasted_bytes)
}

/// Loads a fresh hot index from the store and swaps it in, under the store
/// mutex (callers already holding it use [`reload_hot_index_locked`]).
fn reload_hot_index(store: &Arc<Mutex<Store>>, hot: &Arc<RwLock<HotIndex>>) {
    let guard = lock_unpoisoned(store);
    reload_hot_index_locked(&guard, hot);
}

fn reload_hot_index_locked(store: &Store, hot: &Arc<RwLock<HotIndex>>) {
    match HotIndex::load(store) {
        Ok(loaded) => *write_unpoisoned(hot) = loaded,
        Err(err) => log::error!("synso: hot-index load failed: {err}"),
    }
}

// ---------------- Task 9: one-time recycle-bin index purge ----------------

/// The `meta` key guarding the startup purge: stamped ONLY after
/// [`Store::purge_recycle_bin_rows`] succeeded, so a failed purge (marker
/// absent) retries on the next launch. A `meta` key, not a schema
/// migration — the store's schema stays untouched.
const RECYCLE_PURGE_DONE_KEY: &str = "recycle_purge_done";

/// Task 9's once-per-database purge: on a store without the
/// [`RECYCLE_PURGE_DONE_KEY`] marker, delete every already-indexed
/// recycle-bin row and stamp the marker. Every failure logs and returns
/// normally — open must not fail over housekeeping; the marker is
/// written only on success, so the next launch retries, and a purged
/// index re-running into `Ok(0)` is harmless. Runs under the store lock
/// during `open`, before the hot-index load spawns.
fn purge_recycle_bin_once(store: &mut Store) {
    match store.get_meta(RECYCLE_PURGE_DONE_KEY) {
        Ok(Some(_)) => {}
        Ok(None) => match store.purge_recycle_bin_rows() {
            Ok(count) => {
                if let Err(err) = store.set_meta(RECYCLE_PURGE_DONE_KEY, "1") {
                    log::warn!(
                        "synso: recycle-bin purge done but the marker write failed \
                         (it re-runs harmlessly next launch): {err}"
                    );
                }
                log::info!("synso: recycle-bin purge removed {count} indexed rows");
            }
            Err(err) => {
                log::warn!("synso: recycle-bin purge failed; will retry next launch: {err}")
            }
        },
        Err(err) => {
            log::warn!("synso: recycle purge marker read failed; skipping this launch: {err}")
        }
    }
}

// ---------------- v1.4: heat persistence --------------------------------

/// How many heat rows one snapshot write carries: the score-descending
/// head of the floored heat map — a whole-table picture, capped so even
/// a runaway heat map (bounded by the tier budgets, but still) can
/// never make the snapshot unbounded.
const HEAT_MAX_ROWS: usize = 10_000;

/// How often the `synso-heat-persist` thread snapshots the heat map
/// into the store between rescans.
const HEAT_PERSIST_INTERVAL: Duration = Duration::from_secs(600);

/// One heat snapshot write: the tier cache's heat map, decayed to now,
/// floored (`raw / 256 >= hot_threshold` — the same unit gate the
/// backfill side applies, so a one-touch entry never round-trips),
/// capped at [`HEAT_MAX_ROWS`], every row stamped with the one snapshot
/// epoch (the scores already carry all in-session ageing — the
/// heat-persistence design doc's §5). Best-effort: a store failure
/// logs and skips — the previous snapshot stays, merely stale.
fn persist_heat_into(store: &mut Store, tiers: &Arc<Mutex<TierCache<FileId, FileDetail>>>) {
    // One epoch for the whole snapshot: every score was decayed to the
    // same instant, so one stamp is exact, not an approximation.
    let at = unix_now();
    let rows: Vec<(FileId, u32, i64)> = {
        let mut cache = lock_unpoisoned(tiers);
        let floor = cache.policy().hot_threshold;
        cache
            .heat_above_floor(floor)
            .into_iter()
            .take(HEAT_MAX_ROWS)
            .map(|(id, raw)| (id, raw, at))
            .collect()
    };
    if let Err(err) = store.replace_heat(&rows) {
        log::warn!("synso: heat snapshot persist skipped: {err}");
    }
}

/// The rescan flush's carry list: the hot tier's residents with their
/// decayed scores, plus the flush itself. `None` when preloading is
/// off — the flush still happens (cached details are suspect after a
/// rescan either way), there is just nothing to carry back.
fn flush_snapshot(tiers: &mut TierCache<FileId, FileDetail>) -> Option<Vec<(FileId, u32)>> {
    if !tiers.policy().preload_on_start {
        tiers.clear();
        return None;
    }
    let ids = tiers.hot_ids_with_scores();
    tiers.clear();
    Some(ids)
}

/// Replays a scored id list into the hot tier: one batched cold read
/// (`file_details` chunks its IN-lists at 500 internally) fetches the
/// surviving rows — an id deleted since the score was booked is simply
/// absent from the result and silently drops out — and each survivor
/// re-enters hot WITH its score (the max-merge staging takeover of
/// [`TierCache::put_hot_direct_with_score`]). Returns whether the cold
/// reads succeeded — also `true` for an empty list (nothing to read).
/// The rescan flushes use `false` to SKIP their persist: their heat map
/// was already flushed, so writing the snapshot now would replace the
/// last good table with an empty one.
fn reinsert_with_scores(
    tiers: &mut TierCache<FileId, FileDetail>,
    cold: &ColdReader,
    scored: &[(FileId, u32)],
) -> bool {
    if scored.is_empty() {
        return true;
    }
    let ids: Vec<FileId> = scored.iter().map(|(id, _)| *id).collect();
    // Best-effort: a failed read leaves the tier empty-handed — the
    // next rescan or restart rebuilds it.
    let fresh = match cold.file_details(&ids) {
        Ok(fresh) => fresh,
        Err(err) => {
            log::warn!("synso: heat replay skipped — the cold read failed: {err}");
            return false;
        }
    };
    let by_id: HashMap<FileId, FileDetail> = fresh
        .into_iter()
        .map(|detail| (detail.id, detail))
        .collect();
    for (id, score) in scored {
        if let Some(detail) = by_id.get(id) {
            tiers.put_hot_direct_with_score(*id, detail.clone(), detail.cached_bytes(), *score);
        }
    }
    true
}

/// The rescan companion's body — the engine's own path
/// ([`Engine::flush_dimensions_and_preheat`]) runs the same three steps
/// through the engine's locks: flush the tier caches, reinsert the
/// previously-hot ids with their scores, then persist the fresh
/// snapshot. Opens its OWN ColdReader: this runs on a companion thread
/// and the engine's connection is not `Sync`.
fn rescan_flush_thread_side(
    tiers: &Arc<Mutex<TierCache<FileId, FileDetail>>>,
    store: &Arc<Mutex<Store>>,
    db_path: &Path,
) {
    let Some(scored) = flush_snapshot(&mut lock_unpoisoned(tiers)) else {
        return; // preload off: nothing to carry, nothing to persist
    };
    if !scored.is_empty() {
        // A throwaway reader in THIS thread (same rule as the scan
        // companion always followed).
        let carried = match ColdReader::open(db_path) {
            Ok(cold) => {
                let mut cache = lock_unpoisoned(tiers);
                reinsert_with_scores(&mut cache, &cold, &scored)
            }
            Err(err) => {
                log::warn!("synso: heat carry-back skipped — cold reader open failed: {err}");
                false
            }
        };
        if !carried {
            // The heat map was flushed and could not be rebuilt from the
            // store: persisting now would replace the last good snapshot
            // with an EMPTY table. Skip — the periodic persist or the
            // next successful rescan re-snapshots whatever heat has
            // re-earned by then.
            return;
        }
    }
    let mut guard = lock_unpoisoned(store);
    persist_heat_into(&mut guard, tiers);
}

/// The startup preheat's body: replay the persisted heat snapshot into
/// the fresh tier cache, applying the OFFLINE decay first — each row's
/// wall-clock age (`now − at`, clamped at zero for a future stamp:
/// clock skew must neither inflate nor crash a score) halves per
/// `decay_half_life`, and anything that fell below the floor
/// (`raw / 256 < hot_threshold`, the same unit gate as the persist
/// side) drops out — re-earning is the honest outcome for heat that
/// decayed away. `preload_on_start` off skips the whole replay (the
/// table is only ever read here); persistence itself is unaffected.
/// Opens its own ColdReader (thread-side; see
/// [`rescan_flush_thread_side`]).
fn preheat_thread_side(tiers: &Arc<Mutex<TierCache<FileId, FileDetail>>>, db_path: &Path) {
    // Best-effort at every step: no reader or no snapshot means no
    // preheat, never an error.
    let cold = match ColdReader::open(db_path) {
        Ok(cold) => cold,
        Err(err) => {
            log::warn!("synso: heat preheat skipped — cold reader open failed: {err}");
            return;
        }
    };
    let rows = match cold.read_heat(HEAT_MAX_ROWS) {
        Ok(rows) => rows,
        Err(err) => {
            log::warn!("synso: heat preheat skipped — heat snapshot read failed: {err}");
            return;
        }
    };
    let mut cache = lock_unpoisoned(tiers);
    let policy = cache.policy();
    if !policy.preload_on_start {
        return;
    }
    let now = unix_now();
    let scored: Vec<(FileId, u32)> = rows
        .into_iter()
        .map(|(id, score, at)| {
            let elapsed = now.saturating_sub(at).max(0) as u64;
            (
                id,
                tiers::decay_score(score, Duration::from_secs(elapsed), policy.decay_half_life),
            )
        })
        // The floor in HITS, on both ends of the round trip.
        .filter(|(_, raw)| raw / 256 >= policy.hot_threshold)
        .collect();
    reinsert_with_scores(&mut cache, &cold, &scored);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The JSON spelling of every phase, as served by the app's
    /// `dedup_state` IPC (Task 16): snake_case, matching the hand-mapped
    /// `dedup-progress` event payload in synso-app's events.rs — one
    /// spelling on both channels, or a frontend's phase switch silently
    /// never matches. Compiled only under the serde feature (plain
    /// `cargo test -p synso-core` keeps core dependency-lean; the
    /// workspace test run builds the feature via synso-app).
    #[cfg(feature = "serde")]
    #[test]
    fn dedup_phase_serializes_snake_case_for_json_bridges() {
        assert_eq!(
            serde_json::to_string(&DedupPhase::Idle).unwrap(),
            "\"idle\""
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::Grouping).unwrap(),
            "\"grouping\""
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::HashingFirst).unwrap(),
            "\"hashing_first\""
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::HashingFull).unwrap(),
            "\"hashing_full\""
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::Sampling).unwrap(),
            "\"sampling\"",
            "gate③'s phase (Task 6) spells snake_case like the rest"
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::Cancelled).unwrap(),
            "\"cancelled\"",
            "a cancelled run's final phase (Task 7) spells snake_case"
        );
        assert_eq!(
            serde_json::to_string(&DedupPhase::Done).unwrap(),
            "\"done\""
        );
        assert_eq!(
            serde_json::from_str::<DedupPhase>("\"hashing_first\"").unwrap(),
            DedupPhase::HashingFirst
        );
        assert_eq!(
            serde_json::from_str::<DedupPhase>("\"sampling\"").unwrap(),
            DedupPhase::Sampling
        );
        assert_eq!(
            serde_json::from_str::<DedupPhase>("\"cancelled\"").unwrap(),
            DedupPhase::Cancelled
        );
    }

    /// The scan-slot flag reads false while idle — the app shell's
    /// quit-dialog trigger (Task 19) must never fire without a scan.
    #[test]
    fn is_scanning_false_while_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = write_fixture(tmp.path());
        let engine = open_engine(&tmp.path().join("index.db"), &root);
        assert!(!engine.is_scanning());
    }

    /// Bounded poll for async side effects (watcher batches).
    fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cond() {
            assert!(Instant::now() < deadline, "condition never became true");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// THE settings-scope contract (fix round 1 for Task 19): a live
    /// exclusion swap (a) validates, (b) applies to the NEXT scan —
    /// excluded files leave search and the hot index, the rest survive —
    /// and (c) reaches the RESTARTED watcher, whose matcher must honor
    /// the new patterns without an engine restart.
    #[test]
    fn set_scan_options_exclusions_apply_to_next_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_, files, dirs) = wait_scan_finished(&rx);
        assert_eq!((files, dirs), (3, 2));
        assert_eq!(engine.search("readme", 0, 10).total, 1);

        // Exclude *.md live, rescan: readme.md is gone, the rest survive.
        engine
            .set_scan_options(
                vec![root.to_string_lossy().into_owned()],
                vec!["*.md".to_owned()],
            )
            .unwrap();
        engine.start_full_scan();
        let (_, files, dirs) = wait_scan_finished(&rx);
        assert_eq!((files, dirs), (2, 2), "readme.md excluded");
        assert_eq!(
            engine.search("readme", 0, 10).total,
            0,
            "excluded file out of search"
        );
        assert_eq!(engine.search("notes.txt", 0, 10).total, 1);

        // Watcher re-registration smoke: the restarted watcher derives
        // the NEW matcher — a new .md file stays out, a .txt lands.
        engine.start_watcher().unwrap();
        fs::write(root.join("later.md"), b"skip").unwrap();
        fs::write(root.join("later.txt"), b"keep").unwrap();
        wait_until(|| engine.search("later.txt", 0, 10).total == 1);
        assert_eq!(
            engine.search("later.md", 0, 10).total,
            0,
            "the restarted watcher honors the exclusion"
        );
    }

    /// Dropping a root from the live scope purges its index rows
    /// SYNCHRONOUSLY — store, hot index and search lose the removed
    /// root's data without any rescan, the kept root stays intact, and
    /// the dedup path mapping stops resolving the removed root.
    #[test]
    fn set_scan_options_purges_removed_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root_a = write_fixture(tmp.path());
        let root_b = tmp.path().join("other");
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_b.join("other.txt"), b"other").unwrap();

        let engine = Engine::open(
            &db_path,
            EngineOptions {
                volumes: vec![
                    root_a.to_string_lossy().into_owned(),
                    root_b.to_string_lossy().into_owned(),
                ],
                exclusions: Vec::new(),
            },
        )
        .unwrap();
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_, files, _) = wait_scan_finished(&rx);
        assert_eq!(files, 4, "3 fixture + 1 other");
        assert_eq!(engine.search("other.txt", 0, 10).total, 1);

        // Drop root_b — no rescan needed.
        engine
            .set_scan_options(vec![root_a.to_string_lossy().into_owned()], Vec::new())
            .unwrap();
        assert_eq!(
            engine.search("other.txt", 0, 10).total,
            0,
            "removed root out of search"
        );
        assert_eq!(
            engine.stats().files,
            3,
            "hot index reloaded without the removed root"
        );
        assert_eq!(engine.search("readme", 0, 10).total, 1, "kept root intact");
        let root_b_path = format!("other{}", std::path::MAIN_SEPARATOR);
        assert_eq!(
            engine.resolve_absolute(&root_b_path),
            None,
            "dedup mapping drops the removed root"
        );

        // Store-level truth: a fresh engine over the same DB (root_a
        // scope) reloads exactly the kept rows.
        let reopened = open_engine(&db_path, &root_a);
        wait_warm(&reopened);
        assert_eq!(reopened.stats().files, 3);
    }

    /// A bad pattern fails BEFORE any mutation: the error surfaces, the
    /// scope stays as it was, and a subsequent scan indexes everything.
    #[test]
    fn set_scan_options_rejects_bad_pattern_without_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        let err = engine
            .set_scan_options(
                vec![root.to_string_lossy().into_owned()],
                vec!["[bad".to_owned()],
            )
            .expect_err("bad glob must fail");
        assert!(matches!(err, crate::SynsoError::InvalidPattern(_)));

        engine.start_full_scan();
        let (_, files, _) = wait_scan_finished(&rx);
        assert_eq!(files, 3, "the scope was left untouched");
    }

    /// Fixture tree: `fixture/{readme.md, docs/{notes.txt, final.pdf,
    /// empty_dir}}` — 3 files, 2 directories.
    fn write_fixture(base: &Path) -> PathBuf {
        let root = base.join("fixture");
        fs::create_dir_all(root.join("docs").join("empty_dir")).unwrap();
        fs::write(root.join("readme.md"), b"hello").unwrap();
        fs::write(root.join("docs").join("notes.txt"), b"0123456789").unwrap();
        fs::write(root.join("docs").join("final.pdf"), b"x").unwrap();
        root
    }

    /// Backdates a fixture file's mtime far into the past. The four-gate
    /// pipeline skips candidates about to be OPENED whose snapshot mtime
    /// is inside the grace window (`crate::dedup::FRESH_SECS`, 300s — a
    /// file may still be mid-write), so freshly written dedup fixtures
    /// must be settled or every run skips them without reading. Content
    /// is untouched; rescans re-read the settled mtime, so persisted
    /// hashes survive the upsert conflict path as in production.
    fn backdate(path: &Path) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
    }

    fn open_engine(db: &Path, root: &Path) -> Engine {
        open_engine_with(db, root, Vec::new())
    }

    fn open_engine_with(db: &Path, root: &Path, exclusions: Vec<String>) -> Engine {
        Engine::open(
            db,
            EngineOptions {
                volumes: vec![root.to_string_lossy().into_owned()],
                exclusions,
            },
        )
        .unwrap()
    }

    /// Polls until the initial hot-index load completed.
    fn wait_warm(engine: &Engine) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !engine.stats().warm {
            assert!(Instant::now() < deadline, "hot index never warmed up");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Receives events until `ScanFinished`, returning the files-done
    /// counts of every `ScanProgress` seen and the final counts.
    fn wait_scan_finished(rx: &Receiver<EngineEvent>) -> (Vec<u64>, u64, u64) {
        let mut progress = Vec::new();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("an event arrives before the timeout");
            match event {
                EngineEvent::ScanProgress { done, total } => {
                    assert_eq!(total, 0, "the walk total is indeterminate");
                    progress.push(done);
                }
                EngineEvent::ScanFinished { files, dirs } => return (progress, files, dirs),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    /// Receives events until `ScanFailed`, asserting no `ScanFinished` ever
    /// arrives; returns the failure message.
    fn wait_scan_failed(rx: &Receiver<EngineEvent>) -> String {
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("an event arrives before the timeout");
            match event {
                EngineEvent::ScanProgress { .. } => continue,
                EngineEvent::ScanFailed { message, .. } => return message,
                other => panic!("expected ScanFailed, got {other:?}"),
            }
        }
    }

    #[test]
    fn engine_scan_then_reload_from_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        // First run: scan the fixture, then drop the engine (app closes).
        let first = {
            let engine = open_engine(&db_path, &root);
            let rx = engine.subscribe();
            engine.start_full_scan();
            let (_progress, files, dirs) = wait_scan_finished(&rx);
            assert_eq!(
                (files, dirs),
                (3, 2),
                "readme.md/notes.txt/final.pdf; docs, empty_dir"
            );
            let stats = engine.stats();
            assert!(stats.warm, "the post-scan reload marks the index warm");
            assert_eq!(stats.files, 3);
            stats.files
        };

        // Reopen the SAME database: the background load must surface the
        // persisted rows without any new scan.
        let engine = open_engine(&db_path, &root);
        wait_warm(&engine);
        let stats = engine.stats();
        assert_eq!(
            stats.files, first,
            "reload from the DB keeps the file count"
        );
        assert_eq!(stats.dirs, 2);
        assert!(stats.interned_bytes > 0);
    }

    /// Task 9: `Engine::open` runs the one-time recycle-bin purge BEFORE
    /// the hot-index load spawns — purged rows never materialize in the
    /// hot index — and stamps the `recycle_purge_done` meta key so the
    /// work never repeats. A recycle-bin row that appears after the
    /// marker survives a second open untouched: the guard holds (the
    /// matcher hard rule keeps NEW rows out of the index; this marker is
    /// only about the historical one).
    #[test]
    fn recycle_purge_runs_once_before_hot_load() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let seed = |store: &mut Store, dir_path: &[&str], name: &str, size: u64| {
            let mut record = crate::model::FileRecord {
                id: 0,
                parent: 0,
                name: name.to_string(),
                size,
                mtime: 1_700_000_000,
                ctime: 1_700_000_000,
                atime: 1_700_000_000,
                ext_id: 0,
                is_dir: false,
                hash_first: None,
                hash_full: None,
                py_full: String::new(),
                py_init: String::new(),
            };
            store.upsert_file(dir_path, &mut record).unwrap();
        };

        // A historical index that still carries recycle-bin rows (seeded
        // directly — the production source is a pre-Task-9 scan).
        {
            let mut store = Store::open(&db_path).unwrap();
            seed(&mut store, &["$Recycle.Bin", "S-1-5"], "r1.bin", 7);
            seed(&mut store, &["docs"], "keep.txt", 1);
        }

        let engine = open_engine(&db_path, &root);
        wait_warm(&engine);
        let stats = engine.stats();
        assert_eq!(
            stats.files, 1,
            "only keep.txt surfaces: the purge ran before the hot load"
        );
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let marker: String = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'recycle_purge_done'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(marker, "1", "the once-marker is stamped");
            let left: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM files WHERE name = 'r1.bin'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "the store row is deleted, not just hidden");
        }
        drop(engine);

        // A recycle-bin row landing after the marker must survive the
        // second open: no re-run.
        {
            let mut store = Store::open(&db_path).unwrap();
            seed(&mut store, &["$Recycle.Bin", "S-2"], "sneaky.bin", 9);
        }
        let engine = open_engine(&db_path, &root);
        wait_warm(&engine);
        assert_eq!(
            engine.stats().files,
            2,
            "the marker is present, so the second open does not re-purge"
        );
    }

    #[test]
    fn scan_progress_events_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();

        let (progress, files, dirs) = wait_scan_finished(&rx);
        assert!(
            !progress.is_empty(),
            "at least one ScanProgress must reach subscribers"
        );
        assert_eq!(
            *progress.last().unwrap(),
            files,
            "the last progress reports files-done"
        );
        assert_eq!((files, dirs), (3, 2));
        assert_eq!(
            engine.stats().files,
            3,
            "the hot index is fresh by ScanFinished"
        );
    }

    #[test]
    fn rescan_purges_files_deleted_while_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, files, _dirs) = wait_scan_finished(&rx);
        assert_eq!(files, 3);
        assert_eq!(engine.stats().files, 3);

        // "App was closed": a file and a whole directory vanish from disk.
        fs::remove_file(root.join("readme.md")).unwrap();
        fs::remove_dir_all(root.join("docs").join("empty_dir")).unwrap();

        engine.start_full_scan();
        let (_progress2, files2, dirs2) = wait_scan_finished(&rx);
        assert_eq!(
            (files2, dirs2),
            (2, 1),
            "readme.md and empty_dir must not linger after the rescan"
        );

        // The fresh hot index reflects disk truth...
        let stats = engine.stats();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.dirs, 1);
        // ...and the rows are truly gone from the store itself.
        let store = Store::open(&db_path).unwrap();
        let names: Vec<String> = store
            .all_files()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        for gone in ["readme.md", "empty_dir"] {
            assert!(!names.contains(&gone.to_string()), "{gone} must be purged");
        }
        for kept in ["notes.txt", "final.pdf", "docs"] {
            assert!(names.contains(&kept.to_string()), "{kept} must survive");
        }
    }

    #[test]
    fn failed_scan_emits_scan_failed_and_keeps_last_known_good() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        // A healthy scan first, so the database holds a good snapshot.
        {
            let engine = open_engine(&db_path, &root);
            let rx = engine.subscribe();
            engine.start_full_scan();
            let (_progress, files, _dirs) = wait_scan_finished(&rx);
            assert_eq!(files, 3);
        }

        // "[" is an invalid glob: the scan fails right after the per-root
        // purge (which already committed) and before any walk.
        let engine = open_engine_with(&db_path, &root, vec!["[".to_string()]);
        wait_warm(&engine); // the initial load serves the good snapshot
        let rx = engine.subscribe();
        engine.start_full_scan();
        let message = wait_scan_failed(&rx);
        assert!(!message.is_empty(), "the cause travels in the event");

        // stats() still works and keeps serving the last-known-good
        // snapshot (the hot index was NOT swapped)...
        let stats = engine.stats();
        assert_eq!(stats.files, 3);
        assert!(stats.warm);
        // ...and with generation-based rescans the STORE keeps its rows
        // too: nothing is deleted up front anymore (the old purge left a
        // partial store behind; the failed walk now simply re-stamps
        // nothing and sweeps nothing).
        let store = Store::open(&db_path).unwrap();
        assert_eq!(
            store.all_files().unwrap().len(),
            5,
            "a failed scan must not destroy the previous index"
        );

        // The scan slot was released on the failure path: a second scan
        // runs again instead of being a silent no-op (it fails again for
        // the same reason, which still proves the slot came free).
        engine.start_full_scan();
        wait_scan_failed(&rx);

        // A good engine self-heals the partial store with one rescan.
        drop(engine);
        let healed = open_engine(&db_path, &root);
        let rx2 = healed.subscribe();
        healed.start_full_scan();
        let (_progress, files, dirs) = wait_scan_finished(&rx2);
        assert_eq!((files, dirs), (3, 2));
        assert_eq!(healed.stats().files, 3);
    }

    #[test]
    fn poisoned_locks_keep_engine_usable() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, files, _dirs) = wait_scan_finished(&rx);
        assert_eq!(files, 3);

        // Panic while holding each of the engine's three locks, poisoning
        // it. (The panic output below is expected.)
        let store = Arc::clone(&engine.store);
        std::thread::spawn(move || {
            let _guard = store.lock().unwrap();
            panic!("poison the store mutex");
        })
        .join()
        .unwrap_err();

        let hot = Arc::clone(&engine.hot);
        std::thread::spawn(move || {
            let _guard = hot.write().unwrap();
            panic!("poison the hot-index lock");
        })
        .join()
        .unwrap_err();

        let subscribers = Arc::clone(&engine.subscribers);
        std::thread::spawn(move || {
            let _guard = subscribers.lock().unwrap();
            panic!("poison the subscriber list");
        })
        .join()
        .unwrap_err();

        // Every public path still works across the poisoned locks: none of
        // these may panic, and a full scan cycle runs to completion.
        let _rx2 = engine.subscribe();
        assert_eq!(engine.stats().files, 3);
        engine.start_full_scan();
        let (_progress, files, dirs) = wait_scan_finished(&rx);
        assert_eq!((files, dirs), (3, 2));
        assert_eq!(engine.stats().files, 3);
    }

    // ------------------- Task 9: dedup pipeline ---------------------------

    /// Receives events until `DedupFinished`, returning the finished run's
    /// group count and the number of `DedupProgress` events seen on the way.
    fn wait_dedup_finished(rx: &Receiver<EngineEvent>) -> (u64, usize) {
        let mut progress = 0usize;
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("an event arrives before the timeout");
            match event {
                EngineEvent::DedupProgress { .. } => progress += 1,
                EngineEvent::DedupFinished { groups } => return (groups, progress),
                other => panic!("expected a dedup event, got {other:?}"),
            }
        }
    }

    #[test]
    fn dedup_finds_identical_content() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("dup_a.txt"), b"alpha bravo").unwrap();
        fs::write(root.join("sub").join("dup_b.txt"), b"alpha bravo").unwrap();
        fs::write(root.join("other.txt"), b"alpha bravO").unwrap(); // same size, last byte differs
        for f in ["dup_a.txt", "other.txt"] {
            backdate(&root.join(f));
        }
        backdate(&root.join("sub").join("dup_b.txt"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, files, _dirs) = wait_scan_finished(&rx);
        assert_eq!(files, 3);

        engine.dedup_start();
        let (groups, progress_events) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1, "only the byte-identical pair groups");
        assert!(progress_events >= 1, "progress flows via DedupProgress");

        let state = engine.dedup_state();
        assert_eq!(state.phase, DedupPhase::Done);
        assert_eq!(state.groups, 1);

        let page = engine.dedup_groups(0, 10);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.total_alive, 1);
        let group = &page.groups[0];
        assert_eq!(group.size, 11, "alpha bravo");
        assert_eq!(group.members.len(), 2);
        let mut names: Vec<&str> = group.members.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["dup_a.txt", "dup_b.txt"]);
        assert!(group.members.iter().all(|m| m.size == 11 && !m.is_dir));
        // Members are real hits whose paths resolve through the engine.
        for member in &group.members {
            assert!(engine.resolve_absolute(&member.path).is_some());
        }
        // The one-byte-different file belongs to no group; paging past the
        // single group yields nothing.
        assert!(engine
            .dedup_groups(0, 10)
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .all(|m| m.name != "other.txt"));
        assert!(engine.dedup_groups(1, 10).groups.is_empty());
    }

    /// THE paged-frontend contract (Task 8): `dedup_groups` returns a
    /// [`DedupPage`] whose `total_alive` counts every group that still
    /// materializes against the CURRENT hot index (>= 2 surviving members),
    /// dissolved groups' members never materialize — and the run-completion
    /// aggregates (`alive_groups` / `wasted_bytes`) recount ONLY the
    /// surviving group once its rivals' members leave the store and a
    /// rerun folds the book. Before any completed run both are 0.
    #[test]
    fn dedup_page_total_alive_and_wasted_track_surviving_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        // Group A: two 11-byte twins. Group B: two 5-byte twins. The
        // differing sizes make the wasted arithmetic attributable per
        // group (each alive pair wastes exactly one redundant copy).
        fs::write(root.join("a1.txt"), b"alpha bravo").unwrap();
        fs::write(root.join("a2.txt"), b"alpha bravo").unwrap();
        fs::write(root.join("b1.txt"), b"bbbbb").unwrap();
        fs::write(root.join("b2.txt"), b"bbbbb").unwrap();
        for name in ["a1.txt", "a2.txt", "b1.txt", "b2.txt"] {
            backdate(&root.join(name));
        }

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // No completed run yet: zero aggregates, zero alive total.
        let state = engine.dedup_state();
        assert_eq!(
            (state.alive_groups, state.wasted_bytes),
            (0, 0),
            "before any run the aggregates are 0/0"
        );
        assert_eq!(engine.dedup_groups(0, 10).total_alive, 0);

        // Run 1: both pairs group.
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 2);

        let state = engine.dedup_state();
        assert_eq!(state.phase, DedupPhase::Done);
        assert_eq!(state.groups, 2);
        assert_eq!(
            state.alive_groups, 2,
            "both groups fully alive at completion"
        );
        assert_eq!(
            state.wasted_bytes,
            11 + 5,
            "each pair wastes one copy of its size"
        );

        let page = engine.dedup_groups(0, 10);
        assert_eq!(page.groups.len(), 2);
        assert_eq!(page.total_alive, 2);
        // limit 0: an empty page still reports the full alive total.
        assert_eq!(engine.dedup_groups(0, 0).groups.len(), 0);
        assert_eq!(engine.dedup_groups(0, 0).total_alive, 2);
        // Paging past the alive total yields nothing.
        assert!(engine.dedup_groups(2, 10).groups.is_empty());

        // Group B's members leave the disk; the rescan reconciles the
        // store and the hot index (the existing fixture deletion pattern).
        fs::remove_file(root.join("b1.txt")).unwrap();
        fs::remove_file(root.join("b2.txt")).unwrap();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // Live view, BEFORE any rerun: the dissolved group's members never
        // materialize and the alive total recounts only group A.
        let page = engine.dedup_groups(0, 10);
        assert_eq!(page.total_alive, 1, "group B dissolved below 2 members");
        assert_eq!(page.groups.len(), 1);
        let names: Vec<&str> = page.groups[0]
            .members
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(
            names.iter().all(|n| n.starts_with('a')),
            "only group A materializes: {names:?}"
        );

        // A rerun folds the book over the post-deletion store: the
        // aggregates recount ONLY the surviving group.
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1, "group B is gone from the store");
        let state = engine.dedup_state();
        assert_eq!(state.alive_groups, 1);
        assert_eq!(
            state.wasted_bytes, 11,
            "wasted counts only the surviving group's one redundant copy"
        );
    }

    #[test]
    fn dedup_excludes_zero_size() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("empty_a.txt"), b"").unwrap();
        fs::write(root.join("empty_b.txt"), b"").unwrap();
        fs::write(root.join("real.txt"), b"r").unwrap();
        backdate(&root.join("real.txt"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 0, "zero-size files never group");
        assert!(engine.dedup_groups(0, 10).groups.is_empty());
        let state = engine.dedup_state();
        assert_eq!(state.phase, DedupPhase::Done);
        assert_eq!(state.errors, 0, "the size-0 filter is not an error");
    }

    #[cfg(windows)]
    #[test]
    fn dedup_marks_unreadable_skipped() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("locked.txt"), b"shared secret").unwrap();
        fs::write(root.join("free.txt"), b"shared secret").unwrap();
        backdate(&root.join("locked.txt"));
        backdate(&root.join("free.txt"));

        // Hold the file with NO sharing flags: any other open fails.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.join("locked.txt"))
            .unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 0, "the locked member cannot confirm a group");
        assert!(engine.dedup_groups(0, 10).groups.is_empty());
        let state = engine.dedup_state();
        assert_eq!(state.phase, DedupPhase::Done);
        assert!(
            state.errors >= 1,
            "the sharing violation is counted, not fatal"
        );

        // Release the handle: a rerun hashes the file and the pair groups.
        drop(held);
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1);
        assert_eq!(engine.dedup_groups(0, 10).groups[0].members.len(), 2);
        assert_eq!(engine.dedup_state().errors, 0, "errors are per run");
    }

    #[test]
    fn dedup_persists_hashes_and_is_incremental() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep1.txt"), b"persist me!!").unwrap();
        fs::write(root.join("keep2.txt"), b"persist me!!").unwrap();
        backdate(&root.join("keep1.txt"));
        backdate(&root.join("keep2.txt"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // First run: both files are hashed.
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1);
        let first_state = engine.dedup_state();
        assert!(first_state.done > 0, "the first run did hashing work");

        // The computed hashes live in the store itself.
        let store = Store::open(&db_path).unwrap();
        let rows = store.all_files().unwrap();
        for name in ["keep1.txt", "keep2.txt"] {
            let row = rows.iter().find(|r| r.name == name).unwrap();
            assert!(row.hash_first.is_some(), "{name} hash_first persisted");
            assert!(row.hash_full.is_some(), "{name} hash_full persisted");
        }

        // Second run: everything is already persisted, no hashing happens.
        engine.dedup_start();
        let (groups2, _progress2) = wait_dedup_finished(&rx);
        assert_eq!(groups2, 1, "the group is found again");
        let second_state = engine.dedup_state();
        assert_eq!(second_state.phase, DedupPhase::Done);
        assert_eq!(
            (second_state.done, second_state.total),
            (0, 0),
            "done does not grow: already-hashed files are skipped"
        );
    }

    /// User requirement: dedup is manual-only — a completed scan must NOT
    /// kick off hashing. Give the (removed) 30s autostart an order of
    /// magnitude less than its old delay and assert nothing ran.
    #[test]
    fn scan_does_not_autostart_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"x").unwrap();
        fs::write(root.join("b.txt"), b"x").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            engine.dedup_state().phase,
            DedupPhase::Idle,
            "dedup must stay Idle after a scan — it is manual-trigger only"
        );
        // RecvTimeoutError::Timeout is the expected outcome; any event
        // would be a DedupProgress we must not see.
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// A cancel pressed while idle is a no-op: nothing panics, the state
    /// stays Idle, and the NEXT run is not pre-armed cancelled — it runs
    /// to `Done` normally.
    #[test]
    fn dedup_cancel_while_idle_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"twin").unwrap();
        fs::write(root.join("b.txt"), b"twin").unwrap();
        backdate(&root.join("a.txt"));
        backdate(&root.join("b.txt"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_cancel();
        engine.dedup_cancel(); // idempotent
        assert_eq!(
            engine.dedup_state().phase,
            DedupPhase::Idle,
            "an idle cancel leaves the state machine alone"
        );

        // The next run completes normally — the idle cancel pre-armed
        // nothing.
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1);
        assert_eq!(engine.dedup_state().phase, DedupPhase::Done);
    }

    /// THE cancel contract, end to end: a run cancelled mid-flight ends at
    /// `Cancelled` with exactly ONE final `Cancelled` progress report,
    /// never broadcasts `DedupFinished`, and the engine keeps serving the
    /// last completed run's groups.
    #[test]
    fn dedup_cancel_mid_run_reports_cancelled_and_keeps_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"twin").unwrap();
        fs::write(root.join("b.txt"), b"twin").unwrap();
        backdate(&root.join("a.txt"));
        backdate(&root.join("b.txt"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // Run 1: small and fast — it must COMPLETE, so "previous groups"
        // is non-empty for run 2 to keep.
        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1);
        assert_eq!(engine.dedup_groups(0, 10).groups.len(), 1);
        // Run-completion aggregates (Task 8) for the 4-byte twin pair.
        let state = engine.dedup_state();
        assert_eq!(
            (state.alive_groups, state.wasted_bytes),
            (1, 4),
            "one alive pair wastes one redundant 4-byte copy"
        );

        // Run 2 gets real work: 300 identical unhashed 80KB copies join the
        // index via a rescan (the unchanged twins keep their hashes — the
        // new files are the only work). ~50MB of hashing lies between the
        // run's FIRST progress report and its end, so a cancel armed on
        // that report lands mid-run with orders of magnitude of margin.
        // (The copies are backdated: fresh candidates are skipped without
        // being opened, which would leave the run nothing to do.)
        fs::create_dir_all(root.join("big")).unwrap();
        let big = vec![b'e'; 80 * 1024];
        for i in 0..300 {
            let path = root.join("big").join(format!("copy{i:03}.bin"));
            fs::write(&path, &big).unwrap();
            backdate(&path);
        }
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_start();
        let mut reports = 0usize;
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the cancelled run still broadcasts its final report");
            match event {
                EngineEvent::DedupProgress { phase, done, total } => {
                    reports += 1;
                    if phase == DedupPhase::Cancelled {
                        // The final report re-carries the last in-run
                        // counters — the run had started real work.
                        assert!(
                            done <= total,
                            "cancelled report carries sane counters: {done}/{total}"
                        );
                        break; // the cycle broadcasts nothing after this
                    }
                    // Any in-run report: arm the cancel (idempotent).
                    engine.dedup_cancel();
                }
                EngineEvent::DedupFinished { groups } => panic!(
                    "a cancelled run must NOT broadcast DedupFinished (got groups={groups})"
                ),
                other => panic!("unexpected event during dedup cancel: {other:?}"),
            }
        }
        assert!(reports >= 2, "cancel armed on an in-run report, saw {reports}");

        // The state machine landed on Cancelled — not Done — and the last
        // completed run's groups stay on duty (no reload happened).
        let state = engine.dedup_state();
        assert_eq!(state.phase, DedupPhase::Cancelled);
        assert_eq!(
            state.groups, 1,
            "a cancelled run keeps the previous run's group count"
        );
        assert_eq!(
            (state.alive_groups, state.wasted_bytes),
            (1, 4),
            "a cancelled run carries the PREVIOUS run's aggregates, not zeroes"
        );
        assert_eq!(
            engine.dedup_groups(0, 10).groups.len(),
            1,
            "dedup_groups still serves the previous run's group"
        );

        // The stream went quiet: the Cancelled report is the cycle's last
        // broadcast (the run has already ended by the time the state says
        // Cancelled — the report itself wrote it).
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "no event may follow the Cancelled report (no DedupFinished)"
        );
    }

    /// User requirement: "mtime unchanged → never recompute" across
    /// RESCANS too — the upsert's conflict path preserves persisted hashes
    /// for unchanged files (the purge+reinsert of a rescan used to wipe
    /// them, forcing a full re-hash every time).
    #[test]
    fn rescan_preserves_hashes_of_unchanged_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("same1.bin"), b"hash cache me").unwrap();
        fs::write(root.join("same2.bin"), b"hash cache me").unwrap();
        backdate(&root.join("same1.bin"));
        backdate(&root.join("same2.bin"));

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_start();
        let (groups, _) = wait_dedup_finished(&rx);
        assert_eq!(groups, 1);
        let first_done = engine.dedup_state().done;
        assert!(first_done > 0);

        // A rescan purges and re-inserts every row; unchanged files must
        // keep their hashes through the upsert's COALESCE-style conflict.
        engine.start_full_scan();
        wait_scan_finished(&rx);
        let store = Store::open(&db_path).unwrap();
        for row in store.all_files().unwrap() {
            assert!(
                row.hash_full.is_some(),
                "{} lost its hash in the rescan",
                row.name
            );
        }

        // And the next dedup run has nothing to hash.
        engine.dedup_start();
        let (groups2, _) = wait_dedup_finished(&rx);
        assert_eq!(groups2, 1);
        assert_eq!(
            engine.dedup_state().done,
            0,
            "recompute skipped entirely after rescan"
        );
    }

    /// The other half of the cache rule: a file that CHANGED (size/mtime)
    /// loses its stale hash in the rescan and gets re-hashed.
    #[test]
    fn changed_file_invalidates_its_hash_on_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("mutant.bin"), b"old content").unwrap();
        backdate(&root.join("mutant.bin")); // settled: the first run really hashes it

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);
        engine.dedup_start();
        wait_dedup_finished(&rx);

        // Grow the file, bump its mtime, rescan.
        thread::sleep(Duration::from_millis(1100)); // mtime second granularity
        fs::write(root.join("mutant.bin"), b"new longer content!").unwrap();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let store = Store::open(&db_path).unwrap();
        let row = store
            .all_files()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "mutant.bin")
            .unwrap();
        assert!(row.hash_full.is_none(), "changed file must drop its hash");
    }

    /// The absolute-path resolution the dedup hashing (and Task 10 trash /
    /// Task 15 reveal) rely on: a hit's index-relative path under a NESTED
    /// root resolves to the real file, which then actually opens on disk.
    #[test]
    fn resolve_absolute_opens_nested_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("readme.md"), b"hello nested").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let res = engine.search("readme", 0, 5);
        assert_eq!(res.total, 1);
        let abs = engine
            .resolve_absolute(&res.hits[0].path)
            .expect("the root segment resolves");
        assert_eq!(abs, root.join("readme.md"));
        assert_eq!(fs::read_to_string(&abs).unwrap(), "hello nested");
        // A path whose root segment matches no root cannot resolve.
        assert_eq!(engine.resolve_absolute("not_a_root_segment/x.txt"), None);
    }

    // ------------------ Task 15: id → hit for reveal/open ----------------

    /// The quick window's open/reveal commands resolve an id to its hit
    /// before mapping to disk: a live id returns the very hit search
    /// returned, and dead/unknown ids (deleted entries, garbage from the
    /// webview) resolve to `None` so the app layer can error cleanly.
    #[test]
    fn hit_by_id_resolves_live_ids_only() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let res = engine.search("notes", 0, 5);
        assert_eq!(res.total, 1);
        let hit = &res.hits[0];
        assert_eq!(
            engine.hit_by_id(hit.id).as_ref(),
            Some(hit),
            "a live id resolves to the same hit search returned"
        );

        // Never-indexed and structurally invalid ids are dead ends.
        assert_eq!(engine.hit_by_id(999_999), None);
        assert_eq!(engine.hit_by_id(-1), None);
    }

    /// `file_id_of` (Task 18) turns an index path — the spelling
    /// flame-chart nodes carry — into the same row id a search hit for it
    /// would carry, files and directory entries alike; unknown paths and
    /// the virtual root resolve to `None` so the app layer can error.
    #[test]
    fn file_id_of_resolves_paths_to_row_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs").join("notes.txt"), b"hello").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let file_hit = engine.search("notes", 0, 5);
        assert_eq!(file_hit.total, 1);
        let file = &file_hit.hits[0];
        assert_eq!(
            engine.file_id_of(&file.path),
            Some(file.id),
            "a file path resolves to its own row id"
        );

        let dir_hit = engine.search("docs", 0, 5);
        assert_eq!(dir_hit.total, 1);
        let dir = &dir_hit.hits[0];
        assert!(dir.is_dir);
        assert_eq!(
            engine.file_id_of(&dir.path),
            Some(dir.id),
            "a directory path resolves to the dir's files row id"
        );

        // Dead ends: the virtual root names no single entry, and neither
        // does anything outside the index.
        assert_eq!(engine.file_id_of(""), None);
        assert_eq!(engine.file_id_of("docs/absent.bin"), None);
        assert_eq!(engine.file_id_of("not_a_root/x.txt"), None);
    }

    // ------------------ Task 10: recycle-bin deletion ---------------------

    /// The recycle-bin contract end to end: the file leaves the disk, leaves
    /// search, and leaves the store.
    #[test]
    fn delete_to_trash_removes_and_updates_index() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("doomed.txt"), b"trash me").unwrap();
        fs::write(root.join("keeper.txt"), b"keep me").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let res = engine.search("doomed", 0, 10);
        assert_eq!(res.total, 1);
        let hit = res.hits[0].clone();
        let doomed_id = hit.id;
        let abs = engine
            .resolve_absolute(&hit.path)
            .expect("the root segment resolves");
        assert!(abs.is_file());

        assert_eq!(
            engine.delete_to_trash(&[doomed_id]).unwrap(),
            1,
            "exactly the one path counts as success"
        );

        // Gone from disk (the trash moved it away)...
        assert!(!abs.exists(), "the file must be gone from disk");
        // ...gone from search while the sibling survives...
        assert_eq!(engine.search("doomed", 0, 10).total, 0);
        assert_eq!(engine.search("keeper", 0, 10).total, 1);
        // ...gone from the store itself...
        let store = Store::open(&db_path).unwrap();
        assert!(store
            .all_files()
            .unwrap()
            .iter()
            .all(|rec| rec.name != "doomed.txt"));
        assert_eq!(engine.stats().files, 1);
    }

    /// A path already gone from disk (and an id that was never indexed) is a
    /// plain skip: zero successes, no error, and the stale index row is left
    /// whole for the next rescan to reconcile — no half-deletion.
    #[test]
    fn delete_missing_path_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("doomed.txt"), b"gone before the delete").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let res = engine.search("doomed", 0, 10);
        assert_eq!(res.total, 1);
        let id = res.hits[0].id;

        // The file vanishes OUTSIDE Synso (the "app was closed" scenario).
        // Renamed away here — Synso never permanently deletes, not even in
        // arranging this fixture.
        fs::rename(root.join("doomed.txt"), root.join("moved.txt")).unwrap();

        assert_eq!(engine.delete_to_trash(&[id]).unwrap(), 0);
        assert_eq!(engine.delete_to_trash(&[424_242]).unwrap(), 0);
        assert_eq!(engine.delete_to_trash(&[]).unwrap(), 0);

        // The skips left the index untouched.
        assert_eq!(engine.search("doomed", 0, 10).total, 1);
        assert_eq!(engine.stats().files, 1);
    }

    /// Trashing a directory takes the whole subtree with it: every
    /// descendant drops out of search AND out of the store (rows and dir
    /// rows alike), while a sibling outside the directory survives.
    #[test]
    fn delete_dir_to_trash_purges_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, files, dirs) = wait_scan_finished(&rx);
        assert_eq!((files, dirs), (3, 2));

        let res = engine.search("docs", 0, 10);
        assert_eq!(res.total, 1);
        assert!(res.hits[0].is_dir, "the docs directory entry itself");
        let docs_id = res.hits[0].id;

        assert_eq!(engine.delete_to_trash(&[docs_id]).unwrap(), 1);

        // Every descendant is gone from search...
        for purged in ["docs", "notes", "final", "empty_dir"] {
            assert_eq!(
                engine.search(purged, 0, 10).total,
                0,
                "{purged} must be purged with the subtree"
            );
        }
        assert_eq!(engine.search("readme", 0, 10).total, 1, "sibling survives");
        let stats = engine.stats();
        assert_eq!((stats.files, stats.dirs), (1, 0));
        // ...and from the store itself: only the sibling's row and the root
        // segment's dirs row remain.
        let store = Store::open(&db_path).unwrap();
        let names: Vec<String> = store
            .all_files()
            .unwrap()
            .into_iter()
            .map(|rec| rec.name)
            .collect();
        assert_eq!(names, vec!["readme.md".to_string()]);
        let dir_names: Vec<String> = store
            .all_dirs()
            .unwrap()
            .into_iter()
            .map(|(_, _, name)| name)
            .collect();
        assert_eq!(
            dir_names,
            vec!["fixture".to_string()],
            "docs and its subtree dirs rows are gone"
        );
    }

    // ------------------ Task 11: directory size aggregation --------------

    /// Fixture with known byte sizes at three levels:
    /// fixture/{readme.md(5), empty/, docs/{notes.txt(10), final.pdf(1),
    /// inner/{deep.bin(100), a.txt(2)}}, media/{b.jpg(7), c.png(7)}} —
    /// fixture totals 132 bytes / 7 files; docs 113/4; inner 102/2;
    /// media 14/2; empty 0/0.
    fn write_size_fixture(base: &Path) -> PathBuf {
        let root = base.join("fixture");
        fs::create_dir_all(root.join("docs").join("inner")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("media")).unwrap();
        fs::write(root.join("readme.md"), b"hello").unwrap();
        fs::write(root.join("docs").join("notes.txt"), b"0123456789").unwrap();
        fs::write(root.join("docs").join("final.pdf"), b"x").unwrap();
        fs::write(
            root.join("docs").join("inner").join("deep.bin"),
            vec![0u8; 100],
        )
        .unwrap();
        fs::write(root.join("docs").join("inner").join("a.txt"), b"ab").unwrap();
        fs::write(root.join("media").join("b.jpg"), b"1234567").unwrap();
        fs::write(root.join("media").join("c.png"), b"abcdefg").unwrap();
        root
    }

    /// Scans [`write_size_fixture`] and returns the warmed engine.
    fn size_fixture_engine(tmp: &tempfile::TempDir) -> Engine {
        let engine = open_engine(&tmp.path().join("index.db"), &tmp.path().join("fixture"));
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, _files, _dirs) = wait_scan_finished(&rx);
        engine
    }

    /// Known-fixture aggregation: every level's size/file_count and the
    /// size-descending (ties: name ascending) child ordering.
    #[test]
    fn size_tree_aggregates_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        write_size_fixture(tmp.path());
        let engine = size_fixture_engine(&tmp);
        let sep = std::path::MAIN_SEPARATOR;

        let tree = engine
            .size_tree("fixture", 2, 40)
            .expect("the scanned root segment resolves");
        assert_eq!(tree.name, "fixture");
        assert_eq!(tree.path, "fixture");
        assert_eq!(tree.size, 132, "sum of ALL descendant file bytes");
        assert_eq!(tree.file_count, 7, "files only — dirs never count");

        // Level 1: size-descending, dirs and the direct file interleaved.
        // The empty dir is ABSENT by contract: the aggregation walks the
        // interned dirs tree, and an empty dir never gets a dirs row
        // (nothing was ever upserted into it) — it aggregates nothing and
        // its path can never be a navigable root.
        let names: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["docs", "media", "readme.md"]);
        assert!(tree.children.iter().all(|c| c.name != "empty"));

        let docs = &tree.children[0];
        assert_eq!(docs.path, format!("fixture{sep}docs"));
        assert_eq!((docs.size, docs.file_count), (113, 4));

        // Level 2: docs' children mix a dir and files, size-descending;
        // inner's own aggregates are complete but its children are NOT
        // materialized (two levels below the root is all depth 2 buys).
        let docs_names: Vec<&str> = docs.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(docs_names, vec!["inner", "notes.txt", "final.pdf"]);
        let inner = &docs.children[0];
        assert_eq!(inner.path, format!("fixture{sep}docs{sep}inner"));
        assert_eq!((inner.size, inner.file_count), (102, 2));
        assert!(
            inner.children.is_empty(),
            "depth 2: inner's leaves are level 3"
        );

        // A same-size tie sorts by name ascending.
        let media = &tree.children[1];
        assert_eq!((media.size, media.file_count), (14, 2));
        let media_names: Vec<&str> = media.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(media_names, vec!["b.jpg", "c.png"]);

        // A file child is a leaf carrying its own size and file_count 1.
        let readme = &tree.children[2];
        assert_eq!((readme.size, readme.file_count), (5, 1));
        assert_eq!(readme.path, format!("fixture{sep}readme.md"));
        assert!(readme.children.is_empty());
    }

    #[test]
    fn size_tree_depth_and_max_children() {
        let tmp = tempfile::tempdir().unwrap();
        write_size_fixture(tmp.path());
        let engine = size_fixture_engine(&tmp);
        let sep = std::path::MAIN_SEPARATOR;
        let docs_path = format!("fixture{sep}docs");

        // depth 0: full aggregates, no children at all.
        let shallow = engine.size_tree("fixture", 0, 40).unwrap();
        assert_eq!((shallow.size, shallow.file_count), (132, 7));
        assert!(shallow.children.is_empty(), "depth 0 -> children empty");

        // Truncation: docs has 3 children; with max_children 2 the
        // overflow merges into a trailing "(other)" node that counts
        // against the budget.
        let truncated = engine.size_tree(&docs_path, 1, 2).unwrap();
        assert_eq!(truncated.size, 113);
        assert_eq!(truncated.children.len(), 2, "(other) counts as one child");
        assert_eq!(truncated.children[0].name, "inner");
        assert_eq!(
            (truncated.children[0].size, truncated.children[0].file_count),
            (102, 2)
        );
        let other = &truncated.children[1];
        assert_eq!(other.name, "(other)");
        assert_eq!(other.path, format!("{docs_path}{sep}(other)"));
        assert_eq!((other.size, other.file_count), (11, 2), "notes + final");
        assert!(other.children.is_empty());

        // max_children 1: only "(other)" remains, carrying everything.
        let single = engine.size_tree(&docs_path, 1, 1).unwrap();
        assert_eq!(single.children.len(), 1);
        assert_eq!(single.children[0].name, "(other)");
        assert_eq!(
            (single.children[0].size, single.children[0].file_count),
            (113, 4)
        );

        // Everything fits: no "(other)" is synthesized (fixture's three
        // children: docs, media, readme.md — empty dirs carry no node).
        let full = engine.size_tree("fixture", 1, 40).unwrap();
        assert_eq!(full.children.len(), 3);
        assert!(full.children.iter().all(|c| c.name != "(other)"));

        // Depth 2 materializes inner's leaf files, size-descending.
        let deep = engine.size_tree(&docs_path, 2, 40).unwrap();
        let inner = &deep.children[0];
        let inner_names: Vec<&str> = inner.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(inner_names, vec!["deep.bin", "a.txt"]);
        assert_eq!(
            (inner.children[0].size, inner.children[0].file_count),
            (100, 1)
        );
    }

    #[test]
    fn size_tree_unknown_root_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_size_fixture(tmp.path());
        let engine = size_fixture_engine(&tmp);

        assert_eq!(engine.size_tree("nosuch", 2, 40), None);
        assert_eq!(engine.size_tree("fixture/nosuch", 2, 40), None);
        assert_eq!(engine.size_tree("fixture/no_such_dir/deeper", 2, 40), None);
        // A file mid-path can never have children below it.
        assert_eq!(engine.size_tree("fixture/readme.md/deeper", 2, 40), None);
    }

    #[test]
    fn size_tree_virtual_root_lists_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let vol_a = tmp.path().join("vol_a");
        let vol_b = tmp.path().join("vol_b");
        fs::create_dir_all(&vol_a).unwrap();
        fs::create_dir_all(&vol_b).unwrap();
        fs::write(vol_a.join("big1.txt"), vec![0u8; 50]).unwrap();
        fs::write(vol_b.join("small1.txt"), b"abc").unwrap();

        let engine = Engine::open(
            &tmp.path().join("index.db"),
            EngineOptions {
                volumes: vec![
                    vol_a.to_string_lossy().into_owned(),
                    vol_b.to_string_lossy().into_owned(),
                ],
                exclusions: Vec::new(),
            },
        )
        .unwrap();
        let rx = engine.subscribe();
        engine.start_full_scan();
        let (_progress, files, _dirs) = wait_scan_finished(&rx);
        assert_eq!(files, 2);
        let sep = std::path::MAIN_SEPARATOR;

        let all = engine
            .size_tree("", 2, 40)
            .expect("the virtual root always exists");
        assert_eq!(all.name, "(all)");
        assert_eq!(all.path, "");
        assert_eq!((all.size, all.file_count), (53, 2), "both volumes fold in");

        // One child per scan-root segment, size-descending.
        let segments: Vec<&str> = all.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(segments, vec!["vol_a", "vol_b"]);
        let va = &all.children[0];
        assert_eq!(va.path, "vol_a");
        assert_eq!((va.size, va.file_count), (50, 1));

        // Depth 2 reaches the files below each volume segment.
        let leaf = &va.children[0];
        assert_eq!(leaf.name, "big1.txt");
        assert_eq!(leaf.path, format!("vol_a{sep}big1.txt"));
        assert_eq!((leaf.size, leaf.file_count), (50, 1));
    }

    #[test]
    fn size_tree_file_root_is_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        write_size_fixture(tmp.path());
        let engine = size_fixture_engine(&tmp);
        let sep = std::path::MAIN_SEPARATOR;

        let tree = engine
            .size_tree(&format!("fixture{sep}readme.md"), 3, 40)
            .expect("a path that names a file resolves to a leaf");
        assert_eq!(tree.name, "readme.md");
        assert_eq!(tree.path, format!("fixture{sep}readme.md"));
        assert_eq!((tree.size, tree.file_count), (5, 1));
        assert!(tree.children.is_empty(), "a file is always a leaf");

        // Sanity: the dir-root path next to it still aggregates.
        let dir_tree = engine
            .size_tree(&format!("fixture{sep}docs"), 1, 40)
            .unwrap();
        assert_eq!((dir_tree.size, dir_tree.file_count), (113, 4));
    }

    // ---------------- v1.1: cached settings reads ------------------------

    /// `settings()` must be served from an in-engine cache, NOT the store
    /// mutex: the startup hot-index load and full scans hold that mutex
    /// for their whole duration, and the app's main thread reads the
    /// shortcut through `quick::init` inside `.setup` — a store-lock read
    /// there is the "window not responding for tens of seconds" bug. The
    /// test holds the store lock exactly the way a load would; a cached
    /// read returns instantly.
    #[test]
    fn settings_read_does_not_wait_on_store_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::open(&tmp.path().join("index.db"), EngineOptions::default()).unwrap();

        // Hold the store mutex the way the hot-index loader / a scan does.
        // The guard borrows the local Arc clone, so `engine` itself can
        // still move into the reader thread below.
        let store = Arc::clone(&engine.store);
        let _store_guard = lock_unpoisoned(&store);

        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let settings = engine.settings();
            tx.send(settings.shortcut).unwrap();
        });

        let shortcut = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("settings() must not wait on the store mutex");
        assert_eq!(shortcut, crate::settings::DEFAULT_SHORTCUT);
        reader.join().unwrap();
    }

    /// `set_settings` writes through to the store AND refreshes the cache
    /// in one step: the next `settings()` sees the new value without a
    /// store round-trip, and a fresh `Engine::open` over the same database
    /// reads it back from disk (cache and store never disagree).
    /// set_settings carries the memory policy live: a shrunken hot
    /// budget evicts to size immediately, no restart (v1.2 settings UI).
    #[test]
    fn policy_live_apply_through_set_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();

        let engine = open_engine(&db_path, &root);
        // Defaults from a fresh DB.
        assert_eq!(
            lock_unpoisoned(&engine.tiers).policy().hot_budget_bytes,
            64 * 1024 * 1024
        );
        // Shrink through the normal settings flow.
        let mut s = engine.settings();
        s.mem_hot_budget_mb = 8;
        engine.set_settings(&s).unwrap();
        assert_eq!(
            lock_unpoisoned(&engine.tiers).policy().hot_budget_bytes,
            8 * 1024 * 1024,
            "set_settings applies the policy without a restart"
        );
        // And it persists: a fresh engine over the same DB reads it back.
        drop(engine);
        let reopened = Engine::open(&db_path, EngineOptions::default()).unwrap();
        assert_eq!(
            lock_unpoisoned(&reopened.tiers).policy().hot_budget_bytes,
            8 * 1024 * 1024
        );
    }

    #[test]
    fn set_settings_roundtrip_cache_and_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let engine = Engine::open(&db, EngineOptions::default()).unwrap();

        let mut next = engine.settings();
        next.lang = "en".into();
        next.shortcut = "Ctrl+Space".into();
        engine.set_settings(&next).unwrap();

        // The cache reflects the write immediately.
        assert_eq!(engine.settings(), next);

        // The store holds it too: a fresh open (whose cache is preheated
        // from disk) reads the same values back.
        drop(engine);
        let reopened = Engine::open(&db, EngineOptions::default()).unwrap();
        assert_eq!(reopened.settings(), next);
    }

    // ---------------- v1.1: volume usage overlay -------------------------

    /// `match_volume` decides whether a pane root names a whole volume
    /// (the only view level that gets the physical-capacity overlay).
    /// Index paths and mount points spell the same volume differently
    /// (`C` from the index root segment vs `C:\` from sysinfo) — trailing
    /// separators are trimmed and comparison is ASCII case-insensitive.
    #[test]
    fn match_volume_normalizes_windows_paths() {
        let volumes = vec![
            crate::volumes::VolumeInfo {
                path: "C:\\".into(),
                label: "System".into(),
                total_bytes: 500,
                available_bytes: 100,
            },
            crate::volumes::VolumeInfo {
                path: "D:\\".into(),
                label: "Data".into(),
                total_bytes: 900,
                available_bytes: 300,
            },
        ];

        let c = |root: &str| match_volume(root, &volumes).map(|v| v.path.clone());
        assert_eq!(c("C:\\"), Some("C:\\".into()), "exact mount matches");
        assert_eq!(c("c:\\"), Some("C:\\".into()), "case-insensitive");
        assert_eq!(
            c("C:"),
            Some("C:\\".into()),
            "the index root segment (root_segment keeps the colon) matches"
        );
        assert_eq!(c("D:\\"), Some("D:\\".into()));
        assert_eq!(c("D:\\data"), None, "a subdirectory is not a volume");
        assert_eq!(c(""), None, "the (all) root never matches");
        assert_eq!(c("fixture"), None, "a custom scan root never matches");
        assert_eq!(c("C"), None, "a bare folder name is not a volume");
    }

    /// `volume_overlay` arithmetic on a synthetic volume list: `used =
    /// total - available`, `indexed` = the subtree total the hot index
    /// reports for the root, and `unindexed` clamps at zero when the
    /// indexed sum exceeds physical usage (sparse files / accounting
    /// skew) — a negative block must never reach the chart.
    #[test]
    fn volume_overlay_clamps_unindexed() {
        let tmp = tempfile::tempdir().unwrap();
        write_size_fixture(tmp.path());
        let engine = size_fixture_engine(&tmp);
        let indexed = engine
            .size_tree_dirs("fixture", 0, 0)
            .expect("fixture root resolves")
            .size;
        assert!(indexed > 0);

        // A "volume" whose used space (total - available) is smaller than
        // what the index adds up: unindexed must clamp to 0, not underflow.
        let tiny = vec![crate::volumes::VolumeInfo {
            path: "fixture".into(),
            label: "Fake".into(),
            total_bytes: 1000,
            available_bytes: 1000,
        }];
        let o = engine
            .volume_overlay_in("fixture", &tiny)
            .expect("matching volume yields an overlay");
        assert_eq!(o.used_bytes, 0);
        assert_eq!(o.indexed_bytes, indexed);
        assert_eq!(o.unindexed_bytes, 0, "saturating clamp at zero");

        // A healthy margin: used > indexed leaves the unindexed remainder.
        let big = vec![crate::volumes::VolumeInfo {
            path: "fixture".into(),
            label: "Fake".into(),
            total_bytes: indexed + 20_000,
            available_bytes: 5_000,
        }];
        let o = engine.volume_overlay_in("fixture", &big).unwrap();
        assert_eq!(o.used_bytes, indexed + 15_000);
        assert_eq!(o.unindexed_bytes, 15_000);

        // No matching volume (a custom root): no overlay at all.
        let volumes = vec![crate::volumes::VolumeInfo {
            path: "C:\\".into(),
            label: "System".into(),
            total_bytes: 1,
            available_bytes: 1,
        }];
        assert!(engine.volume_overlay_in("fixture", &volumes).is_none());
    }

    // ---------------- v1.2: dimension tiers --------------------------------

    /// Fixture: a scanned engine with one known file, its detail fetched
    /// once (so the value sits in a tier).
    fn detail_fixture(tmp: &tempfile::TempDir) -> (Engine, FileId) {
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("notes.txt"), b"hello").unwrap();
        let engine = open_engine(&tmp.path().join("index.db"), &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);
        let hit = engine.search("notes", 0, 5);
        assert_eq!(hit.total, 1);
        (engine, hit.hits[0].id)
    }

    /// The cold read must not queue behind the store mutex: with the main
    /// write connection HELD for two seconds, a detail fetch (cold miss —
    /// nothing cached) still completes fast. This is the freeze class
    /// v1.1 fought; the read-only WAL connection is the guarantee.
    #[test]
    fn readonly_connection_unblocked_by_write_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);

        let _store_guard = lock_unpoisoned(&engine.store);
        let started = Instant::now();
        let detail = engine
            .file_detail(id)
            .expect("the cold read succeeds while the writer holds its lock");
        assert_eq!(detail.id, id);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cold read took {:?} — it queued behind the store mutex",
            started.elapsed()
        );
    }

    /// A watcher upsert invalidates the cached detail: the store's
    /// hash-cache rule NULLs hashes when mtime+size change, so a cached
    /// hash that no longer exists must never be served after the update.
    #[test]
    fn stale_entry_invalidated_by_upsert() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);

        let first = engine.file_detail(id).expect("cold fetch works");
        // Simulate the watcher's invalidation callback (the engine wiring
        // calls invalidate on upserted ids; here: same ids path).
        engine.invalidate_dimensions(&[id]);
        // And prove the ladder re-reads: touch the row's ctime directly on
        // the store so the refetched value differs from any cache.
        {
            let mut store = lock_unpoisoned(&engine.store);
            store
                .conn_mut()
                .execute("UPDATE files SET ctime = 12345 WHERE id = ?1", [id])
                .unwrap();
        }
        let second = engine.file_detail(id).expect("refetch after invalidate");
        assert_eq!(
            second.ctime,
            Some(12345),
            "stale cached ctime must not be served"
        );
        assert_ne!(first.ctime, second.ctime);
    }

    /// Rescan completion flushes every tier entry, then preheats the hot
    /// tier with the ids it held (preload on by default).
    #[test]
    fn rescan_flushes_then_preheats() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);

        // Promote the id to hot: enough recorded hits under a tiny
        // threshold, then the flush+preheat cycle must keep it hot.
        // promote_hits 2: staging seed + one ladder hit qualifies the id.
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
            drop(tiers);
        }
        let _ = engine.file_detail(id); // staging
        let _ = engine.file_detail(id); // hit → promote path
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            assert!(tiers.get_hot(&id).is_some(), "precondition: id is hot");
        }

        engine.flush_dimensions_and_preheat();

        let mut tiers = lock_unpoisoned(&engine.tiers);
        assert!(
            tiers.get_hot(&id).is_some(),
            "preload re-inserts the previously-hot id"
        );
        assert_eq!(tiers.stats().staging_entries, 0, "everything was flushed");
    }

    // ------------- v1.4: heat persistence --------------------------------

    /// Heat persistence round trip: an id driven into the hot tier
    /// persists, and the startup preheat thread's body replays it into
    /// a FRESH engine over the same database — yesterday's hot file is
    /// today's first hot resident again, no re-earning from zero.
    #[test]
    fn persist_then_preheat_restores_heat() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);

        // Same low-floor policy as rescan_flushes_then_preheats: one
        // ladder hit qualifies the id for hot.
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
        }
        let _ = engine.file_detail(id); // staging
        let _ = engine.file_detail(id); // hit → promote path
        engine.persist_heat();

        // The restart: a fresh engine over the same DB, the same low
        // floor (the default 8 would floor the 1-hit score out at the
        // backfill gate), then the preheat thread's body.
        drop(engine);
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        let reopened = open_engine(&db_path, &root);
        {
            let mut tiers = lock_unpoisoned(&reopened.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
        }
        preheat_thread_side(&reopened.tiers, &db_path);
        let mut tiers = lock_unpoisoned(&reopened.tiers);
        assert!(
            tiers.get_hot(&id).is_some(),
            "the persisted snapshot restores the id into the fresh engine's hot tier"
        );
    }

    /// Offline decay at the backfill gate: a row three half-lives old
    /// decays 8×256 → 1×256 and drops below the default floor 8; the
    /// same score stamped now survives; a FUTURE stamp (clock skew)
    /// clamps elapsed to zero — the score stands, nothing panics.
    #[test]
    fn offline_decay_drops_below_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);
        let db_path = tmp.path().join("index.db");
        let half_life = crate::tiers::MemoryPolicy::default().decay_half_life;
        let now = unix_now();

        // Three half-lives offline: 2048 → 256, floored 1 < 8 — dropped.
        lock_unpoisoned(&engine.store)
            .replace_heat(&[(id, 8 * 256, now - 3 * half_life.as_secs() as i64)])
            .unwrap();
        preheat_thread_side(&engine.tiers, &db_path);
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            assert!(
                tiers.get_hot(&id).is_none(),
                "three half-lives of decay must floor the score out of the backfill"
            );
        }

        // Same score, stamped now: nothing decays, the id re-enters hot.
        lock_unpoisoned(&engine.store)
            .replace_heat(&[(id, 8 * 256, now)])
            .unwrap();
        preheat_thread_side(&engine.tiers, &db_path);
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            assert!(
                tiers.get_hot(&id).is_some(),
                "a fresh stamp survives the gate"
            );
        }

        // A future stamp: elapsed clamps at zero, the score stands.
        lock_unpoisoned(&engine.tiers).clear();
        lock_unpoisoned(&engine.store)
            .replace_heat(&[(id, 8 * 256, now + 3600)])
            .unwrap();
        preheat_thread_side(&engine.tiers, &db_path);
        let mut tiers = lock_unpoisoned(&engine.tiers);
        assert!(
            tiers.get_hot(&id).is_some(),
            "a future `at` decays nothing and must not panic"
        );
    }

    /// A snapshot row whose file no longer exists (deleted between the
    /// snapshot and the restart) silently disappears at the cold read —
    /// no error, no hot resident.
    #[test]
    fn stale_id_absent_in_preheat() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, _id) = detail_fixture(&tmp);
        let db_path = tmp.path().join("index.db");

        lock_unpoisoned(&engine.store)
            .replace_heat(&[(999_999, 100 * 256, unix_now())])
            .unwrap();
        preheat_thread_side(&engine.tiers, &db_path);

        let tiers = lock_unpoisoned(&engine.tiers);
        assert_eq!(
            tiers.stats().hot_entries,
            0,
            "a stale id drops out at the cold read — no panic, no resident"
        );
    }

    /// The floor is in HITS on both ends: one touch books raw 256
    /// (÷256 = 1), which must NOT persist under the default threshold
    /// 8 — the snapshot stays empty (an empty snapshot legitimately
    /// clears the table: nothing qualified).
    #[test]
    fn floor_units_one_touch_not_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);
        let db_path = tmp.path().join("index.db");

        // One touch: staging seed + one recorded hit = raw 256.
        let _ = engine.file_detail(id);
        lock_unpoisoned(&engine.tiers).record_hit(&id);

        engine.persist_heat();

        let rows = ColdReader::open(&db_path)
            .unwrap()
            .read_heat(HEAT_MAX_ROWS)
            .unwrap();
        assert!(
            rows.is_empty(),
            "a one-touch entry (raw 256) must not persist under the default floor 8"
        );
    }

    /// The quit-path persist must SKIP a busy store lock, never block
    /// the main thread: with the lock held the way a mid-scan exit
    /// would, the call returns in well under a second; once free, the
    /// same call writes the snapshot.
    #[test]
    fn exit_persist_skips_when_store_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);
        let db_path = tmp.path().join("index.db");

        // An id above the floor so the unlocked retry writes a row.
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
        }
        let _ = engine.file_detail(id); // staging
        let _ = engine.file_detail(id); // → hot, raw 256 ≥ floor 1

        let store = Arc::clone(&engine.store);
        let guard = lock_unpoisoned(&store);
        let started = Instant::now();
        engine.persist_heat_on_exit();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "exit persist took {:?} — it blocked on the held store lock",
            started.elapsed()
        );
        drop(guard);

        engine.persist_heat_on_exit();
        let rows = ColdReader::open(&db_path)
            .unwrap()
            .read_heat(HEAT_MAX_ROWS)
            .unwrap();
        assert!(
            !rows.is_empty(),
            "with the lock free the exit persist writes the snapshot"
        );
    }

    /// A rescan carries heat scores instead of zeroing them: an id
    /// driven to a floored score ≥ 5 before the flush still clears the
    /// floor afterwards — the zero-score reseed of the old
    /// `put_hot_direct` flush is gone.
    #[test]
    fn rescan_carries_scores() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);

        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
        }
        let _ = engine.file_detail(id); // staging
        let _ = engine.file_detail(id); // → hot, raw 256
        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            for _ in 0..8 {
                let _ = tiers.get_hot(&id); // each hit books +256
            }
            assert!(
                tiers.heat_above_floor(5).iter().any(|(k, _)| *k == id),
                "precondition: the id's floored score reaches 5"
            );
        }

        engine.flush_dimensions_and_preheat();

        let mut tiers = lock_unpoisoned(&engine.tiers);
        assert!(
            tiers.heat_above_floor(5).iter().any(|(k, _)| *k == id),
            "the flush carries the score — it must not zero it"
        );
    }

    /// The no-wipe guard: when the carry-back cold read FAILS, the rescan
    /// flush must SKIP its persist — the flush already emptied the heat
    /// map, so persisting would overwrite the last good snapshot with an
    /// empty table. The failure is forced with a db path no reader can
    /// open (a read-only SQLite open cannot create a file); production
    /// always passes the store's own db, so the seam stays test-only.
    #[test]
    fn rescan_flush_failed_carry_keeps_persisted_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (engine, id) = detail_fixture(&tmp);
        let db_path = tmp.path().join("index.db");

        {
            let mut tiers = lock_unpoisoned(&engine.tiers);
            tiers.set_policy(crate::tiers::MemoryPolicy {
                hot_threshold: 1,
                promote_hits: 2,
                ..crate::tiers::MemoryPolicy::default()
            });
        }
        let _ = engine.file_detail(id); // staging
        let _ = engine.file_detail(id); // → hot, raw 256
        engine.persist_heat();

        let before = ColdReader::open(&db_path)
            .unwrap()
            .read_heat(HEAT_MAX_ROWS)
            .unwrap();
        assert!(!before.is_empty(), "precondition: a snapshot exists");

        // The companion thread's body with an unopenable db path: the
        // flush runs (preload on), the carry fails at ColdReader::open,
        // and the persist must be skipped.
        rescan_flush_thread_side(&engine.tiers, &engine.store, &tmp.path().join("absent.db"));

        let after = ColdReader::open(&db_path)
            .unwrap()
            .read_heat(HEAT_MAX_ROWS)
            .unwrap();
        assert_eq!(
            after, before,
            "a failed carry must not wipe the persisted snapshot"
        );
    }
}
