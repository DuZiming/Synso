//! Filesystem watcher (Task 8): incremental index updates from OS file
//! events, so the index stays fresh between scans without rescanning.
//!
//! Shape: recursive [`notify`] watches on the effective scan roots — plain
//! spellings, because `notify` takes ordinary paths (unlike the scanner,
//! which traverses `\\?\`-verbatim forms against MAX_PATH; a verbatim root
//! still works here, since event paths are built from the exact spelling
//! the watch was registered with). Events drain into a channel owned by one
//! debounce thread, which merges them into per-path FINAL states: within a
//! 500ms quiet window the last event for a path wins
//! (`Create`/`Modify` -> upsert, `Remove` -> remove, `Rename(from, to)` ->
//! remove `from` + upsert `to`; Windows usually splits renames into
//! `Remove(from)` + `Create(to)` pairs, which the per-path merge reduces to
//! exactly the same two operations). When the window goes quiet, ONE batch
//! is applied in three passes:
//!
//! 0. every upsert path is stat'd OUTSIDE any lock (metadata fresh from
//!    disk; a path that vanished again demotes to a removal, converging
//!    half-renames and same-window delete races instead of upserting
//!    phantom rows);
//! 1. under the store mutex (held only for the batch): all removals —
//!    first, so a delete can never land on a row a sibling op in the same
//!    batch just wrote. Removals delete the row located by (dir segments,
//!    name) — never by rowid, which SQLite may have recycled between
//!    events — plus the removed path's subtree (catches children of a
//!    deleted directory even when their individual events were coalesced
//!    away); then all upserts (the store computes pinyin at its upsert
//!    choke point);
//! 2. the same ops apply INCREMENTALLY to the hot index
//!    ([`HotIndex::upsert_slot`] / [`HotIndex::remove_by_dir_name`]);
//!    wholesale reload stays the scan path;
//! 3. if the batch contained deletions or renames,
//!    [`crate::collections::reconcile`] runs under the same store lock
//!    (tombstone + rebind);
//! 4. [`EngineEvent::FsChanged`] broadcasts as a UI refresh hint.
//!
//! Queue-during-scan: a running scan holds the store mutex for its whole
//! walk, so the debounce thread simply BLOCKS on the mutex at batch-apply
//! time while events keep piling up in the (unbounded) channel — nothing is
//! ever dropped, and the batch lands right after the scan releases the
//! lock. The scan's own wholesale hot reload happens under the same mutex,
//! so watcher batches can never interleave with it.
//!
//! Accepted corner: events arriving from under junction/reparse subtrees
//! may upsert rows the scanner would prune (the watcher deliberately does
//! not chase reparse ancestry per event). The next rescan purges and
//! rebuilds each root from disk truth, so the index self-heals; per-event
//! ancestry checks would cost a stat per event for a corner the rescan
//! already fixes.
//!
//! Self-feedback filter: the SQLite database's own writes must never loop
//! back into the index (a database living inside a watched root would
//! ping-pong forever), so events for the db file and its journal sidecars
//! are dropped at absorb time.
//!
//! Failure semantics: setup/watch errors are logged (`eprintln!`, the
//! established engine pattern) and never fatal — a root that cannot be
//! watched only loses its live updates; a store error on one batch entry
//! skips that entry and the batch continues. Dropping the [`Watcher`]
//! handle signals the thread to stop (a final pending batch is still
//! applied, so the index reflects the last-seen disk state) and joins it.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use globset::GlobSet;
use notify::event::{ModifyKind, RenameMode};
use notify::{
    recommended_watcher, Event as NotifyEvent, EventHandler, EventKind, RecommendedWatcher,
    RecursiveMode, Watcher as NotifyWatcher,
};

use crate::collections;
use crate::engine::{broadcast, EngineEvent};
use crate::hotindex::HotIndex;
use crate::model::FileRecord;
use crate::scanner;
use crate::store::Store;
use crate::{Result, SynsoError};

/// Events collect for this long after the LAST event before the batch is
/// applied (the debounce quiet window from the design).
const QUIET_WINDOW: Duration = Duration::from_millis(500);

/// Wake-up granularity while no window is open (no pending ops): only a
/// safety net — events and the stop signal wake the thread immediately.
const SLOW_POLL: Duration = Duration::from_secs(600);

/// Per-path final disposition after merging a whole quiet window of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Final state on disk is "exists": upsert with fresh metadata.
    Upsert,
    /// Final state on disk is "gone": delete the row (and subtree).
    Remove,
}

/// One pending op with its index spelling precomputed at absorb time:
/// `segments` is the containing directory's `dirs`-table chain (root
/// segment first — the same spelling the scanner upserts under), `name`
/// the entry's final component.
#[derive(Debug)]
struct PendingOp {
    segments: Vec<String>,
    name: String,
    op: Op,
}

/// Everything the debounce thread needs, captured once at start.
struct Ctx {
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    matcher: GlobSet,
    /// Watch roots, in the spelling the watches were registered with (so
    /// `strip_prefix` on event paths always succeeds).
    roots: Vec<PathBuf>,
    /// Parallel `scanner::root_segment` per root (the first `dirs` segment).
    root_segments: Vec<String>,
    /// Lowercased database path + journal sidecars: events for these are
    /// self-feedback from the store's own writes and are dropped.
    db_filter: Vec<String>,
}

impl Ctx {
    /// Turns one raw event path into a pending op, or `None` when it must
    /// not be indexed: outside every root, the root itself, the database's
    /// own files, excluded by the scan's globset patterns, or non-UTF-8
    /// (skipped exactly like the scanner skips unstoreable names).
    fn make_pending(&self, path: &Path, op: Op) -> Option<PendingOp> {
        let lower = path.to_string_lossy().to_lowercase();
        if self.db_filter.contains(&lower) {
            return None;
        }
        for (root, segment) in self.roots.iter().zip(&self.root_segments) {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                return None; // the root itself is never indexed
            }
            let name = rel.file_name().and_then(|n| n.to_str())?;
            if scanner::excluded(&self.matcher, path, root) {
                return None;
            }
            let mut segments = Vec::with_capacity(rel.components().count() + 1);
            segments.push(segment.clone());
            if let Some(parent) = rel.parent() {
                for component in parent.components() {
                    segments.push(component.as_os_str().to_str()?.to_string());
                }
            }
            return Some(PendingOp {
                segments,
                name: name.to_string(),
                op,
            });
        }
        None
    }
}

/// Handle keep-alive for the debounce thread. Dropping it stops the thread
/// (after one final batch drain) and joins it; while it lives, the thread
/// owns the notify watches and keeps applying batches.
pub struct Watcher {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Watcher {
    /// Starts watching `roots` (recursive) and applying debounced batches
    /// to `store` + `hot`, broadcasting [`EngineEvent::FsChanged`] to
    /// `subscribers` after each applied batch. `exclusions` are the same
    /// globset patterns the scan prunes with; `db_path` feeds the
    /// self-feedback filter. Errors only from matcher compilation, notify
    /// watcher creation, or thread spawn; per-root watch failures are
    /// logged and never disable the other roots.
    pub fn start(
        roots: &[String],
        exclusions: &[String],
        db_path: &Path,
        store: Arc<Mutex<Store>>,
        hot: Arc<RwLock<HotIndex>>,
        subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    ) -> Result<Watcher> {
        let matcher = scanner::build_matcher(exclusions)?;
        let (event_tx, event_rx) = unbounded();
        let (stop_tx, stop_rx) = unbounded::<()>();
        let mut fs_watcher: RecommendedWatcher =
            recommended_watcher(EventForwarder { tx: event_tx }).map_err(notify_error)?;
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        let root_segments: Vec<String> = roots.iter().map(|r| scanner::root_segment(r)).collect();
        for root in &roots {
            if let Err(err) = NotifyWatcher::watch(&mut fs_watcher, root, RecursiveMode::Recursive)
            {
                eprintln!("synso: watching {root:?} failed: {err}");
            }
        }
        let mut db_filter = vec![db_path.to_string_lossy().to_lowercase()];
        for sidecar in ["-journal", "-wal", "-shm"] {
            let mut sidecar_path = db_path.as_os_str().to_os_string();
            sidecar_path.push(sidecar);
            db_filter.push(sidecar_path.to_string_lossy().to_lowercase());
        }
        let ctx = Ctx {
            store,
            hot,
            subscribers,
            matcher,
            roots,
            root_segments,
            db_filter,
        };
        let spawn = std::thread::Builder::new()
            .name("synso-watcher".into())
            .spawn(move || {
                // The notify watcher must outlive the loop: it is moved
                // here on purpose, so dropping this handle stops watching.
                let _fs_watcher = fs_watcher;
                let mut pending = BTreeMap::new();
                run_loop(&ctx, &event_rx, &stop_rx, &mut pending);
                // Final drain: stopping with a non-empty window still
                // applies it, so the index reflects last-seen disk state.
                apply_batch(&ctx, &mut pending);
            });
        match spawn {
            Ok(thread) => Ok(Watcher {
                stop: stop_tx,
                thread: Some(thread),
            }),
            Err(err) => {
                eprintln!("synso: failed to spawn the watcher thread: {err}");
                Err(SynsoError::Io(err))
            }
        }
    }
}

/// Forwards notify events into the debounce channel. notify's own errors
/// (e.g. an overflow of the OS event buffer) are logged, never fatal.
struct EventForwarder {
    tx: Sender<NotifyEvent>,
}

impl EventHandler for EventForwarder {
    fn handle_event(&mut self, event: notify::Result<NotifyEvent>) {
        match event {
            Ok(event) => {
                let _ = self.tx.send(event);
            }
            Err(err) => eprintln!("synso: watcher event error: {err}"),
        }
    }
}

/// The debounce loop: absorb events, sliding a 500ms quiet-window deadline
/// forward on every arrival; once the window holds, apply ONE batch. See
/// the module docs for the lock/queueing story.
fn run_loop(
    ctx: &Ctx,
    event_rx: &Receiver<NotifyEvent>,
    stop_rx: &Receiver<()>,
    pending: &mut BTreeMap<PathBuf, PendingOp>,
) {
    let mut deadline: Option<Instant> = None;
    loop {
        if let Some(at) = deadline {
            if Instant::now() >= at {
                apply_batch(ctx, pending);
                deadline = None;
                continue;
            }
        }
        let wait = deadline.map_or(SLOW_POLL, |at| at.saturating_duration_since(Instant::now()));
        select! {
            recv(event_rx) -> msg => match msg {
                Ok(event) => {
                    absorb(ctx, event, pending);
                    deadline = if pending.is_empty() {
                        None
                    } else {
                        Some(Instant::now() + QUIET_WINDOW)
                    };
                }
                Err(_) => return, // sender gone: nothing watchable remains
            },
            recv(stop_rx) -> _ => return,
            default(wait) => {} // woke at the deadline (or a slow poll)
        }
    }
}

/// Merges one notify event into the pending map: per path, the latest
/// disposition wins (create-then-delete becomes a removal,
/// delete-then-create an upsert — disk truth at apply time decides).
fn absorb(ctx: &Ctx, event: NotifyEvent, pending: &mut BTreeMap<PathBuf, PendingOp>) {
    for (path, op) in event_ops(&event) {
        if let Some(entry) = ctx.make_pending(&path, op) {
            pending.insert(path, entry);
        }
    }
}

/// Reduces a notify event to (path, op) pairs. Rename kinds map their two
/// paths to remove-from + upsert-to; a single-path `From`/`To` half maps
/// accordingly; unknown name modes fall back to upsert — an upsert of a
/// path that vanished by apply time degrades into a removal there, so the
/// fallback converges from either direction. Access (reads) and unknown
/// kinds never change the index.
fn event_ops(event: &NotifyEvent) -> Vec<(PathBuf, Op)> {
    let paths = &event.paths;
    let mut ops = Vec::new();
    match event.kind {
        EventKind::Create(_) => ops.extend(paths.iter().map(|p| (p.clone(), Op::Upsert))),
        EventKind::Remove(_) => ops.extend(paths.iter().map(|p| (p.clone(), Op::Remove))),
        EventKind::Modify(ModifyKind::Name(mode)) => match mode {
            RenameMode::From => ops.extend(paths.first().map(|p| (p.clone(), Op::Remove))),
            RenameMode::To => ops.extend(paths.first().map(|p| (p.clone(), Op::Upsert))),
            RenameMode::Both if paths.len() >= 2 => {
                ops.push((paths[0].clone(), Op::Remove));
                ops.push((paths[1].clone(), Op::Upsert));
            }
            _ => ops.extend(paths.iter().map(|p| (p.clone(), Op::Upsert))),
        },
        EventKind::Modify(_) => ops.extend(paths.iter().map(|p| (p.clone(), Op::Upsert))),
        EventKind::Access(_) | EventKind::Other | EventKind::Any => {}
    }
    ops
}

/// Applies one debounced batch in three passes: (0) stat every upsert path
/// OUTSIDE any lock (fresh metadata, and vanished paths demote to removals
/// — converging half-renames and same-window delete races instead of
/// upserting phantom rows); (1) all removals; (2) all upserts. Removals run
/// before upserts so a delete can never land on a row a sibling op in the
/// same batch just wrote (e.g. a case-renamed path whose two spellings are
/// distinct map keys). Both locks are held for passes 1-2 only — the same
/// store -> hot ordering the scan path uses — so a concurrent scan either
/// finishes first (this thread blocked on the store mutex, events safely
/// queued in the channel) or runs after this batch wholesale.
fn apply_batch(ctx: &Ctx, pending: &mut BTreeMap<PathBuf, PendingOp>) {
    if pending.is_empty() {
        return;
    }
    let mut removals: Vec<(&Path, &PendingOp)> = Vec::new();
    let mut upserts: Vec<(&Path, &PendingOp, FileRecord)> = Vec::new();
    for (path, entry) in pending.iter() {
        match entry.op {
            Op::Upsert => match std::fs::symlink_metadata(path) {
                Ok(meta) => upserts.push((path, entry, record_from_meta(&meta, &entry.name))),
                // Gone again by apply time: apply a removal instead.
                Err(_) => removals.push((path, entry)),
            },
            Op::Remove => removals.push((path, entry)),
        }
    }
    let mut applied = 0u32;
    {
        let mut store = lock_unpoisoned(&ctx.store);
        let mut hot = write_unpoisoned(&ctx.hot);
        for (path, entry) in &removals {
            let segments: Vec<&str> = entry.segments.iter().map(String::as_str).collect();
            remove_path(&mut store, &mut hot, &segments, &entry.name, path);
            applied += 1;
        }
        for (path, entry, rec) in upserts {
            let segments: Vec<&str> = entry.segments.iter().map(String::as_str).collect();
            let mut rec = rec;
            match store.upsert_file(&segments, &mut rec) {
                Ok(()) => {
                    if let Err(err) = hot.upsert_slot(&store, &rec) {
                        eprintln!("synso: watcher hot upsert of {path:?} failed: {err}");
                    }
                    applied += 1;
                }
                Err(err) => eprintln!("synso: watcher upsert of {path:?} failed: {err}"),
            }
        }
        // Renames already contribute removals through their from-side.
        if !removals.is_empty() {
            if let Err(err) = collections::reconcile(&mut store) {
                eprintln!("synso: watcher collection reconcile failed: {err}");
            }
        }
    }
    pending.clear();
    if applied > 0 {
        broadcast(&ctx.subscribers, EngineEvent::FsChanged);
    }
}

/// Removes one path's row (located by dir segments + name) and, when the
/// path was a directory, its whole subtree — a no-op for files, whose
/// names never resolve to a `dirs` row. Errors are logged per statement
/// and never abort the batch.
fn remove_path(store: &mut Store, hot: &mut HotIndex, segments: &[&str], name: &str, path: &Path) {
    if let Err(err) = store.delete_file_by_path(segments, name) {
        eprintln!("synso: watcher delete of {path:?} failed: {err}");
    }
    let mut subtree: Vec<&str> = segments.to_vec();
    subtree.push(name);
    if let Err(err) = store.delete_subtree(&subtree) {
        eprintln!("synso: watcher subtree delete of {path:?} failed: {err}");
    }
    hot.remove_by_dir_name(segments, name);
}

/// Builds a fresh [`FileRecord`] from a stat, mirroring the scanner's
/// field conventions (directories record size 0; hashes and pinyin are
/// computed elsewhere — pinyin by the store's upsert choke point).
fn record_from_meta(meta: &Metadata, name: &str) -> FileRecord {
    let is_dir = meta.is_dir();
    FileRecord {
        id: 0,
        parent: 0,
        name: name.to_string(),
        size: if is_dir { 0 } else { meta.len() },
        mtime: scanner::unix_secs(meta.modified()),
        ctime: scanner::unix_secs(meta.created()),
        atime: scanner::unix_secs(meta.accessed()),
        ext_id: 0,
        is_dir,
        hash_first: None,
        hash_full: None,
        py_full: String::new(),
        py_init: String::new(),
    }
}

/// notify errors carry no io conversion; wrap the rendered message (the
/// engine pattern is logged, rendered causes).
fn notify_error(err: notify::Error) -> SynsoError {
    SynsoError::Io(std::io::Error::other(err.to_string()))
}

/// Poison-tolerant locking, mirroring the engine's helpers (see
/// `crate::engine` module docs: the store's invariants survive a panic).
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `lock_unpoisoned`, for the hot-index write side.
fn write_unpoisoned<T>(rwlock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rwlock
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use crate::engine::{Engine, EngineEvent, EngineOptions};
    use crate::store::Store;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Fixture tree: `fixture/{readme.md, seed.txt, sub/, docs/{notes.txt,
    /// final.pdf, empty_dir}}` — 4 files, 3 directories (`sub` included so
    /// the storm test can rename into a pre-scanned directory).
    fn write_fixture(base: &Path) -> PathBuf {
        let root = base.join("fixture");
        fs::create_dir_all(root.join("docs").join("empty_dir")).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("readme.md"), b"hello").unwrap();
        fs::write(root.join("sub").join("seed.txt"), b"seed").unwrap();
        fs::write(root.join("docs").join("notes.txt"), b"0123456789").unwrap();
        fs::write(root.join("docs").join("final.pdf"), b"x").unwrap();
        root
    }

    fn open_engine(db: &Path, root: &Path, exclusions: Vec<String>) -> Engine {
        Engine::open(
            db,
            EngineOptions {
                volumes: vec![root.to_string_lossy().into_owned()],
                exclusions,
            },
        )
        .unwrap()
    }

    /// Scans once and waits for `ScanFinished` so the index starts warm.
    fn scan_once(engine: &Engine) {
        let rx = engine.subscribe();
        engine.start_full_scan();
        loop {
            match rx.recv_timeout(Duration::from_secs(60)).unwrap() {
                EngineEvent::ScanProgress { .. } => continue,
                EngineEvent::ScanFinished { .. } => return,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    /// Polls `f` every 50ms until it returns `Some`, panicking if `budget`
    /// elapses first. Returns the witnessed value.
    fn poll_until<T>(budget: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "condition not met within {budget:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn search_total(engine: &Engine, text: &str) -> u64 {
        engine.search(text, 0, 10).total
    }

    /// Recursive count of non-directory entries under `root` (disk truth).
    fn disk_file_count(root: &Path) -> u64 {
        let mut count = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let file_type = entry.file_type().unwrap();
                if file_type.is_dir() {
                    stack.push(entry.path());
                } else {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn watcher_single_create_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        let engine = open_engine(&db_path, &root, Vec::new());
        scan_once(&engine);
        assert_eq!(engine.stats().files, 4);
        engine.start_watcher().unwrap();
        let rx = engine.subscribe();

        // Create: poll search until the file appears (brief: <=5s).
        let created = root.join("watched_new.txt");
        fs::write(&created, b"fresh").unwrap();
        let hit = poll_until(Duration::from_secs(5), || {
            let result = engine.search("watched_new", 0, 10);
            (result.total == 1).then(|| result.hits[0].clone())
        });
        assert!(
            hit.path.ends_with("watched_new.txt"),
            "path {hit:?} must end in the file name"
        );
        assert!(!hit.is_dir);

        // Delete: poll search until it disappears (brief: <=5s).
        fs::remove_file(&created).unwrap();
        poll_until(Duration::from_secs(5), || {
            (search_total(&engine, "watched_new") == 0).then_some(())
        });
        assert_eq!(engine.stats().files, 4, "back to the fixture count");

        // The store row is truly gone, not just the hot slot.
        let store = Store::open(&db_path).unwrap();
        assert!(store
            .all_files()
            .unwrap()
            .iter()
            .all(|rec| rec.name != "watched_new.txt"));

        // Each applied batch broadcast FsChanged while we polled.
        let mut fs_changed = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, EngineEvent::FsChanged) {
                fs_changed += 1;
            }
        }
        assert!(fs_changed > 0, "FsChanged must be broadcast per batch");
    }

    #[test]
    fn watcher_rename_updates_path() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        let engine = open_engine(&db_path, &root, Vec::new());
        scan_once(&engine);
        engine.start_watcher().unwrap();

        fs::rename(
            root.join("docs").join("notes.txt"),
            root.join("docs").join("renamed.txt"),
        )
        .unwrap();

        // The new path appears (with the docs chain intact)...
        let hit = poll_until(Duration::from_secs(5), || {
            let result = engine.search("renamed", 0, 10);
            (result.total == 1).then(|| result.hits[0].clone())
        });
        assert!(
            hit.path.contains("docs"),
            "path {hit:?} keeps the dir chain"
        );
        assert!(hit.path.ends_with("renamed.txt"));

        // ...and the old name is gone from search and from the store.
        poll_until(Duration::from_secs(5), || {
            (search_total(&engine, "notes.txt") == 0).then_some(())
        });
        let store = Store::open(&db_path).unwrap();
        let names: Vec<String> = store
            .all_files()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert!(!names.contains(&"notes.txt".to_string()));
        assert!(names.contains(&"renamed.txt".to_string()));
        assert_eq!(engine.stats().files, 4, "a rename is a move, not an add");
    }

    #[test]
    fn watcher_converges_after_event_storm() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        let engine = open_engine(&db_path, &root, Vec::new());
        scan_once(&engine);
        engine.start_watcher().unwrap();

        // 200 rapid create/rename/delete cycles: even i leaves a survivor in
        // sub/, odd i renames then deletes. End state: 100 storm files.
        for i in 0..200u32 {
            let a = root.join(format!("s{i:03}.txt"));
            fs::write(&a, [i as u8; 8]).unwrap();
            if i % 2 == 0 {
                fs::rename(&a, root.join("sub").join(format!("s{i:03}.txt"))).unwrap();
            } else {
                let b = root.join(format!("t{i:03}.txt"));
                fs::rename(&a, &b).unwrap();
                fs::remove_file(&b).unwrap();
            }
        }

        // Directory lifecycle: a nested tree gets indexed as its own batch,
        // then removed wholesale. The children's events sort AFTER the
        // parent dir's, so this is the convergence proof for the purge's
        // child-slot sweep (without it the children linger in the hot
        // index and stats().files never reaches disk truth).
        fs::create_dir_all(root.join("bye").join("inner")).unwrap();
        fs::write(root.join("bye").join("inner").join("gone.txt"), b"g").unwrap();
        fs::write(root.join("bye").join("also.txt"), b"a").unwrap();
        std::thread::sleep(Duration::from_millis(800)); // land as a batch first
        fs::remove_dir_all(root.join("bye")).unwrap();

        // After quiesce (brief: <=10s): hot stats equal disk truth.
        let disk_files = disk_file_count(&root);
        assert_eq!(disk_files, 104, "4 fixture files + 100 survivors");
        poll_until(Duration::from_secs(10), || {
            (engine.stats().files == disk_files).then_some(())
        });
        // SQLite holds no duplicate rows: the UNIQUE(dir,name) pair count
        // equals the row count, and non-dir rows equal disk truth.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        let distinct: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM (SELECT dir, name FROM files GROUP BY dir, name)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let file_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM files WHERE is_dir = 0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(distinct, rows, "no duplicate (dir,name) rows");
        assert_eq!(file_rows, disk_files as i64, "store non-dir rows == disk");
        assert_eq!(engine.stats().files as i64, file_rows, "hot index == store");

        // Spot-check survivors and deleted names.
        assert_eq!(search_total(&engine, "s000"), 1, "even i survives in sub/");
        assert_eq!(search_total(&engine, "s001"), 0, "odd i was deleted");
        assert_eq!(search_total(&engine, "t003"), 0, "rename target deleted");
    }

    #[test]
    fn watcher_ignores_excluded_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        // Excluded both at scan time and in the watcher (same patterns).
        let exclusions = vec!["skipme".to_string()];
        let engine = open_engine(&db_path, &root, exclusions);
        scan_once(&engine);
        assert_eq!(engine.stats().files, 4);
        engine.start_watcher().unwrap();

        // A file under the excluded subtree, created AFTER the scan: the
        // watcher must not apply it.
        fs::create_dir_all(root.join("skipme")).unwrap();
        fs::write(root.join("skipme").join("late.txt"), b"x").unwrap();
        // Positive control proving the watcher is alive and applying.
        fs::write(root.join("keep2.txt"), b"k").unwrap();
        poll_until(Duration::from_secs(5), || {
            (search_total(&engine, "keep2") == 1).then_some(())
        });

        // Give any (wrong) application of the excluded path ample time to
        // land: more than a debounce window after the control appeared.
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(search_total(&engine, "late"), 0, "excluded path ignored");
        let store = Store::open(&db_path).unwrap();
        assert!(
            store
                .all_files()
                .unwrap()
                .iter()
                .all(|rec| rec.name != "late.txt"),
            "no store row for the excluded path either"
        );
    }
}
