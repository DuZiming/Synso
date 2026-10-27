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
//! running.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::hotindex::HotIndex;
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
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Starts a full scan on a background thread. One scan at a time:
    /// calling this while a scan is running is a no-op. Per root, the
    /// root's index subtree is purged before the walk (rescan
    /// reconciliation); when the scan finishes, the hot index is reloaded
    /// wholesale from the store and `ScanFinished` is emitted.
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
        let hot = self.hot.read().unwrap();
        EngineStats {
            files: hot.file_count(),
            dirs: hot.dir_count(),
            warm: hot.is_warm(),
            interned_bytes: hot.interned_bytes(),
        }
    }
}

/// Sends `event` to every live subscriber, dropping disconnected ones.
fn broadcast(subscribers: &Arc<Mutex<Vec<Sender<EngineEvent>>>>, event: EngineEvent) {
    subscribers
        .lock()
        .unwrap()
        .retain(|sender| sender.send(event.clone()).is_ok());
}

/// One full scan cycle on its own thread: purge each root's subtree, walk,
/// reload the hot index, broadcast `ScanFinished`, release the scan slot.
fn scan_cycle(
    store: Arc<Mutex<Store>>,
    hot: Arc<RwLock<HotIndex>>,
    subscribers: Arc<Mutex<Vec<Sender<EngineEvent>>>>,
    scanning: Arc<AtomicBool>,
    opts: ScanOptions,
) {
    let (files, dirs) = {
        let mut guard = store.lock().unwrap();
        purge_root_subtrees(&mut guard, &opts);
        let counts = match scanner::scan(&mut guard, &opts, &|done| {
            broadcast(&subscribers, EngineEvent::ScanProgress { done, total: 0 });
        }) {
            Ok(stats) => (stats.files, stats.dirs),
            Err(err) => {
                eprintln!("synso: full scan failed: {err}");
                (0, 0)
            }
        };
        // Reload while still holding the store lock (see module docs), so
        // subscribers that react to ScanFinished already see fresh stats.
        reload_hot_index_locked(&guard, &hot);
        counts
    };
    broadcast(&subscribers, EngineEvent::ScanFinished { files, dirs });
    scanning.store(false, Ordering::Release);
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
    let guard = store.lock().unwrap();
    reload_hot_index_locked(&guard, hot);
}

fn reload_hot_index_locked(store: &Store, hot: &Arc<RwLock<HotIndex>>) {
    match HotIndex::load(store) {
        Ok(loaded) => *hot.write().unwrap() = loaded,
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

    fn engine_opts(root: &Path) -> EngineOptions {
        EngineOptions {
            volumes: vec![root.to_string_lossy().into_owned()],
            exclusions: Vec::new(),
        }
    }

    fn open_engine(db: &Path, root: &Path) -> Engine {
        Engine::open(db, engine_opts(root)).unwrap()
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
}
