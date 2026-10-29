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
//! cannot wedge the engine. A successful scan also runs the collections
//! reconcile (tombstone + rebind, see [`crate::collections`]) inside the
//! same store critical section, before `ScanFinished` is emitted.
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
//! Dedup (Task 9): [`Engine::dedup_start`] runs the staged hashing
//! pipeline (see [`crate::dedup`]) on its own thread — one run at a time,
//! the slot claimed by CAS and released by the same drop-guard pattern as
//! the scan slot. Hashing runs entirely OUTSIDE the store lock; only the
//! snapshot read and short batched hash persists take it. One run is also
//! auto-started 30 seconds after the first successful `ScanFinished` (the
//! timer holds only a `Weak` to the dedup state, so dropping the engine
//! cancels a pending kick-off instead of hashing behind the app's back).
//! [`Engine::resolve_absolute`] maps index-relative paths back to real
//! disk paths — hashing needs them, and trash (Task 10) and reveal
//! (Task 15) will reuse the same mapping.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::collections::{self, Collection, MemberHit};
use crate::dedup::{self, DedupState, DupeGroup, StoredGroup};
use crate::hotindex::HotIndex;
use crate::model::{FileId, ParsedQuery};
use crate::query::{self, SearchResult};
use crate::scanner::{self, effective_roots, root_segment, ScanOptions};
use crate::settings::{self, Settings};
use crate::sizeagg::{self, SizeNode};
use crate::store::Store;
use crate::trash;
use crate::watcher::{self, Watcher};
use crate::Result;

/// Delay between the first successful `ScanFinished` and the engine's
/// automatic dedup kick-off (the design's "idle-time start"; the dedup
/// view calls `dedup_start()` to run it right now instead).
const DEDUP_AUTOSTART_DELAY: Duration = Duration::from_secs(30);

/// What the engine scans and what it skips. Empty `volumes` means "all
/// fixed local volumes" (`crate::volumes::fixed_volumes`); `exclusions`
/// are globset patterns as documented on `crate::scanner::ScanOptions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineOptions {
    pub volumes: Vec<String>,
    pub exclusions: Vec<String>,
}

/// Phase of a dedup run (Task 9). The engine walks `Idle -> Grouping ->
/// HashingFirst -> HashingFull -> Done` (intermediate stages are skipped
/// when they have no work); `Grouping` reappears as the in-progress phase
/// of every new run.
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
    HashingFull,
    Done,
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
    /// `done == total ==` the hash computations the run performed.
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
    /// Set on the first successful `ScanFinished` so the automatic dedup
    /// kick-off happens exactly once per engine.
    dedup_autostarted: Arc<AtomicBool>,
}

/// State shared between the engine, the dedup worker thread and the
/// auto-start timer (Task 9).
struct DedupShared {
    /// One dedup run at a time: CAS claim, released by a drop guard.
    running: AtomicBool,
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
        let hot = Arc::new(RwLock::new(HotIndex::cold()));
        let scan_opts = ScanOptions {
            roots: opts.volumes,
            exclusions: opts.exclusions,
        };
        let subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let dedup = Arc::new(DedupShared {
            running: AtomicBool::new(false),
            book: Mutex::new(DedupBookkeeping {
                state: DedupState::default(),
                groups: Arc::new(Vec::new()),
            }),
            store: Arc::clone(&store),
            roots: RwLock::new(effective_roots(&scan_opts.roots)),
            subscribers: Arc::clone(&subscribers),
        });
        let engine = Engine {
            store: Arc::clone(&store),
            hot: hot.clone(),
            subscribers,
            scan_opts: Mutex::new(scan_opts),
            scanning: Arc::new(AtomicBool::new(false)),
            db_path: db_path.to_path_buf(),
            watcher: Mutex::new(None),
            dedup,
            dedup_autostarted: Arc::new(AtomicBool::new(false)),
        };
        if let Err(err) = thread::Builder::new()
            .name("synso-hot-load".into())
            .spawn(move || reload_hot_index(&store, &hot))
        {
            log::error!("synso: failed to spawn the hot-index loader: {err}");
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
            let autostarted = Arc::clone(&self.dedup_autostarted);
            move || scan_cycle(store, hot, subscribers, scanning, opts, dedup, autostarted)
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
        let watcher = Watcher::start(
            &roots,
            &exclusions,
            &self.db_path,
            Arc::clone(&self.store),
            Arc::clone(&self.hot),
            Arc::clone(&self.subscribers),
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

    // ---------------- Task 13: user settings -----------------------------

    /// The persisted user settings (see [`crate::settings`]). Missing
    /// fields read as their documented defaults, and a store read error
    /// degrades to the default for that field — settings are UI
    /// convenience, never a startup failure.
    pub fn settings(&self) -> Settings {
        settings::load(&lock_unpoisoned(&self.store))
    }

    /// Persists `settings` to the store's meta table; store errors surface
    /// to the caller. The change applies to the field's consumers on their
    /// next read (the engine holds no in-memory copy).
    pub fn set_settings(&self, settings: &Settings) -> Result<()> {
        settings::save(&lock_unpoisoned(&self.store), settings)
    }

    /// Case-insensitive substring search over the hot index's name arena
    /// (see `crate::query` for the full contract). `text` is first
    /// parsed by [`crate::parse_query`]: Everything-style modifier
    /// tokens (`ext:` / `size:` / `path:` / `coll:`) become dimension
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
    /// (`ext:` / `size:` / `path:` / `coll:`) apply as fast predicates
    /// BEFORE ranking and classification; ranking itself is unchanged.
    /// `q.collections` resolves against the store: a file must be an ALIVE
    /// member of every named collection, an unknown name filters the result
    /// to empty (not an error), and a store error is logged and yields an
    /// empty result (search is never fatal).
    pub fn search_parsed(&self, q: &ParsedQuery, offset: u32, limit: u32) -> SearchResult {
        // Resolve the coll: filter against the store before touching the
        // hot index (membership rows live only there). The store guard is
        // released before the hot read; the engine's locks never nest.
        let coll_members = if q.collections.is_empty() {
            None
        } else {
            let store = lock_unpoisoned(&self.store);
            match collections::alive_member_intersection(&store, &q.collections) {
                // Ok(None) = an unknown collection name: no results, not
                // an error — and NOT "no filter", hence the early return.
                Ok(members) => match members {
                    Some(members) => Some(members),
                    None => return SearchResult::default(),
                },
                Err(err) => {
                    log::error!(
                        "synso: resolving collection filter {:?} failed: {err}",
                        q.collections
                    );
                    return SearchResult::default();
                }
            }
        };
        let hot = read_unpoisoned(&self.hot);
        query::run_parsed(&hot, q, coll_members.as_ref(), offset, limit, unix_now())
    }

    /// Creates a virtual collection. A duplicate name is
    /// `Err(SynsoError::Conflict)`. See `crate::collections`.
    pub fn coll_create(&self, name: &str) -> Result<Collection> {
        collections::create(&mut lock_unpoisoned(&self.store), name)
    }

    /// Renames a collection (`Err(NotFound)` for an unknown id,
    /// `Err(Conflict)` when the name is taken).
    pub fn coll_rename(&self, id: i64, name: &str) -> Result<()> {
        collections::rename(&mut lock_unpoisoned(&self.store), id, name)
    }

    /// Deletes a collection and every member row it owns.
    pub fn coll_delete(&self, id: i64) -> Result<()> {
        collections::delete(&mut lock_unpoisoned(&self.store), id)
    }

    /// Every collection, id-ordered (creation order). Store errors surface
    /// to the caller rather than degrading to an empty list.
    pub fn coll_list(&self) -> Result<Vec<Collection>> {
        collections::list(&lock_unpoisoned(&self.store))
    }

    /// Adds file OR folder row ids to a collection, snapshotting each
    /// entry's current name and size (the later rebind key).
    pub fn coll_add(&self, id: i64, members: &[FileId]) -> Result<()> {
        collections::add(&mut lock_unpoisoned(&self.store), id, members)
    }

    // ---------------- Task 9: dedup state machine ------------------------

    /// Starts a dedup run on a background thread; a no-op while one is
    /// already running. The pipeline (see `crate::dedup`) groups by size,
    /// hashes first 4 KiB for same-size survivors, then whole files —
    /// persisted hashes are reused, so repeat runs are incremental.
    /// `Idle -> Grouping` happens synchronously here; subscribers receive
    /// [`EngineEvent::DedupProgress`] / [`EngineEvent::DedupFinished`] and
    /// pollers read [`Engine::dedup_state`]. The engine also auto-starts
    /// one run 30s after the first successful scan finishes.
    pub fn dedup_start(&self) {
        launch_dedup(&self.dedup);
    }

    /// Snapshot of the dedup state machine. While a run is going,
    /// `done`/`total` mirror the latest progress event; at
    /// [`DedupPhase::Done`] they equal the hash computations this run
    /// performed (0 = fully incremental). `groups` is the last COMPLETED
    /// run's group count; `errors` counts files skipped unreadable or as
    /// cloud placeholders.
    pub fn dedup_state(&self) -> DedupState {
        lock_unpoisoned(&self.dedup.book).state
    }

    /// The last completed run's groups, `[offset, offset + limit)` of the
    /// size-descending order, members materialized as search-style
    /// [`crate::Hit`]s from the hot index. Files deleted since the
    /// run drop out; a group falling below two surviving members dissolves
    /// (its slot in the ordering disappears, the other groups keep their
    /// relative order).
    pub fn dedup_groups(&self, offset: u32, limit: u32) -> Vec<DupeGroup> {
        if limit == 0 {
            return Vec::new();
        }
        // Snapshot the stored groups (cheap Arc clone) and release the
        // bookkeeping lock before touching the hot index — the engine's
        // locks never nest.
        let groups = Arc::clone(&lock_unpoisoned(&self.dedup.book).groups);
        let hot = read_unpoisoned(&self.hot);
        let mut skipped: u32 = 0;
        let mut page = Vec::new();
        for group in groups.iter() {
            let members: Vec<crate::query::Hit> = group
                .members
                .iter()
                .filter_map(|id| hot.slot_of_id(*id).map(|slot| query::hit_of(&hot, slot)))
                .collect();
            if members.len() < 2 {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            page.push(DupeGroup {
                hash: group.hash,
                size: group.size,
                members,
            });
            if page.len() == limit as usize {
                break;
            }
        }
        page
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
    /// spelling; both separators accepted, directories included) — the
    /// path → id bridge the app's `file_id_of` command (Task 18) needs,
    /// because flame-chart nodes carry paths rather than row ids: adding a
    /// flame node to a collection resolves it here first, then `coll_add`s
    /// the id. `None` when the path matches nothing (unknown path, or the
    /// virtual root `""` — it names no single entry).
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

    /// Removes one member row (alive or tombstoned) from a collection.
    pub fn coll_remove(&self, id: i64, member: FileId) -> Result<()> {
        collections::remove(&mut lock_unpoisoned(&self.store), id, member)
    }

    /// The collection's members, tombstoned ones included (their `hit` is
    /// synthesized from the snapshot — see [`MemberHit`]). Store errors
    /// surface to the caller rather than degrading to an empty list.
    pub fn coll_members(&self, id: i64) -> Result<Vec<MemberHit>> {
        collections::members(&lock_unpoisoned(&self.store), id)
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
    /// sweep on the hot index) under one store -> hot lock pair, and — when
    /// at least one path landed — the collections reconcile (tombstone +
    /// rebind) runs in the same store critical section. A path the trash
    /// batch refused is logged and keeps its index rows; the next rescan
    /// heals any drift. Returns the success count.
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
            if deleted > 0 {
                // Tombstone + rebind the collections in the same store
                // critical section a scan's and a watcher batch's reconcile
                // use; best-effort, like those.
                if let Err(err) = collections::reconcile(&mut store) {
                    log::warn!("synso: collection reconcile after trash failed: {err}");
                }
            }
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
/// by [`SlotGuard`] on every exit path, panic included. On success this is
/// ALSO where the engine's one-time automatic dedup kick-off is scheduled
/// (30s later, see [`DEDUP_AUTOSTART_DELAY`]).
#[allow(clippy::too_many_arguments)]
fn scan_cycle(
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    scanning: Arc<AtomicBool>,
    opts: ScanOptions,
    dedup: Arc<DedupShared>,
    autostarted: Arc<AtomicBool>,
) {
    let _slot = SlotGuard(&scanning);
    let outcome = {
        let mut guard = lock_unpoisoned(&store);
        purge_root_subtrees(&mut guard, &opts);
        scanner::scan(&mut guard, &opts, &|done| {
            broadcast(&subscribers, EngineEvent::ScanProgress { done, total: 0 });
        })
        .map(|stats| {
            // Success only: swap in the fresh snapshot while still holding
            // the store lock (see module docs), so subscribers reacting to
            // ScanFinished already see fresh stats. On failure the hot
            // index keeps serving the last known-good snapshot.
            reload_hot_index_locked(&guard, &hot);
            // Collection reconcile (tombstone + rebind, see `collections`)
            // runs in the same store critical section, after the hot
            // reload and before ScanFinished, so subscribers see member
            // state consistent with the fresh index. Best-effort: the scan
            // itself succeeded, so a reconcile error is logged, not fatal.
            if let Err(err) = collections::reconcile(&mut guard) {
                log::warn!("synso: collection reconcile after scan failed: {err}");
            }
            (stats.files, stats.dirs)
        })
    };
    match outcome {
        Ok((files, dirs)) => {
            broadcast(&subscribers, EngineEvent::ScanFinished { files, dirs });
            // Auto-start (Task 9): exactly one run, 30s after the FIRST
            // successful scan. The timer holds only a `Weak` to the dedup
            // state — an engine dropped meanwhile cancels the kick-off (the
            // upgrade fails) instead of hashing behind the app's back. A
            // spawn failure just loses this best-effort kick-off.
            if !autostarted.swap(true, Ordering::AcqRel) {
                let dedup = Arc::downgrade(&dedup);
                let _ = thread::Builder::new()
                    .name("synso-dedup-autostart".into())
                    .spawn(move || {
                        thread::sleep(DEDUP_AUTOSTART_DELAY);
                        if let Some(dedup) = dedup.upgrade() {
                            launch_dedup(&dedup);
                        }
                    });
            }
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
/// so pollers never miss it) and spawns the worker thread. A no-op when a
/// run is already going. Returns whether a run was started.
fn launch_dedup(dedup: &Arc<DedupShared>) -> bool {
    if dedup
        .running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    {
        let mut book = lock_unpoisoned(&dedup.book);
        book.state = DedupState {
            phase: DedupPhase::Grouping,
            ..DedupState::default()
        };
        // `groups` keeps the previous run's list: dedup_groups goes on
        // serving it until the new run finishes.
    }
    let spawn = thread::Builder::new().name("synso-dedup".into()).spawn({
        let dedup = Arc::clone(dedup);
        move || dedup_cycle(dedup)
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

/// One dedup cycle on its own thread: run the staged pipeline against the
/// store (hashing happens outside the lock — only the snapshot read and
/// short persist batches take it), fold the outcome into the state
/// machine, then broadcast the final progress and `DedupFinished`. The
/// slot is released by [`SlotGuard`] on every exit path, panic included.
/// The cycle is eprintln-free by design: every skip is counted in
/// [`DedupState::errors`] instead, and store failures merely degrade the
/// run — they never panic it.
fn dedup_cycle(dedup: Arc<DedupShared>) {
    let _slot = SlotGuard(&dedup.running);
    let subscribers = Arc::clone(&dedup.subscribers);
    let progress = |phase: DedupPhase, done: u64, total: u64| {
        {
            let mut book = lock_unpoisoned(&dedup.book);
            book.state.phase = phase;
            book.state.done = done;
            book.state.total = total;
        }
        broadcast(
            &subscribers,
            EngineEvent::DedupProgress { phase, done, total },
        );
    };
    let reader = dedup::RealFileReader;
    // One snapshot of the live root mapping for this run (the scope can
    // change mid-run; the next run picks it up).
    let roots = read_unpoisoned(&dedup.roots).clone();
    let outcome = dedup::run_pipeline(&dedup.store, &roots, &reader, &progress);
    let groups = outcome.groups.len() as u64;
    let hashed = outcome.hashed_first + outcome.hashed_full;
    {
        let mut book = lock_unpoisoned(&dedup.book);
        book.state = DedupState {
            phase: DedupPhase::Done,
            done: hashed,
            total: hashed,
            groups,
            // Both skip kinds share the public counter (the design's
            // "marked skipped, not grouped"); the pipeline outcome keeps
            // them apart for tests.
            errors: outcome.errors + outcome.placeholders,
        };
        book.groups = Arc::new(outcome.groups);
    }
    broadcast(&subscribers, EngineEvent::DedupFinished { groups });
}

/// Rescan reconciliation: delete each root's whole index subtree before
/// walking it, so the root is rebuilt from disk truth. Root segments come
/// from the scanner's own `root_segment`; roots must be non-overlapping
/// (see its docs). Purging every root up front still satisfies "purged
/// before walking that root" for each root, since nothing is walked yet.
fn purge_root_subtrees(store: &mut Store, opts: &ScanOptions) {
    for root in effective_roots(&opts.roots) {
        let segment = root_segment(Path::new(&root));
        if let Err(err) = store.delete_subtree(&[segment.as_str()]) {
            log::error!("synso: failed to purge the index subtree of {root:?}: {err}");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SynsoError;
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
            serde_json::to_string(&DedupPhase::Done).unwrap(),
            "\"done\""
        );
        assert_eq!(
            serde_json::from_str::<DedupPhase>("\"hashing_first\"").unwrap(),
            DedupPhase::HashingFirst
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
        // ...even though the store is now partial: purged, never re-walked.
        let store = Store::open(&db_path).unwrap();
        assert_eq!(
            store.all_files().unwrap().len(),
            0,
            "the store stays partial until a successful rescan"
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

    /// Store errors behind the collection reads SURFACE: `coll_list` and
    /// `coll_members` return `Err` instead of silently degrading to empty
    /// lists. Dropping the underlying tables on the engine's own connection
    /// is the cheapest way to force real store errors.
    #[test]
    fn coll_reads_surface_store_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());

        let engine = open_engine(&db_path, &root);
        let coll = engine.coll_create("keep").unwrap();
        assert_eq!(engine.coll_list().unwrap().len(), 1);
        assert_eq!(engine.coll_members(coll.id).unwrap().len(), 0);

        // Corrupt the schema out from under the engine's store connection:
        // both reads must now report the store error to the caller.
        {
            let mut store = lock_unpoisoned(&engine.store);
            store
                .conn_mut()
                .execute("DROP TABLE collection_members", [])
                .unwrap();
            store
                .conn_mut()
                .execute("DROP TABLE collections", [])
                .unwrap();
        }

        assert!(
            matches!(engine.coll_list(), Err(SynsoError::Db(_))),
            "coll_list surfaces the store error, not an empty list"
        );
        assert!(
            matches!(engine.coll_members(coll.id), Err(SynsoError::Db(_))),
            "coll_members surfaces the store error, not an empty list"
        );
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
        assert_eq!(page.len(), 1);
        let group = &page[0];
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
            .iter()
            .flat_map(|g| &g.members)
            .all(|m| m.name != "other.txt"));
        assert!(engine.dedup_groups(1, 10).is_empty());
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

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        engine.dedup_start();
        let (groups, _progress) = wait_dedup_finished(&rx);
        assert_eq!(groups, 0, "zero-size files never group");
        assert!(engine.dedup_groups(0, 10).is_empty());
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
        assert!(engine.dedup_groups(0, 10).is_empty());
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
        assert_eq!(engine.dedup_groups(0, 10)[0].members.len(), 2);
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
    /// search, leaves the store, and its collection membership is
    /// tombstoned (alive=0) with the name snapshot kept for a later rebind.
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

        // A collection tracks the doomed file so the tombstone is visible.
        let coll = engine.coll_create("purge-test").unwrap();
        engine.coll_add(coll.id, &[doomed_id]).unwrap();
        assert!(engine.coll_members(coll.id).unwrap()[0].alive);

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
        // ...and tombstoned in the collection (alive=0, snapshot kept).
        let members = engine.coll_members(coll.id).unwrap();
        assert_eq!(members.len(), 1);
        assert!(!members[0].alive, "the deleted member must be tombstoned");
        assert_eq!(members[0].hit.name, "doomed.txt");
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
}
