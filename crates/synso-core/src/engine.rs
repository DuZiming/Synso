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
use crate::store::Store;
use crate::watcher::Watcher;
use crate::Result;

/// Delay between the first successful `ScanFinished` and the engine's
/// automatic dedup kick-off (the design's "idle-time start"; the dedup
/// view calls `dedup_start()` to run it right now instead).
const DEDUP_AUTOSTART_DELAY: Duration = Duration::from_secs(30);

/// What the engine scans and what it skips. Empty `volumes` means "all
/// fixed local volumes" (see [`crate::volumes::fixed_volumes`]);
/// `exclusions` are globset patterns as documented on
/// [`crate::scanner::ScanOptions`].
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
    scan_opts: ScanOptions,
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
    /// Effective scan roots (plain spellings), precomputed once at open so
    /// path resolution never re-enumerates volumes per hashed file.
    roots: Vec<String>,
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
            roots: effective_roots(&scan_opts.roots),
            subscribers: Arc::clone(&subscribers),
        });
        let engine = Engine {
            store: Arc::clone(&store),
            hot: hot.clone(),
            subscribers,
            scan_opts,
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
            eprintln!("synso: failed to spawn the hot-index loader: {err}");
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
            let opts = self.scan_opts.clone();
            let dedup = Arc::clone(&self.dedup);
            let autostarted = Arc::clone(&self.dedup_autostarted);
            move || scan_cycle(store, hot, subscribers, scanning, opts, dedup, autostarted)
        });
        if let Err(err) = spawn {
            eprintln!("synso: failed to spawn the scan thread: {err}");
            self.scanning.store(false, Ordering::Release);
        }
    }

    /// Starts the filesystem watcher (see [`crate::watcher`]): recursive
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
        let roots = effective_roots(&self.scan_opts.roots);
        let watcher = Watcher::start(
            &roots,
            &self.scan_opts.exclusions,
            &self.db_path,
            Arc::clone(&self.store),
            Arc::clone(&self.hot),
            Arc::clone(&self.subscribers),
        )?;
        *slot = Some(watcher);
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

    /// Case-insensitive substring search over the hot index's name arena
    /// (see [`crate::query`] for the full contract). `text` is first
    /// parsed by [`crate::query::parse_query`]: Everything-style modifier
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
    /// [`crate::query::parse_query`]). The parsed dimension filters
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
                    eprintln!(
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
    /// `Err(SynsoError::Conflict)`. See [`crate::collections`].
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

    /// Every collection, id-ordered. A store error is logged and yields an
    /// empty list (listing is a read-side convenience, never fatal).
    pub fn coll_list(&self) -> Vec<Collection> {
        collections::list(&lock_unpoisoned(&self.store)).unwrap_or_else(|err| {
            eprintln!("synso: listing collections failed: {err}");
            Vec::new()
        })
    }

    /// Adds file OR folder row ids to a collection, snapshotting each
    /// entry's current name and size (the later rebind key).
    pub fn coll_add(&self, id: i64, members: &[FileId]) -> Result<()> {
        collections::add(&mut lock_unpoisoned(&self.store), id, members)
    }

    // ---------------- Task 9: dedup state machine ------------------------

    /// Starts a dedup run on a background thread; a no-op while one is
    /// already running. The pipeline (see [`crate::dedup`]) groups by size,
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
    /// [`crate::query::Hit`]s from the hot index. Files deleted since the
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
    /// [`crate::query::Hit::path`]), resolved by matching the path's first
    /// segment against the engine's effective scan roots. `None` when no
    /// root's segment matches. Hashing (Task 9), trash (Task 10) and
    /// reveal (Task 15) all key on this: index paths deliberately carry no
    /// drive prefix, so the engine owns the root mapping. Public for the
    /// same reason as [`crate::scanner::is_placeholder_attr`] — its
    /// consumers straddle crate and app layers.
    pub fn resolve_absolute(&self, index_path: &str) -> Option<PathBuf> {
        dedup::resolve_absolute_path(&self.dedup.roots, index_path)
    }

    /// Removes one member row (alive or tombstoned) from a collection.
    pub fn coll_remove(&self, id: i64, member: FileId) -> Result<()> {
        collections::remove(&mut lock_unpoisoned(&self.store), id, member)
    }

    /// The collection's members, tombstoned ones included (their `hit` is
    /// synthesized from the snapshot — see [`crate::collections::MemberHit`]).
    /// A store error is logged and yields an empty list.
    pub fn coll_members(&self, id: i64) -> Vec<MemberHit> {
        collections::members(&lock_unpoisoned(&self.store), id).unwrap_or_else(|err| {
            eprintln!("synso: listing members of collection {id} failed: {err}");
            Vec::new()
        })
    }
}

/// Sends `event` to every live subscriber, dropping disconnected ones.
/// `pub(crate)`: the watcher (Task 8) broadcasts [`EngineEvent::FsChanged`]
/// through the same list.
pub(crate) fn broadcast(subscribers: &Arc<Mutex<Vec<Sender<EngineEvent>>>>, event: EngineEvent) {
    lock_unpoisoned(subscribers).retain(|sender| sender.send(event.clone()).is_ok());
}

/// Current Unix-epoch seconds (0 if the clock reads pre-epoch); the query
/// time the search recency window is measured against.
fn unix_now() -> i64 {
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
                eprintln!("synso: collection reconcile after scan failed: {err}");
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
            eprintln!("synso: failed to spawn the dedup thread: {err}");
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
    let outcome = dedup::run_pipeline(&dedup.store, &dedup.roots, &reader, &progress);
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
            eprintln!("synso: failed to purge the index subtree of {root:?}: {err}");
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
        Err(err) => eprintln!("synso: hot-index load failed: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

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
}
