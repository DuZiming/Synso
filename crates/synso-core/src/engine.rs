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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::collections::{self, Collection, MemberHit};
use crate::hotindex::HotIndex;
use crate::model::FileId;
use crate::query::{self, SearchResult};
use crate::scanner::{self, effective_roots, root_segment, ScanOptions};
use crate::store::Store;
use crate::Result;

/// What the engine scans and what it skips. Empty `volumes` means "all
/// fixed local volumes" (see [`crate::volumes::fixed_volumes`]);
/// `exclusions` are globset patterns as documented on
/// [`crate::scanner::ScanOptions`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineOptions {
    pub volumes: Vec<String>,
    pub exclusions: Vec<String>,
}

/// Phase of a dedup run. Defined here so the event stream keeps its shape
/// when dedup lands (Task 9); the engine does not emit dedup events yet.
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
    /// Reserved for Task 9.
    DedupProgress {
        phase: DedupPhase,
        done: u64,
        total: u64,
    },
    /// Reserved for Task 9.
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
}

impl Engine {
    /// Opens (creating if needed) the index database at `db_path` and
    /// starts the background full load of the hot index; `stats().warm`
    /// flips to `true` when that load completes.
    pub fn open(db_path: &Path, opts: EngineOptions) -> Result<Engine> {
        let store = Arc::new(Mutex::new(Store::open(db_path)?));
        let hot = Arc::new(RwLock::new(HotIndex::cold()));
        let engine = Engine {
            store: store.clone(),
            hot: hot.clone(),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            scan_opts: ScanOptions {
                roots: opts.volumes,
                exclusions: opts.exclusions,
            },
            scanning: Arc::new(AtomicBool::new(false)),
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
            move || scan_cycle(store, hot, subscribers, scanning, opts)
        });
        if let Err(err) = spawn {
            eprintln!("synso: failed to spawn the scan thread: {err}");
            self.scanning.store(false, Ordering::Release);
        }
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
    /// (see [`crate::query`] for the full contract). Ranking: prefix >
    /// word-start > substring, a within-class boost for mtimes within the
    /// last 30 days, then shorter path, then lexicographic path; an empty
    /// `text` lists the most recent entries by mtime (ties: path asc).
    /// `hits` covers `[offset, offset + limit)` of that ordering, `total`
    /// is the full match count. A cold index returns an empty result —
    /// this never panics.
    pub fn search(&self, text: &str, offset: u32, limit: u32) -> SearchResult {
        let hot = read_unpoisoned(&self.hot);
        query::run(&hot, text, offset, limit, unix_now())
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
fn broadcast(subscribers: &Arc<Mutex<Vec<Sender<EngineEvent>>>>, event: EngineEvent) {
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
/// this crate has no `unwrap`/`expect` in library code.
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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

/// Releases the one-scan-at-a-time flag on ANY exit from a scan cycle —
/// normal return or panic unwind — so a failed or panicked scan can never
/// leave `start_full_scan` a silent no-op forever.
struct ScanSlot<'a>(&'a AtomicBool);

impl Drop for ScanSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One full scan cycle on its own thread: purge each root's subtree, walk,
/// reload the hot index, broadcast the outcome. The scan slot is released
/// by [`ScanSlot`] on every exit path, panic included.
fn scan_cycle(
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    scanning: Arc<AtomicBool>,
    opts: ScanOptions,
) {
    let _slot = ScanSlot(&scanning);
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
}
