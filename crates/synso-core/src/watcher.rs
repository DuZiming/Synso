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
//! 1. under the STORE mutex alone: all removals — first, so a delete can
//!    never land on a row a sibling op in the same batch just wrote.
//!    Removals delete the row located by (dir segments, name) — never by
//!    rowid, which SQLite may have recycled between events — plus the
//!    removed path's subtree (catches children of a deleted directory
//!    even when their individual events were coalesced away); then all
//!    upserts (the store computes pinyin at its upsert choke point);
//! 2. the same ops apply INCREMENTALLY to the hot index
//!    ([`HotIndex::upsert_slot`] / [`HotIndex::remove_by_dir_name`]),
//!    under the canonical store -> hot pair with the store guard
//!    read-only — so the hot WRITE lock never spans pass 1's per-row
//!    store commits (Task 20 churn measurement: that span froze searches
//!    for a whole batch, 8.8s at a 400-file burst); wholesale reload
//!    stays the scan path;
//! 3. [`EngineEvent::FsChanged`] broadcasts as a UI refresh hint.
//!
//! Directory arrivals walk: an upsert that resolves to a DIRECTORY gets
//! its subtree indexed in-batch via [`scanner::scan_subtree`] — the OS
//! reports only the directory entry itself (ReadDirectoryChangesW /
//! inotify deliver no events for the children), so a populated create or
//! a rename/move would otherwise lose every descendant until the next
//! rescan. The walk runs under the store lock the batch already holds —
//! the exact discipline a full scan uses for its whole walk, watcher
//! batches queue meanwhile — and is bounded by the same exclusion matcher
//! and reparse-point guard; every walked row is hot-indexed through the
//! incremental upsert path.
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
//! Failure semantics: setup/watch errors are logged (`log::warn!` /
//! `log::error!`, the established engine pattern) and never fatal — a root
//! that cannot be watched only loses its live updates; a store error on one
//! batch entry skips that entry and the batch continues. Dropping the
//! [`Watcher`] handle signals the thread to stop (a final pending batch is
//! still applied, so the index reflects the last-seen disk state) and joins
//! it.

use std::collections::BTreeMap;
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
/// the entry's final component, `root_idx` the watch root this path
/// belongs to (the exclusion test's rel-base and the subtree walk's root).
#[derive(Debug)]
struct PendingOp {
    root_idx: usize,
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
        for (root_idx, (root, segment)) in self.roots.iter().zip(&self.root_segments).enumerate() {
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
                root_idx,
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
                log::warn!("synso: watching {root:?} failed: {err}");
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
                log::error!("synso: failed to spawn the watcher thread: {err}");
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
            Err(err) => log::warn!("synso: watcher event error: {err}"),
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
                    // Only REAL ops (re)open the quiet window: an event the
                    // matcher filtered (excluded subtree, db sidecars,
                    // outside every root) must not push the deadline out —
                    // a flood of filtered noise would otherwise keep
                    // sliding the window and starve the pending batch.
                    if absorb(ctx, event, pending) {
                        deadline = Some(Instant::now() + QUIET_WINDOW);
                    }
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
/// Returns whether at least one op was actually queued — filtered paths
/// contribute nothing and must not reopen the debounce window.
fn absorb(ctx: &Ctx, event: NotifyEvent, pending: &mut BTreeMap<PathBuf, PendingOp>) -> bool {
    let mut queued = false;
    for (path, op) in event_ops(&event) {
        if let Some(entry) = ctx.make_pending(&path, op) {
            pending.insert(path, entry);
            queued = true;
        }
    }
    queued
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
/// upserting phantom rows); (1) all removals, then all upserts — under the
/// STORE lock alone; (2) the same ops replayed into the hot index, under
/// the canonical store -> hot pair. Removals run before upserts so a
/// delete can never land on a row a sibling op in the same batch just
/// wrote (e.g. a case-renamed path whose two spellings are distinct map
/// keys).
///
/// The store/hot split is the Task 20 churn finding's fix: the original
/// version held the hot WRITE lock across pass 1's per-row autocommit
/// store writes, so a 400-file burst froze every search for the whole
/// batch — measured at 8.8s on this machine. Searches take only the hot
/// READ lock, so keeping pass 1 store-only (and pass 2's store access
/// read-only: `upsert_slot`'s ancestor reads hit the fully committed
/// store) shrinks the hot-lock hold to the in-memory replay, microseconds
/// per row. The watcher thread still blocks on the store mutex for the
/// batch duration — same store -> hot ordering as the scan path, so a
/// concurrent scan either finishes first (events safely queue in the
/// channel) or runs after this batch wholesale, and batches can never
/// interleave with a scan reload.
fn apply_batch(ctx: &Ctx, pending: &mut BTreeMap<PathBuf, PendingOp>) {
    if pending.is_empty() {
        return;
    }
    let mut removals: Vec<(&Path, &PendingOp)> = Vec::new();
    let mut upserts: Vec<(&Path, &PendingOp, FileRecord)> = Vec::new();
    for (path, entry) in pending.iter() {
        match entry.op {
            Op::Upsert => match std::fs::symlink_metadata(path) {
                Ok(meta) => upserts.push((
                    path,
                    entry,
                    scanner::new_record(&entry.name, &meta, meta.is_dir()),
                )),
                // Gone again by apply time: apply a removal instead.
                Err(_) => removals.push((path, entry)),
            },
            Op::Remove => removals.push((path, entry)),
        }
    }
    // The hot halves replayed in pass 2, in batch order (removals first).
    enum HotOp {
        Remove { segments: Vec<String>, name: String },
        Upsert(FileRecord),
    }
    let mut hot_ops: Vec<HotOp> = Vec::new();
    let mut applied = 0u32;
    {
        // Pass 1: store writes only — the hot index is not held, so
        // searches never wait on this pass's per-row commits.
        let mut store = lock_unpoisoned(&ctx.store);
        for (path, entry) in &removals {
            let segments: Vec<&str> = entry.segments.iter().map(String::as_str).collect();
            remove_path_store(&mut store, &segments, &entry.name, path);
            hot_ops.push(HotOp::Remove {
                segments: entry.segments.clone(),
                name: entry.name.clone(),
            });
            applied += 1;
        }
        for (path, entry, rec) in upserts {
            let segments: Vec<&str> = entry.segments.iter().map(String::as_str).collect();
            let mut rec = rec;
            match store.upsert_file(&segments, &mut rec) {
                Ok(()) => {
                    // A directory arrives as ONE event with no child
                    // events: a populated create, or a rename/move whose
                    // whole subtree silently rode along (the Remove side
                    // above already purged the old location). Walk the new
                    // subtree in-batch — bounded by the exclusion matcher
                    // and the reparse-point guard — and queue every walked
                    // row for the hot replay.
                    if rec.is_dir {
                        let mut dir_segments = entry.segments.clone();
                        dir_segments.push(entry.name.clone());
                        let mut walked: Vec<FileRecord> = Vec::new();
                        let mut store_row = |_store: &Store, walked_row: &FileRecord| {
                            walked.push(walked_row.clone());
                        };
                        match scanner::scan_subtree(
                            &mut store,
                            path,
                            &dir_segments,
                            &ctx.roots[entry.root_idx],
                            &ctx.matcher,
                            &mut store_row,
                        ) {
                            Ok(sub) if sub.errors > 0 => {
                                log::warn!(
                                    "synso: watcher subtree walk of {path:?} skipped {sub:?}"
                                )
                            }
                            Ok(_) => {}
                            Err(err) => {
                                log::error!("synso: watcher subtree walk of {path:?} failed: {err}")
                            }
                        }
                        hot_ops.extend(walked.into_iter().map(HotOp::Upsert));
                    }
                    hot_ops.push(HotOp::Upsert(rec));
                    applied += 1;
                }
                Err(err) => log::error!("synso: watcher upsert of {path:?} failed: {err}"),
            }
        }
        // Renames already contribute removals through their from-side.
    }
    {
        // Pass 2: hot replay under the canonical store -> hot order (the
        // store guard is read-only here: `upsert_slot`'s on-demand
        // ancestor/extension reads hit the fully committed store).
        let store = lock_unpoisoned(&ctx.store);
        let mut hot = write_unpoisoned(&ctx.hot);
        for op in &hot_ops {
            match op {
                HotOp::Remove { segments, name } => {
                    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
                    hot.remove_by_dir_name(&segments, name);
                }
                HotOp::Upsert(rec) => {
                    if let Err(err) = hot.upsert_slot(&store, rec) {
                        log::error!("synso: watcher hot upsert of {rec:?} failed: {err}");
                    }
                }
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
/// and never abort the batch. `pub(crate)`: the engine's recycle-bin
/// deletion (Task 10) syncs its trashed paths through this exact purge, so
/// the two removal paths cannot drift apart. The watcher's own batch
/// applies the two halves SEPARATELY (see [`apply_batch`]'s doc: the hot
/// lock must not span the batch's store writes) — this paired form stays
/// for the engine's one-shot deletions.
pub(crate) fn remove_path(
    store: &mut Store,
    hot: &mut HotIndex,
    segments: &[&str],
    name: &str,
    path: &Path,
) {
    remove_path_store(store, segments, name, path);
    hot.remove_by_dir_name(segments, name);
}

/// The store half of [`remove_path`]: the row delete plus the subtree
/// purge, store errors logged and never fatal (the watcher's pass 1).
fn remove_path_store(store: &mut Store, segments: &[&str], name: &str, path: &Path) {
    if let Err(err) = store.delete_file_by_path(segments, name) {
        log::error!("synso: watcher delete of {path:?} failed: {err}");
    }
    let mut subtree: Vec<&str> = segments.to_vec();
    subtree.push(name);
    if let Err(err) = store.delete_subtree(&subtree) {
        log::error!("synso: watcher subtree delete of {path:?} failed: {err}");
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
    fn watcher_dir_rename_keeps_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        let engine = open_engine(&db_path, &root, Vec::new());
        scan_once(&engine);
        engine.start_watcher().unwrap();

        // A populated tree, created while the watcher runs (each entry
        // arrives as its own event, so the pre-move index is complete).
        let moveme = root.join("moveme");
        fs::create_dir_all(moveme.join("l1").join("l2")).unwrap();
        fs::write(moveme.join("aaa.txt"), b"a").unwrap();
        fs::write(moveme.join("l1").join("bbb.txt"), b"b").unwrap();
        fs::write(moveme.join("l1").join("l2").join("ccc.txt"), b"c").unwrap();
        poll_until(Duration::from_secs(10), || {
            (search_total(&engine, "ccc") == 1).then_some(())
        });

        // THE regression: renaming a populated directory reports only the
        // directory entry itself — children move with zero events. The
        // Remove side purges the whole old subtree, so without an in-batch
        // subtree walk the descendants would vanish from the index until
        // the next rescan.
        fs::rename(&moveme, root.join("moved")).unwrap();

        // Every descendant is searchable at the NEW path (<=10s, brief).
        // The poll matches on the new tail directly: a total==1 hit at the
        // OLD path (pre-rename row) must not satisfy it.
        let sep = std::path::MAIN_SEPARATOR;
        for (text, tail) in [
            ("aaa", format!("moved{sep}aaa.txt")),
            ("bbb", format!("moved{sep}l1{sep}bbb.txt")),
            ("ccc", format!("moved{sep}l1{sep}l2{sep}ccc.txt")),
        ] {
            let hit = poll_until(Duration::from_secs(10), || {
                engine
                    .search(text, 0, 10)
                    .hits
                    .into_iter()
                    .find(|hit| hit.path.ends_with(&tail))
            });
            assert!(
                hit.path.ends_with(&tail),
                "{text} must resolve under the moved dir, got {hit:?}"
            );
        }

        // The old path is gone from search...
        poll_until(Duration::from_secs(10), || {
            (search_total(&engine, "moveme") == 0).then_some(())
        });
        // ...stats equal disk truth (4 fixture files + 3 moved)...
        let disk_files = disk_file_count(&root);
        assert_eq!(disk_files, 7);
        poll_until(Duration::from_secs(10), || {
            (engine.stats().files == disk_files).then_some(())
        });
        // ...and the store holds the new chain with no moveme segment.
        let store = Store::open(&db_path).unwrap();
        let dir_names: Vec<String> = store
            .all_dirs()
            .unwrap()
            .into_iter()
            .map(|(_, _, name)| name)
            .collect();
        assert!(
            !dir_names.contains(&"moveme".to_string()),
            "the old dir segment must be purged"
        );
        for kept in ["moved", "l1", "l2"] {
            assert!(
                dir_names.contains(&kept.to_string()),
                "dir row {kept} must exist at the new path"
            );
        }
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
        // index and stats().files never reaches disk truth). A second
        // populated tree is RENAMED — its descendants ride along with zero
        // child events, so the in-batch subtree walk must re-index them at
        // the new path.
        fs::create_dir_all(root.join("bye").join("inner")).unwrap();
        fs::write(root.join("bye").join("inner").join("gone.txt"), b"g").unwrap();
        fs::write(root.join("bye").join("also.txt"), b"a").unwrap();
        fs::create_dir_all(root.join("mvme").join("deep")).unwrap();
        fs::write(root.join("mvme").join("keep.txt"), b"k").unwrap();
        fs::write(root.join("mvme").join("deep").join("buried.txt"), b"b").unwrap();
        std::thread::sleep(Duration::from_millis(800)); // land as a batch first
        fs::remove_dir_all(root.join("bye")).unwrap();
        fs::rename(root.join("mvme"), root.join("mvdone")).unwrap();

        // After quiesce (brief: <=10s): hot stats equal disk truth.
        let disk_files = disk_file_count(&root);
        assert_eq!(disk_files, 106, "4 fixture + 100 storm + 2 moved files");
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

        // Spot-check survivors, deleted names, and the moved tree.
        assert_eq!(search_total(&engine, "s000"), 1, "even i survives in sub/");
        assert_eq!(search_total(&engine, "s001"), 0, "odd i was deleted");
        assert_eq!(search_total(&engine, "t003"), 0, "rename target deleted");
        let buried = engine.search("buried", 0, 10);
        assert_eq!(buried.total, 1, "the moved tree's deep file is indexed");
        assert!(
            buried.hits[0].path.contains("mvdone"),
            "buried.txt must live under the new dir name, got {:?}",
            buried.hits[0].path
        );
        assert_eq!(search_total(&engine, "mvme"), 0, "old dir name is gone");
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

    /// Regression: events the matcher FILTERS (here: an excluded subtree)
    /// must not push the debounce deadline out — a quiet window already
    /// open on real ops still closes on time, so the batch applies while
    /// the filtered noise keeps arriving. Before the fix, every filtered
    /// event slid the deadline forward, so a continuous noise stream
    /// starved the pending batch indefinitely.
    #[test]
    fn watcher_filtered_events_do_not_starve_quiet_window() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = write_fixture(tmp.path());
        let engine = open_engine(&db_path, &root, vec!["noise".to_string()]);
        scan_once(&engine);
        engine.start_watcher().unwrap();

        // A real op opens the quiet window (~t=0, deadline ~t+500ms).
        let real = root.join("real_op.txt");
        fs::write(&real, b"r").unwrap();
        std::thread::sleep(Duration::from_millis(100)); // let it queue first

        // Filtered noise, written continuously on another thread for 4s —
        // far past the window's close. Every event is a fresh excluded
        // path, so `make_pending` drops them all.
        let noise_dir = root.join("noise");
        fs::create_dir_all(&noise_dir).unwrap();
        let flooding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flood_flag = flooding.clone();
        let flood_thread = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(4);
            let mut i = 0u32;
            while Instant::now() < end {
                fs::write(noise_dir.join(format!("n{i}.tmp")), b"n").unwrap();
                i += 1;
            }
            flood_flag.store(false, std::sync::atomic::Ordering::SeqCst);
        });

        // The real op must land ON TIME (one window, brief: <=2s) — while
        // the flood is STILL RUNNING. Without the deadline fix the batch
        // waits for the flood to end (~4s) and this budget expires first.
        poll_until(Duration::from_secs(2), || {
            (search_total(&engine, "real_op") == 1).then_some(())
        });
        assert!(
            flooding.load(std::sync::atomic::Ordering::SeqCst),
            "the batch must apply while filtered noise is still streaming"
        );

        flood_thread.join().unwrap();
        // Noise stays excluded (ample time after the flood ended).
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(search_total(&engine, "noise"), 0, "noise never indexed");
    }
}
