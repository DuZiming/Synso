//! Dedup pipeline (Task 9): staged hashing that finds content-identical
//! files without ever reading the whole disk blindly.
//!
//! Stages (see [`run_pipeline`]):
//! 1. **Grouping** — a size histogram straight from the store snapshot;
//!    only sizes carried by `>= 2` non-directory rows with `size > 0`
//!    become candidates. Zero-size files never enter a group (they would
//!    make every empty file a dupe of every other).
//! 2. **HashingFirst** — xxh64 of the first 4 KiB for candidates whose
//!    persisted `hash_first` is missing; files already carrying one reuse
//!    it. Survivors regroup by `(size, hash_first)`; groups that shrink
//!    below 2 members drop out.
//! 3. **HashingFull** — streaming xxh64 of the whole file for surviving
//!    members whose persisted `hash_full` is missing. Final groups key on
//!    `(size, hash_full)` with `>= 2` members.
//!
//! Skip rules: a Windows cloud placeholder (checked via
//! [`crate::scanner::is_placeholder_attr`] on `GetFileAttributesW` output,
//! BEFORE the file is opened) and any open/read failure mark the file
//! skipped — it never enters a group and never panics the run. Counters
//! for both land in [`DedupOutcome`].
//!
//! Concurrency contract with the engine: hashing happens entirely OUTSIDE
//! the store lock; the lock is taken only for the initial snapshot and for
//! short batched persists of freshly computed hashes
//! ([`crate::store::Store::update_file_hashes`]), so scans and watcher
//! batches queue for microseconds, not for the length of a hash pass.
//!
//! Incremental: persisted hashes are reused, so a second run over an
//! unchanged index does zero I/O on already-hashed files. v1 trade-off,
//! accepted and documented: a full rescan upserts rows with NULL hashes
//! (the scanner knows nothing of hashing), which wipes what was persisted —
//! the next dedup run rehashes. Between scans (and across engine restarts
//! without a rescan) persistence works, which is what the dedup view's
//! `dedup_start()` speed-up relies on.
//!
//! The persist itself is guarded ([`HashUpdate`]): every update carries the
//! snapshot's mtime + size and lands only while the row still matches both.
//! That matters because the `files` table has no AUTOINCREMENT — a rescan
//! running WHILE a dedup run is hashing purges and re-inserts rows with
//! REUSED ids, and an unguarded batch would stamp the old files' hashes
//! onto different new files, forging dupe groups the incremental path
//! would never re-verify. A guard-mismatched batch is a no-op; the hashes
//! are recomputed by the next run.
//!
//! v1 deliberately does NOT throttle the hashing thread (no priority
//! tweaks, no idle-time scheduling): `std::thread` at normal priority, the
//! OS scheduler plus the staged skip rules keep the cost bounded.
//!
//! Testability seam: all disk access goes through [`FileReader`], so tests
//! can fake placeholder attributes and locked files ([`RealFileReader`] is
//! the production default; the locked-file behavior is additionally
//! exercised against a real exclusively-opened file in the engine tests on
//! Windows).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf, MAIN_SEPARATOR_STR};
use std::sync::Mutex;

use xxhash_rust::xxh64::{xxh64, Xxh64};

use crate::engine::DedupPhase;
use crate::model::FileId;
use crate::store::{HashUpdate, Store};

/// Bytes hashed in the first stage (the design's "first 4KB").
const FIRST_CHUNK: usize = 4096;
/// Streaming read buffer for the full-hash stage.
const READ_BUFFER: usize = 64 * 1024;
/// Progress events fire at least this often within a stage (and at every
/// stage change).
const PROGRESS_EVERY: u64 = 500;
/// Computed hashes are persisted in batches of this size under one short
/// store lock (one transaction per batch).
const PERSIST_BATCH: usize = 128;

/// Point-in-time state of the dedup state machine, as
/// [`crate::engine::Engine::dedup_state`] reports it.
///
/// `done`/`total` mirror the latest [`crate::engine::EngineEvent::DedupProgress`]:
/// they count the records of the CURRENT stage while a stage runs, and at
/// [`DedupPhase::Done`] they equal the number of hash computations THIS run
/// performed (0 = every hash was already persisted, i.e. a fully
/// incremental run). `groups` is the group count of the last COMPLETED run
/// (0 before the first one finishes); `errors` counts files skipped for
/// this run because they could not be read OR were cloud placeholders —
/// both are "marked skipped, not grouped" per the design.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DedupState {
    pub phase: DedupPhase,
    pub done: u64,
    pub total: u64,
    pub groups: u64,
    pub errors: u64,
}

/// One content-identical file group as returned by
/// [`crate::engine::Engine::dedup_groups`]. Members are materialized
/// [`Hit`]s (index-relative paths, like search results); groups are ordered
/// by size descending for stable paging.
///
/// [`Hit`]: crate::query::Hit
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeGroup {
    /// The full-content xxh64 every member shares.
    pub hash: u64,
    /// Snapshot byte size every member shares.
    pub size: u64,
    /// The group's members (>= 2), stable order (files-row id ascending).
    pub members: Vec<crate::query::Hit>,
}

/// Engine-side record of one finished run's group: members as `files` row
/// ids, materialized into [`DupeGroup`]s only when a page is requested (so
/// files deleted between the run and the query simply drop out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredGroup {
    pub hash: u64,
    pub size: u64,
    pub members: Vec<FileId>,
}

/// Aggregate outcome of one pipeline run; the engine folds this into its
/// [`DedupState`] and stores `groups`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DedupOutcome {
    pub groups: Vec<StoredGroup>,
    /// First-4KB hashes computed by this run.
    pub hashed_first: u64,
    /// Full hashes computed by this run.
    pub hashed_full: u64,
    /// Files skipped because they could not be resolved, opened or read.
    pub errors: u64,
    /// Files skipped because Windows reports them as cloud placeholders
    /// (never opened, so hashing never triggers a hydration download).
    pub placeholders: u64,
}

/// The disk-touching side of dedup, injected so tests can fake placeholder
/// attributes and locked files without OneDrive or handle tricks.
pub(crate) trait FileReader {
    /// Raw Windows attribute bits of `path` (`None` when it cannot be
    /// statted). Consulted BEFORE any open, so placeholder files are never
    /// touched.
    fn attributes(&self, path: &Path) -> Option<u32>;
    /// Opens the file for reading.
    fn open(&self, path: &Path) -> std::io::Result<std::fs::File>;
}

/// Production reader: real `GetFileAttributesW` + `std::fs::File::open`.
pub(crate) struct RealFileReader;

impl FileReader for RealFileReader {
    fn attributes(&self, path: &Path) -> Option<u32> {
        crate::scanner::file_attributes(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(path)
    }
}

/// Resolves an index-relative path (root segment first, `MAIN_SEPARATOR`-
/// joined — the spelling [`crate::query::Hit::path`] and the `dirs`-table
/// chain use) to an absolute disk path under one of `roots`, by matching
/// the path's first segment against each root's
/// [`crate::scanner::root_segment`]. `None` when no root matches.
///
/// Roots should be non-overlapping (see `root_segment`); on a shared final
/// segment the first root in `roots` wins. The engine wraps this with its
/// effective roots ([`crate::engine::Engine::resolve_absolute`]) for Task
/// 10 (trash) and Task 15 (reveal).
pub(crate) fn resolve_absolute_path(roots: &[String], index_path: &str) -> Option<PathBuf> {
    let mut parts = index_path.split(MAIN_SEPARATOR_STR);
    let first = parts.next()?;
    let root = roots
        .iter()
        .find(|root| crate::scanner::root_segment(Path::new(root)) == first)?;
    let mut abs = PathBuf::from(root);
    abs.extend(parts);
    Some(abs)
}

/// One snapshot row the pipeline works from.
#[derive(Debug, Clone)]
struct Entry {
    id: FileId,
    parent: FileId,
    name: String,
    size: u64,
    /// Snapshot mtime — identity half of the persist guard: a batch is
    /// written only while the row still carries this mtime AND `size`, so
    /// a rescan's purge-and-reinsert (row ids are reused) cannot inherit
    /// another file's hashes.
    mtime: i64,
    hash_first: Option<u64>,
    hash_full: Option<u64>,
}

/// Store snapshot taken under one short lock: candidate rows plus the dir
/// tree needed to rebuild index paths (hashing then proceeds lock-free).
#[derive(Debug, Default)]
struct Snapshot {
    /// Non-directory rows with `size > 0` (the only dedup candidates).
    files: Vec<Entry>,
    /// `dirs` table as `(id -> (parent, name))`.
    dirs: HashMap<FileId, (Option<FileId>, String)>,
}

impl Snapshot {
    /// The entry's index-relative path (root segment, ..., name,
    /// `MAIN_SEPARATOR`-joined), or `None` when its dir chain is broken.
    /// Built on demand — only files that actually need hashing pay for it.
    fn index_path(&self, entry: &Entry) -> Option<String> {
        let mut segments = vec![entry.name.as_str()];
        let mut budget = self.dirs.len() + 1; // a corrupt parent cycle cannot loop
        let mut current = Some(entry.parent);
        while let Some(id) = current {
            if budget == 0 {
                return None;
            }
            budget -= 1;
            let (parent, name) = self.dirs.get(&id)?;
            segments.push(name.as_str());
            current = *parent;
        }
        segments.reverse();
        Some(segments.join(MAIN_SEPARATOR_STR))
    }
}

/// Loads the [`Snapshot`] under one short store lock. A store error yields
/// an empty snapshot: the run then finishes trivially with no groups and is
/// never fatal (the next run, after the store heals, finds everything).
fn load_snapshot(store: &Mutex<Store>) -> Snapshot {
    let guard = crate::engine::lock_unpoisoned(store);
    let mut snapshot = Snapshot::default();
    if let Ok(dirs) = guard.all_dirs() {
        snapshot.dirs = dirs
            .into_iter()
            .map(|(id, parent, name)| (id, (parent, name)))
            .collect();
    }
    if let Ok(files) = guard.all_files() {
        snapshot.files = files
            .into_iter()
            .filter(|rec| !rec.is_dir && rec.size > 0)
            .map(|rec| Entry {
                id: rec.id,
                parent: rec.parent,
                name: rec.name,
                size: rec.size,
                mtime: rec.mtime,
                hash_first: rec.hash_first,
                hash_full: rec.hash_full,
            })
            .collect();
    }
    snapshot
}

/// Runs the whole staged pipeline against `store` (locked only for the
/// snapshot and short persist batches), resolving files under `roots` and
/// touching disk exclusively through `reader`. `progress` receives
/// `(phase, done, total)` at every stage change and every
/// [`PROGRESS_EVERY`] records within a stage; the engine forwards it as
/// [`crate::engine::EngineEvent::DedupProgress`]. Never panics on bad
/// files; every skip is counted in the returned [`DedupOutcome`].
pub(crate) fn run_pipeline(
    store: &Mutex<Store>,
    roots: &[String],
    reader: &dyn FileReader,
    progress: &dyn Fn(DedupPhase, u64, u64),
) -> DedupOutcome {
    let mut outcome = DedupOutcome::default();
    let snapshot = load_snapshot(store);
    let total = snapshot.files.len() as u64;

    // ---- Stage 1: Grouping — size histogram, sizes carried >= 2 times. ----
    progress(DedupPhase::Grouping, 0, total);
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (idx, entry) in snapshot.files.iter().enumerate() {
        by_size.entry(entry.size).or_default().push(idx);
    }
    progress(DedupPhase::Grouping, total, total);
    let mut candidates: Vec<usize> = by_size
        .into_values()
        .flat_map(|group| if group.len() >= 2 { group } else { Vec::new() })
        .collect();
    candidates.sort_unstable(); // deterministic progress/persist order

    // ---- Stage 2: HashingFirst — first 4 KiB xxh64, persisted hashes reused. ----
    progress(DedupPhase::HashingFirst, 0, candidates.len() as u64);
    let mut first_hashes: Vec<Option<u64>> = vec![None; snapshot.files.len()];
    let mut pending: Vec<HashUpdate> = Vec::new();
    let mut done: u64 = 0;
    for &idx in &candidates {
        let hash = match snapshot.files[idx].hash_first {
            Some(persisted) => persisted,
            None => {
                match hash_first_of(&snapshot, idx, roots, reader, &mut outcome, &mut pending) {
                    Some(hash) => hash,
                    None => {
                        advance_stage_progress(
                            progress,
                            DedupPhase::HashingFirst,
                            &mut done,
                            candidates.len() as u64,
                            &mut pending,
                            store,
                        );
                        continue;
                    }
                }
            }
        };
        first_hashes[idx] = Some(hash);
        advance_stage_progress(
            progress,
            DedupPhase::HashingFirst,
            &mut done,
            candidates.len() as u64,
            &mut pending,
            store,
        );
    }
    flush_persist(store, &mut pending);
    progress(
        DedupPhase::HashingFirst,
        candidates.len() as u64,
        candidates.len() as u64,
    );

    // Regroup by (size, hash_first); groups below 2 members drop out.
    let mut by_first: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for &idx in &candidates {
        if let Some(hash) = first_hashes[idx] {
            by_first
                .entry((snapshot.files[idx].size, hash))
                .or_default()
                .push(idx);
        }
    }
    let survivors: Vec<Vec<usize>> = by_first
        .into_values()
        .filter(|group| group.len() >= 2)
        .collect();

    // ---- Stage 3: HashingFull — whole-file xxh64 for survivors lacking one. ----
    let full_candidates: Vec<usize> = survivors
        .iter()
        .flat_map(|group| group.iter().copied())
        .filter(|idx| snapshot.files[*idx].hash_full.is_none())
        .collect();
    progress(DedupPhase::HashingFull, 0, full_candidates.len() as u64);
    let mut full_hashes: Vec<Option<u64>> = vec![None; snapshot.files.len()];
    let mut done: u64 = 0;
    for &idx in &full_candidates {
        let hash = match snapshot.files[idx].hash_full {
            Some(persisted) => persisted,
            None => match hash_full_of(&snapshot, idx, roots, reader, &mut outcome, &mut pending) {
                Some(hash) => hash,
                None => {
                    advance_stage_progress(
                        progress,
                        DedupPhase::HashingFull,
                        &mut done,
                        full_candidates.len() as u64,
                        &mut pending,
                        store,
                    );
                    continue;
                }
            },
        };
        full_hashes[idx] = Some(hash);
        advance_stage_progress(
            progress,
            DedupPhase::HashingFull,
            &mut done,
            full_candidates.len() as u64,
            &mut pending,
            store,
        );
    }
    flush_persist(store, &mut pending);
    progress(
        DedupPhase::HashingFull,
        full_candidates.len() as u64,
        full_candidates.len() as u64,
    );

    // ---- Final groups: (size, hash_full) with >= 2 members. A survivor
    // whose full stage failed (both values `None`) drops out here. ----
    let mut by_full: HashMap<(u64, u64), Vec<FileId>> = HashMap::new();
    for group in &survivors {
        for &idx in group.iter() {
            let full = full_hashes[idx].or(snapshot.files[idx].hash_full);
            if let Some(full) = full {
                by_full
                    .entry((snapshot.files[idx].size, full))
                    .or_default()
                    .push(snapshot.files[idx].id);
            }
        }
    }
    let mut groups: Vec<StoredGroup> = by_full
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|((size, hash), mut members)| {
            members.sort_unstable();
            StoredGroup {
                hash,
                size,
                members,
            }
        })
        .collect();
    // Size descending for the stable paging contract; hash ascending as the
    // deterministic tie-break.
    groups.sort_unstable_by(|a, b| b.size.cmp(&a.size).then(a.hash.cmp(&b.hash)));
    outcome.groups = groups;

    let hashed = outcome.hashed_first + outcome.hashed_full;
    progress(DedupPhase::Done, hashed, hashed);
    outcome
}

/// Emits at most one progress tick per [`PROGRESS_EVERY`] records and
/// persists computed hashes alongside: whenever a batch of
/// [`PERSIST_BATCH`] has accumulated or a progress tick fires (short lock,
/// one transaction per flush).
fn advance_stage_progress(
    progress: &dyn Fn(DedupPhase, u64, u64),
    phase: DedupPhase,
    done: &mut u64,
    total: u64,
    pending: &mut Vec<HashUpdate>,
    store: &Mutex<Store>,
) {
    *done += 1;
    if pending.len() >= PERSIST_BATCH {
        flush_persist(store, pending);
    }
    if *done % PROGRESS_EVERY == 0 {
        flush_persist(store, pending);
        progress(phase, *done, total);
    }
}

/// Hashes the first [`FIRST_CHUNK`] bytes of snapshot entry `idx`, recording
/// the computed value in `pending` for batched persistence. `None` (with the
/// skip counted) when the path cannot be resolved, the file is a Windows
/// cloud placeholder (never opened), or it cannot be opened/read.
fn hash_first_of(
    snapshot: &Snapshot,
    idx: usize,
    roots: &[String],
    reader: &dyn FileReader,
    outcome: &mut DedupOutcome,
    pending: &mut Vec<HashUpdate>,
) -> Option<u64> {
    let entry = &snapshot.files[idx];
    let path = open_candidate(snapshot, entry, roots, reader, outcome)?;
    let mut file = match reader.open(&path) {
        Ok(file) => file,
        Err(_) => {
            outcome.errors += 1;
            return None;
        }
    };
    let mut chunk = [0u8; FIRST_CHUNK];
    let mut filled = 0;
    while filled < chunk.len() {
        match file.read(&mut chunk[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => {
                outcome.errors += 1;
                return None;
            }
        }
    }
    let hash = xxh64(&chunk[..filled], 0);
    outcome.hashed_first += 1;
    pending.push(HashUpdate {
        id: entry.id,
        mtime: entry.mtime,
        size: entry.size,
        hash_first: Some(hash),
        hash_full: None,
    });
    Some(hash)
}

/// Hashes the whole file at snapshot entry `idx` (streaming), recording the
/// value in `pending` for batched persistence. `None` (with the skip
/// counted) on any resolution/open/read failure.
fn hash_full_of(
    snapshot: &Snapshot,
    idx: usize,
    roots: &[String],
    reader: &dyn FileReader,
    outcome: &mut DedupOutcome,
    pending: &mut Vec<HashUpdate>,
) -> Option<u64> {
    let entry = &snapshot.files[idx];
    let path = open_candidate(snapshot, entry, roots, reader, outcome)?;
    let mut file = match reader.open(&path) {
        Ok(file) => file,
        Err(_) => {
            outcome.errors += 1;
            return None;
        }
    };
    let mut hasher = Xxh64::new(0);
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(_) => {
                outcome.errors += 1;
                return None;
            }
        }
    }
    let hash = hasher.digest();
    outcome.hashed_full += 1;
    pending.push(HashUpdate {
        id: entry.id,
        mtime: entry.mtime,
        size: entry.size,
        hash_first: None,
        hash_full: Some(hash),
    });
    Some(hash)
}

/// The pre-open gate shared by both hash stages: resolve the snapshot
/// entry's index path to an absolute disk path, then check Windows
/// placeholder attributes BEFORE opening. Returns the path, or `None` with
/// the matching skip counter bumped (a path that cannot be resolved is an
/// error — it can never be opened, which is what the counter counts).
fn open_candidate(
    snapshot: &Snapshot,
    entry: &Entry,
    roots: &[String],
    reader: &dyn FileReader,
    outcome: &mut DedupOutcome,
) -> Option<PathBuf> {
    let abs = match snapshot.index_path(entry) {
        Some(index_path) => resolve_absolute_path(roots, &index_path),
        None => None,
    };
    let Some(abs) = abs else {
        outcome.errors += 1;
        return None;
    };
    match reader.attributes(&abs) {
        // Unstatable (vanished/unreadable): an error, not a placeholder.
        None => {
            outcome.errors += 1;
            return None;
        }
        Some(attrs) => {
            if crate::scanner::is_placeholder_attr(attrs) {
                outcome.placeholders += 1;
                return None;
            }
        }
    }
    Some(abs)
}

/// Persists one batch of computed hashes under one short store lock (single
/// transaction). Store errors are swallowed: hashes are a cache — a lost
/// batch is recomputed by the next run, never fatal to this one. Every
/// update carries the snapshot's mtime + size, so a batch that was in
/// flight while a rescan purged and re-inserted rows (REUSED ids, the
/// files table has no AUTOINCREMENT) becomes a no-op instead of stamping
/// the old files' hashes onto the new rows.
fn flush_persist(store: &Mutex<Store>, pending: &mut Vec<HashUpdate>) {
    if pending.is_empty() {
        return;
    }
    let mut guard = crate::engine::lock_unpoisoned(store);
    let _ = guard.update_file_hashes(pending);
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileRecord;
    use std::fs;
    use std::path::MAIN_SEPARATOR;
    use std::sync::Mutex as StdMutex;

    /// Store in its own tempdir: `(tempdir, store Arc)`.
    fn test_store() -> (tempfile::TempDir, StdMutex<Store>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("index.db")).unwrap();
        (tmp, StdMutex::new(store))
    }

    /// Upserts `name` (size 5) under `["fixture"]` with no hashes.
    fn upsert_fixture_row(store: &StdMutex<Store>, name: &str) -> FileId {
        let mut rec = FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size: 5,
            mtime: 1_700_000_000,
            ctime: 1_700_000_001,
            atime: 1_700_000_002,
            ext_id: 0,
            is_dir: false,
            hash_first: None,
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        };
        crate::engine::lock_unpoisoned(store)
            .upsert_file(&["fixture"], &mut rec)
            .unwrap();
        rec.id
    }

    /// Fake reader: reports configured placeholder paths (and stat-missing
    /// paths) through `attributes`, logs every `open` call.
    struct FakeReader {
        placeholders: Vec<PathBuf>,
        missing: Vec<PathBuf>,
        opened: StdMutex<Vec<PathBuf>>,
    }

    impl FakeReader {
        fn new() -> FakeReader {
            FakeReader {
                placeholders: Vec::new(),
                missing: Vec::new(),
                opened: StdMutex::new(Vec::new()),
            }
        }
    }

    impl FileReader for FakeReader {
        fn attributes(&self, path: &Path) -> Option<u32> {
            if self.missing.iter().any(|p| p == path) {
                return None;
            }
            if self.placeholders.iter().any(|p| p == path) {
                return Some(0x1000); // FILE_ATTRIBUTE_OFFLINE
            }
            Some(0)
        }

        fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
            self.opened.lock().unwrap().push(path.to_path_buf());
            std::fs::File::open(path)
        }
    }

    fn noop_progress(_: DedupPhase, _: u64, _: u64) {}

    #[test]
    fn resolve_absolute_path_matches_root_segment() {
        let roots = vec![r"C:\data".to_string(), "/home/me".to_string()];
        assert_eq!(
            resolve_absolute_path(
                &roots,
                &format!("data{MAIN_SEPARATOR}a{MAIN_SEPARATOR}b.txt")
            ),
            Some(PathBuf::from(r"C:\data\a\b.txt"))
        );
        assert_eq!(
            resolve_absolute_path(&roots, &format!("me{MAIN_SEPARATOR}notes.txt")),
            Some(PathBuf::from("/home/me/notes.txt"))
        );
        // Volume-style root: segment "C:", rest appended to the root itself.
        assert_eq!(
            resolve_absolute_path(&roots, &format!("C:{MAIN_SEPARATOR}tmp{MAIN_SEPARATOR}x")),
            None,
            "no root's segment is C: here"
        );
        assert_eq!(resolve_absolute_path(&roots, ""), None);
    }

    #[test]
    fn snapshot_index_path_rebuilds_chain() {
        let (_tmp, store) = test_store();
        let mut nested = FileRecord {
            id: 0,
            parent: 0,
            name: "memo.md".to_string(),
            size: 3,
            mtime: 0,
            ctime: 0,
            atime: 0,
            ext_id: 0,
            is_dir: false,
            hash_first: None,
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        };
        crate::engine::lock_unpoisoned(&store)
            .upsert_file(&["fixture", "docs", "inner"], &mut nested)
            .unwrap();
        let snapshot = load_snapshot(&store);
        assert_eq!(snapshot.files.len(), 1);
        let path = snapshot.index_path(&snapshot.files[0]).unwrap();
        assert_eq!(
            path,
            format!("fixture{MAIN_SEPARATOR}docs{MAIN_SEPARATOR}inner{MAIN_SEPARATOR}memo.md")
        );
    }

    #[test]
    fn dedup_placeholder_never_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("b.txt"), b"alpha").unwrap();

        let (_tstore, store) = test_store();
        upsert_fixture_row(&store, "a.txt");
        upsert_fixture_row(&store, "b.txt");

        let mut reader = FakeReader::new();
        reader.placeholders.push(root.join("a.txt"));
        let roots = vec![root.to_string_lossy().into_owned()];

        let outcome = run_pipeline(&store, &roots, &reader, &noop_progress);

        // The placeholder was never opened (no hydration download); its
        // identical sibling was hashed — and alone it forms no group.
        let opened = reader.opened.lock().unwrap().clone();
        assert_eq!(opened, vec![root.join("b.txt")], "only b.txt is opened");
        assert_eq!(outcome.placeholders, 1);
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.hashed_first, 1);
        assert!(
            outcome.groups.is_empty(),
            "a skipped member cannot confirm a dupe group"
        );
    }

    /// The rescan-overlap hazard: `files` has no AUTOINCREMENT, so a full
    /// rescan running while a dedup run is hashing purges and re-inserts
    /// rows with REUSED ids. A persist batch queued from the pre-rescan
    /// snapshot carries the old row's mtime + size; the store guard must
    /// turn it into a no-op, or the old file's hashes land on the new
    /// file's row and the incremental path would later group it with the
    /// old file's true twin WITHOUT ever reading it.
    #[test]
    fn stale_hash_batch_cannot_pollute_reused_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("old.txt"), b"XX").unwrap();
        fs::write(root.join("mate.txt"), b"XX").unwrap(); // old.txt's true twin

        let (_tstore, store) = test_store();
        let rec = |name: &str, mtime: i64| FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size: 2,
            mtime,
            ctime: mtime,
            atime: mtime,
            ext_id: 0,
            is_dir: false,
            hash_first: None,
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        };
        {
            let mut guard = crate::engine::lock_unpoisoned(&store);
            let mut mate = rec("mate.txt", 1_700_000_000);
            guard.upsert_file(&["fixture"], &mut mate).unwrap();
            let mut old = rec("old.txt", 1_700_000_100);
            guard.upsert_file(&["fixture"], &mut old).unwrap();
        }

        // The run's queued batch: computed against the old row (mtime
        // 1_700_000_100), holding the old content's hash.
        let twin_hash = xxh64(b"XX", 0);
        let stale = HashUpdate {
            id: 0, // fixed up below once the id is known
            mtime: 1_700_000_100,
            size: 2,
            hash_first: Some(twin_hash),
            hash_full: Some(twin_hash),
        };
        let old_id = {
            let guard = crate::engine::lock_unpoisoned(&store);
            guard
                .all_files()
                .unwrap()
                .into_iter()
                .find(|r| r.name == "old.txt")
                .map(|r| r.id)
                .unwrap()
        };
        let stale = HashUpdate {
            id: old_id,
            ..stale
        };

        // The rescan's purge + reinsert: the deleted row was the max id, so
        // the replacement REUSES it — the exact hazard this guards.
        crate::engine::lock_unpoisoned(&store)
            .delete_file(old_id)
            .unwrap();
        fs::remove_file(root.join("old.txt")).unwrap();
        fs::write(root.join("reused.txt"), b"YY").unwrap();
        let mut reused = rec("reused.txt", 1_700_000_999);
        crate::engine::lock_unpoisoned(&store)
            .upsert_file(&["fixture"], &mut reused)
            .unwrap();
        assert_eq!(
            reused.id, old_id,
            "rowid reuse is what makes the stale batch dangerous"
        );

        let hashes_of = |id: FileId| -> (Option<u64>, Option<u64>) {
            crate::engine::lock_unpoisoned(&store)
                .all_files()
                .unwrap()
                .into_iter()
                .find(|r| r.id == id)
                .map(|r| (r.hash_first, r.hash_full))
                .unwrap()
        };

        // Flushing the stale batch must NOT touch the replaced row...
        assert_eq!(
            crate::engine::lock_unpoisoned(&store)
                .update_file_hashes(&[stale])
                .unwrap(),
            0,
            "the snapshot guard must reject the replaced row"
        );
        assert_eq!(
            hashes_of(old_id),
            (None, None),
            "no stale hash may land on the reused row"
        );

        // ...and the next run must find NO pair between the two
        // different-content files: it re-reads both instead of trusting
        // anything stale.
        let reader = RealFileReader;
        let roots = vec![root.to_string_lossy().into_owned()];
        let outcome = run_pipeline(&store, &roots, &reader, &noop_progress);
        assert!(
            outcome.groups.is_empty(),
            "XX and YY differ, so no group may form (stale or fresh)"
        );

        // Positive control: a batch carrying the CURRENT row's identity
        // still lands — the guard is not over-broad.
        assert_eq!(
            crate::engine::lock_unpoisoned(&store)
                .update_file_hashes(&[HashUpdate {
                    id: reused.id,
                    mtime: 1_700_000_999,
                    size: 2,
                    hash_first: Some(xxh64(b"YY", 0)),
                    hash_full: None,
                }])
                .unwrap(),
            1
        );
        assert_eq!(hashes_of(reused.id), (Some(xxh64(b"YY", 0)), None));
    }

    #[test]
    fn dedup_counts_unresolvable_paths_as_errors() {
        // Rows indexed under a root segment no configured root carries:
        // neither can be resolved to disk, both are skipped as errors and
        // nothing is ever opened.
        let tmp = tempfile::tempdir().unwrap();
        let (_tstore, store) = test_store();
        upsert_fixture_row(&store, "a.txt");
        upsert_fixture_row(&store, "b.txt");

        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let roots = vec![elsewhere.to_string_lossy().into_owned()];
        let reader = FakeReader::new();

        let outcome = run_pipeline(&store, &roots, &reader, &noop_progress);

        assert_eq!(outcome.errors, 2);
        assert_eq!(outcome.placeholders, 0);
        assert!(reader.opened.lock().unwrap().is_empty());
        assert!(outcome.groups.is_empty());
    }

    #[test]
    fn dedup_distinguishes_same_size_different_content() {
        // Same size, first bytes differ: the first stage alone dissolves
        // the pair and the full stage never reads either file.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("b.txt"), b"alphb").unwrap();

        let (_tstore, store) = test_store();
        upsert_fixture_row(&store, "a.txt");
        upsert_fixture_row(&store, "b.txt");

        let reader = FakeReader::new();
        let roots = vec![root.to_string_lossy().into_owned()];
        let outcome = run_pipeline(&store, &roots, &reader, &noop_progress);

        assert_eq!(outcome.hashed_first, 2);
        assert_eq!(outcome.hashed_full, 0, "full hashes never needed");
        assert_eq!(reader.opened.lock().unwrap().len(), 2);
        assert!(outcome.groups.is_empty());
    }
}
