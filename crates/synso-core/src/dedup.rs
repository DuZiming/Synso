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
//! BEFORE the file is opened), a Windows sharing violation (another handle
//! owns the file — a [`OpenSkip::Locked`] skip, nothing read), and any
//! other open/read failure mark the file skipped — it never enters a group
//! and never panics the run. Counters for all three land in
//! [`DedupOutcome`].
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
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf, MAIN_SEPARATOR_STR};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_READ};

use xxhash_rust::xxh64::{xxh64, Xxh64};

use crate::engine::DedupPhase;
use crate::model::FileId;
use crate::store::{HashUpdate, Store};
use crate::{Result, SynsoError};

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
/// this run because they could not be read, were locked by another
/// handle, OR were cloud placeholders — all are "marked skipped, not
/// grouped" per the design.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
/// [`DedupState`] and stores `groups`. Two writers share the struct:
/// `run_pipeline` (the old path, until Task 7 deletes it) fills the
/// legacy scalars, `run_gates` (the rewritten pipeline) fills `counters`
/// and mirrors the scalars from it so engine.rs's single fold keeps
/// working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DedupOutcome {
    pub groups: Vec<StoredGroup>,
    /// First-4KB hashes computed by this run.
    pub hashed_first: u64,
    /// Full hashes computed by this run.
    pub hashed_full: u64,
    /// Files skipped because they could not be resolved, opened or read.
    /// Legacy mirror: `run_pipeline` writes it directly, `run_gates`
    /// copies it from [`DedupOutcome::counters`].
    pub errors: u64,
    /// Files skipped because another handle denied our open (a Windows
    /// sharing violation, [`OpenSkip::Locked`]): nothing was read, and
    /// the next run simply retries. Same legacy-mirror story as
    /// `errors` — the counters own the real number.
    pub locked: u64,
    /// Files skipped because Windows reports them as cloud placeholders
    /// (never opened, so hashing never triggers a hydration download).
    /// Same legacy-mirror story as `errors`.
    pub placeholders: u64,
    /// Every counter the four gates produced (Task 6; `run_pipeline`
    /// leaves it zeroed). The skip reasons live here separated
    /// (`locked`/`fresh`/`placeholders`/`recycle`) and each gate's kill
    /// tally (`size_gate`/`head_gate`/`sample_gate`) rides along.
    pub counters: OutcomeCounters,
    /// True when `cancel` fired and the run stopped at a checkpoint
    /// (Task 6): every hash computed before the hit is persisted, but
    /// the group list comes back EMPTY — the engine keeps serving the
    /// last completed run's groups. `run_pipeline` never sets it.
    pub cancelled: bool,
}

/// The disk-touching side of dedup, injected so tests can fake placeholder
/// attributes and locked files without OneDrive or handle tricks. `Sync`
/// because gate② shares one reader across its worker threads.
pub(crate) trait FileReader: Sync {
    /// Raw Windows attribute bits of `path` (`None` when it cannot be
    /// statted). Consulted BEFORE any open, so placeholder files are never
    /// touched.
    fn attributes(&self, path: &Path) -> Option<u32>;
    /// Opens the file for reading.
    fn open(&self, path: &Path) -> std::io::Result<std::fs::File>;
    /// Opens for gate④'s WHOLE-FILE read: the same deny-write semantics
    /// as [`FileReader::open`] plus — on Windows — the
    /// `FILE_FLAG_SEQUENTIAL_SCAN` read-ahead hint. Gate④ is the only
    /// stage that streams a file front to back in fixed steps, so the
    /// prefetcher is pure win there; gate②'s 4KiB probe and gate③'s
    /// seek-hopping window reads are NOT sequential and keep plain
    /// `open`. The default just calls [`FileReader::open`] — test fakes
    /// inherit it and never need the flag.
    #[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
    fn open_sequential(&self, path: &Path) -> std::io::Result<std::fs::File> {
        self.open(path)
    }
}

/// Production reader: real `GetFileAttributesW` + a read-only open that
/// denies write-sharing on Windows (see the `open` impl for why).
pub(crate) struct RealFileReader;

impl FileReader for RealFileReader {
    fn attributes(&self, path: &Path) -> Option<u32> {
        crate::scanner::file_attributes(path)
    }

    fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        // Deny write-sharing — `FILE_SHARE_READ` is the ONLY access we
        // grant other openers: while this handle is open no writer can
        // open the file, so the bytes we hash are stable. std's default
        // open shares write access and could hash mid-write drift (a
        // group keyed on content the file no longer has) — this setting
        // is that bug's fix. Symmetrically, a file a writer (or an
        // exclusive holder) already owns fails HERE with
        // ERROR_SHARING_VIOLATION; `classify_open_err` files it under
        // [`OpenSkip::Locked`].
        opts.share_mode(FILE_SHARE_READ.0);
        // No share modes off Windows (mac port): the plain read open
        // above — locked detection degenerates to "open failed", with
        // the mtime grace window ([`is_fresh`]) as the backstop.
        opts.open(path)
    }

    /// Gate④'s variant (see [`FileReader::open_sequential`]): the same
    /// deny-write share mode as `open` — the stability argument lives
    /// there — plus the sequential-scan hint, so the OS prefetcher
    /// reads ahead while gate④ streams its 64KB steps.
    #[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
    fn open_sequential(&self, path: &Path) -> std::io::Result<std::fs::File> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            opts.share_mode(FILE_SHARE_READ.0);
            opts.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN.0);
        }
        // No share modes off Windows (mac port): see `open`.
        opts.open(path)
    }
}

// ---------------------------------------------------------------------
// Open semantics (Task 3): how a failed open is classified and when a
// candidate is too young to read. The gate code (Task 4-6) consumes both.

/// Why an open attempt was skipped: [`OpenSkip::Locked`] means another
/// handle denied our access — the file exists, we just cannot read it
/// right now, and the next run retries; [`OpenSkip::Error`] is every
/// other failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenSkip {
    /// Another handle denies our access (a Windows sharing violation).
    #[cfg_attr(not(windows), allow(dead_code))] // constructed on Windows only
    Locked,
    /// Any other open failure.
    Error,
}

/// Classifies an open failure into its skip kind: only
/// `raw_os_error() == Some(32)` (`ERROR_SHARING_VIOLATION`) is
/// [`OpenSkip::Locked`], everything else — access denied, vanished file,
/// a non-OS error — stays [`OpenSkip::Error`]. Off Windows no failure can
/// mean "locked by another handle", so every failure is an error there.
pub(crate) fn classify_open_err(err: &std::io::Error) -> OpenSkip {
    match err.raw_os_error() {
        #[cfg(windows)]
        // ERROR_SHARING_VIOLATION: an open handle's access or share mode
        // conflicts with ours.
        Some(32) => OpenSkip::Locked,
        _ => OpenSkip::Error,
    }
}

/// mtime grace window in seconds: a candidate whose snapshot mtime is
/// younger than this may still be mid-write (a half-finished download, a
/// growing log) and must not be read this run. The grace applies only to
/// candidates about to be OPENED — reusing a persisted hash is never
/// gated on freshness; that policy lives in the gate code (Task 4).
#[allow(dead_code)] // Task 4 wiring: gate_head checks the grace before it opens.
pub(crate) const FRESH_SECS: i64 = 300;

/// True while `mtime` (snapshot seconds) is inside the grace window that
/// ends at `now` — `now` is injected by the caller, so tests control the
/// clock.
#[allow(dead_code)] // Task 4 wiring: gate_head checks the grace before it opens.
pub(crate) fn is_fresh(mtime: i64, now: i64) -> bool {
    now - mtime < FRESH_SECS
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
/// cloud placeholder (never opened), or it cannot be opened/read (a
/// sharing violation is a [`OpenSkip::Locked`] skip, not an error).
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
    let mut file = open_or_skip(reader, &path, outcome)?;
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
    let mut file = open_or_skip(reader, &path, outcome)?;
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

/// The open step both hash stages share: opens `path` via `reader`,
/// counting a failure as the matching [`DedupOutcome`] skip — a Windows
/// sharing violation is a [`OpenSkip::Locked`] skip, every other failure
/// keeps the error semantics. `None` means "skipped": no file came back,
/// so the caller has nothing to read.
fn open_or_skip(
    reader: &dyn FileReader,
    path: &Path,
    outcome: &mut DedupOutcome,
) -> Option<std::fs::File> {
    match reader.open(path) {
        Ok(file) => Some(file),
        Err(err) => {
            match classify_open_err(&err) {
                OpenSkip::Locked => outcome.locked += 1,
                OpenSkip::Error => outcome.errors += 1,
            }
            None
        }
    }
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

// ---------------------------------------------------------------------
// Candidate streaming load (pipeline rewrite, Task 2): the SoA/arena
// picture the rewritten pipeline works from. The old `Snapshot` /
// `load_snapshot` pair above stays in place until the engine swap
// deletes it.

/// Sentinel a candidate's hash column carries when the store row has no
/// persisted hash yet (the streamed row's `Option` flattened).
pub(crate) const NO_HASH: u64 = u64::MAX;

/// Directory name whose subtree the dedup run excludes (the recycle
/// bin): the dir itself carries the rule by name, deeper levels inherit
/// it through [`Dirs::recycle`].
const RECYCLE_BIN_DIR: &str = "$Recycle.Bin";

/// A candidate's parent slot when its dir row is missing from `dirs` (a
/// files row without its dir row — only a hand-edited database produces
/// one): `index_path` treats it as a broken chain.
const UNRESOLVED_PARENT: u32 = u32::MAX;

/// Every dedup candidate — non-directory `files` rows with `size > 0` —
/// as parallel SoA columns, streamed straight from
/// [`Store::iter_dedup_candidates`] one push per row: zero per-file heap
/// allocations (the old `all_files` + `Vec<Entry>` load materialized
/// every name twice). Slot `i` addresses all columns at once;
/// `parents` hold `Dirs` SLOTS, not row ids, so the later path/group
/// walks chase plain indices.
#[derive(Debug, Default)]
#[allow(dead_code)] // Task 4-6 wiring: the pipeline rewrite consumes it.
pub(crate) struct Candidates {
    /// `files` row ids, u32-pressed (checked — past `u32::MAX` the load
    /// reports an explicit error, never truncates).
    pub(crate) ids: Vec<u32>,
    /// Containing directory's slot in [`Dirs`]; [`UNRESOLVED_PARENT`]
    /// when the dir row is missing.
    pub(crate) parents: Vec<u32>,
    pub(crate) sizes: Vec<u64>,
    /// Snapshot mtime — identity half of the hash-persist guard.
    pub(crate) mtimes: Vec<i64>,
    /// First-4KiB hash; [`NO_HASH`] when not persisted.
    pub(crate) hash_first: Vec<u64>,
    /// Whole-file hash; [`NO_HASH`] when not persisted.
    pub(crate) hash_full: Vec<u64>,
    /// Start offset of each row's name in [`Self::names`].
    pub(crate) name_off: Vec<u32>,
    /// All names, one byte arena.
    pub(crate) names: Vec<u8>,
}

impl Candidates {
    /// Appends one streamed row: hash `Option`s flatten to [`NO_HASH`],
    /// the name appends to the arena.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        id: u32,
        parent_slot: u32,
        size: u64,
        mtime: i64,
        hash_first: Option<u64>,
        hash_full: Option<u64>,
        name: &str,
    ) {
        self.ids.push(id);
        self.parents.push(parent_slot);
        self.sizes.push(size);
        self.mtimes.push(mtime);
        self.hash_first.push(hash_first.unwrap_or(NO_HASH));
        self.hash_full.push(hash_full.unwrap_or(NO_HASH));
        self.name_off.push(self.names.len() as u32);
        self.names.extend_from_slice(name.as_bytes());
    }

    /// Candidate `i`'s file name out of the arena.
    // Task 4-6 wiring: the group -> Hit materialization reads names.
    #[allow(dead_code)]
    pub(crate) fn name(&self, i: usize) -> &str {
        arena_name(&self.names, &self.name_off, i)
    }

    /// The candidate's index-relative path (root segment, ..., name,
    /// `MAIN_SEPARATOR`-joined), or `None` when its dir chain is broken.
    /// Same on-demand shape and cycle budget as the old
    /// `Snapshot::index_path` — only candidates that actually need
    /// hashing pay for it.
    // Task 4-6 wiring: the hash stages resolve paths through this.
    #[allow(dead_code)]
    pub(crate) fn index_path(&self, i: usize, dirs: &Dirs) -> Option<String> {
        let mut segments = vec![self.name(i)];
        let mut budget = dirs.parents.len() + 1; // a corrupt parent cycle cannot loop
        let mut current = self.parents[i];
        loop {
            if current == UNRESOLVED_PARENT {
                return None;
            }
            let slot = current as usize;
            if budget == 0 {
                return None;
            }
            budget -= 1;
            let Some(parent) = dirs.parents.get(slot) else {
                return None; // broken chain: slot out of range
            };
            segments.push(dirs.name(slot));
            match parent {
                Some(next) => current = *next,
                None => break, // root reached
            }
        }
        segments.reverse();
        Some(segments.join(MAIN_SEPARATOR_STR))
    }
}

/// The `dirs` table as parallel SoA columns (the path-rebuilding half of
/// the snapshot, arena-ified like [`Candidates`]). Rows stream in
/// id-ascending (`all_dirs`), so slot order mirrors row-id order and a
/// parent row always precedes its children (`resolve_dir` creates a
/// dir's parent levels first).
#[derive(Debug, Default)]
#[allow(dead_code)] // Task 4-6 wiring: the pipeline rewrite consumes it.
pub(crate) struct Dirs {
    /// Parent slot per row (`None` = root). `parents.len()` is the slot
    /// bound every other column shares.
    pub(crate) parents: Vec<Option<u32>>,
    /// Start offset of each row's name in [`Self::names`].
    pub(crate) name_off: Vec<u32>,
    /// All names, one byte arena.
    pub(crate) names: Vec<u8>,
    /// Recycle-bin bit: the dir is named [`RECYCLE_BIN_DIR`] or its
    /// parent slot's bit is set, propagated in one id-ascending pass.
    /// The rewritten pipeline excludes such subtrees outright.
    pub(crate) recycle: Vec<bool>,
    /// `dirs` row id -> slot; the recycle propagation and the candidate
    /// load resolve row ids through this.
    pub(crate) by_id: HashMap<FileId, u32>,
}

impl Dirs {
    /// Appends one streamed dir row and its `by_id` entry.
    fn push(&mut self, id: FileId, parent_slot: Option<u32>, name: &str, recycle: bool) {
        self.by_id.insert(id, self.parents.len() as u32);
        self.parents.push(parent_slot);
        self.name_off.push(self.names.len() as u32);
        self.names.extend_from_slice(name.as_bytes());
        self.recycle.push(recycle);
    }

    /// Dir `slot`'s name out of the arena.
    // Task 4-6 wiring: the group -> Hit materialization reads names.
    #[allow(dead_code)]
    pub(crate) fn name(&self, slot: usize) -> &str {
        arena_name(&self.names, &self.name_off, slot)
    }
}

/// Reads row `i`'s name from an offset-table arena (the end offset is
/// the next row's, or the arena end for the last row). The slices are
/// whole names of originally-valid UTF-8 `String`s, so validation cannot
/// fail.
fn arena_name<'a>(names: &'a [u8], name_off: &'a [u32], i: usize) -> &'a str {
    let start = name_off[i] as usize;
    let end = name_off
        .get(i + 1)
        .map_or(names.len(), |&off| off as usize);
    std::str::from_utf8(&names[start..end]).expect("name arena holds valid UTF-8")
}

/// Loads [`Candidates`] + [`Dirs`] under one short store lock (hashing
/// then proceeds lock-free, like the old snapshot). Any store error —
/// including a candidate id past `u32::MAX` — yields `None`: the run
/// then finishes trivially with no groups and is never fatal (the next
/// run, after the store heals, finds everything). That mirrors the old
/// `load_snapshot`'s error semantics.
// Task 4-6 wiring: the rewritten pipeline loads through this.
#[allow(dead_code)]
pub(crate) fn load_candidates(store: &Mutex<Store>) -> Option<(Candidates, Dirs)> {
    load_candidates_checked(store).ok()
}

/// The checked-conversion core behind [`load_candidates`] — explicit
/// errors, so the test suite (and any future caller that wants them) can
/// tell a corrupt id apart from an empty store.
fn load_candidates_checked(store: &Mutex<Store>) -> Result<(Candidates, Dirs)> {
    let guard = crate::engine::lock_unpoisoned(store);

    // Dirs first: candidates resolve their parent slots against it, and
    // the recycle bits propagate in this same id-ascending pass.
    let mut dirs = Dirs::default();
    for (id, parent, name) in guard.all_dirs()? {
        // Checked press: dir slots are u32 too.
        u32::try_from(id).map_err(|_| bad_u32_id("dir", id))?;
        let parent_slot = match parent {
            None => None,
            Some(parent_id) => {
                // resolve_dir creates a dir's parent levels before the dir
                // itself, so ids ascend parent-first; a violation means a
                // hand-edited database. Debug builds stop here; a release
                // build treats the orphan as a root (its subtree's recycle
                // bits then rest on the name rule alone — corruption
                // already means the index is suspect).
                debug_assert!(
                    parent_id < id,
                    "dirs must load parent-first (resolve_dir build invariant): \
                     parent {parent_id} >= dir {id}"
                );
                dirs.by_id.get(&parent_id).copied()
            }
        };
        // Case-INSENSITIVE compare (Ruling 7): Windows volumes may create
        // the dir as `$RECYCLE.BIN` in any casing; `$` stays literal.
        let recycle = name.eq_ignore_ascii_case(RECYCLE_BIN_DIR)
            || parent_slot.is_some_and(|s| dirs.recycle[s as usize]);
        dirs.push(id, parent_slot, &name, recycle);
    }

    let mut cands = Candidates::default();
    for row in guard.iter_dedup_candidates()? {
        let row = row?;
        let id = u32::try_from(row.id).map_err(|_| bad_u32_id("candidate", row.id))?;
        let parent_slot = dirs
            .by_id
            .get(&row.parent)
            .copied()
            .unwrap_or(UNRESOLVED_PARENT);
        cands.push(
            id,
            parent_slot,
            row.size,
            row.mtime,
            row.hash_first,
            row.hash_full,
            &row.name,
        );
    }
    Ok((cands, dirs))
}

/// The explicit error a `files`/`dirs` id that exceeds `u32::MAX`
/// produces (the SoA world presses ids into `u32`; it never truncates).
fn bad_u32_id(kind: &str, id: FileId) -> SynsoError {
    SynsoError::BadQuery(format!(
        "{kind} id {id} exceeds u32::MAX — the SoA world cannot address it"
    ))
}

// ---------------------------------------------------------------------
// Gates ① and ② (pipeline rewrite, Task 4). The rewritten pipeline is a
// chain of gates over the streamed [`Candidates`] snapshot; Task 6's
// `run_gates` will assemble them and swap the engine off `run_pipeline`.
// This section documents the contracts the later gates consume.

/// One gate's removal tally: `files` candidates left the grouping
/// population at this gate and `bytes` is their snapshot-size total.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GateKills {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

/// Every counter the gates produce. `size_gate`/`head_gate`/`sample_gate`
/// count what each gate REMOVED (files + bytes); the rest are gate②'s
/// skip-reason matrix, kept separate for the engine's outcome folding
/// and diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OutcomeCounters {
    /// gate① kills: singleton sizes (no candidate can ever pair).
    pub(crate) size_gate: GateKills,
    /// gate② kills: every candidate that left without a `head` hash —
    /// `files` equals `recycle + fresh + placeholders + locked + errors`.
    pub(crate) head_gate: GateKills,
    /// gate③'s tally (Task 5 fills it; zero until then).
    pub(crate) sample_gate: GateKills,
    /// Open/read/resolution failures ([`OpenSkip::Error`] kind).
    pub(crate) errors: u64,
    /// Sharing-violation skips ([`OpenSkip::Locked`]): nothing was read,
    /// the next run retries.
    pub(crate) locked: u64,
    /// mtime-grace skips ([`is_fresh`]): the file may still be mid-write.
    pub(crate) fresh: u64,
    /// Cloud-placeholder skips: never opened, so hashing never triggers
    /// a hydration download.
    pub(crate) placeholders: u64,
    /// Recycle-bin subtree exclusions — checked for EVERY candidate up
    /// front, persisted-hash ones included.
    pub(crate) recycle: u64,
}

impl OutcomeCounters {
    /// Folds `other` into `self`: every field is an additive run total,
    /// and `run_gates` sums the four gates' tallies into the run
    /// outcome.
    #[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
    pub(crate) fn accumulate(&mut self, other: &OutcomeCounters) {
        self.size_gate.files += other.size_gate.files;
        self.size_gate.bytes += other.size_gate.bytes;
        self.head_gate.files += other.head_gate.files;
        self.head_gate.bytes += other.head_gate.bytes;
        self.sample_gate.files += other.sample_gate.files;
        self.sample_gate.bytes += other.sample_gate.bytes;
        self.errors += other.errors;
        self.locked += other.locked;
        self.fresh += other.fresh;
        self.placeholders += other.placeholders;
        self.recycle += other.recycle;
    }
}

/// gate②'s result. THE PARALLEL CONTRACT (Ruling 1): `head`, `full` and
/// `cand` are one triplet — `head[k]`/`full[k]` belong to candidate slot
/// `cand[k]` in gate②'s DIRECTORY ORDER (sorted once by (parent dir
/// slot, files-row id) at the gate's entry). The input slice is
/// immutable, so the sorted arrangement travels back in `cand` — later
/// gates MUST consume `head`/`full` only through it (`cand.len()` equals
/// the input length).
///
/// * `head[k]` — first-4KiB xxh64 known after gate②, [`NO_HASH`] when
///   not obtained (skipped candidate). Persisted values pass through
///   verbatim; freshly computed ones also land in `pending`.
/// * `full[k]` — whole-file hash known after gate②, [`NO_HASH`] when
///   unknown. Set for TERMINAL candidates: size <= 4KiB rows (the 4KiB
///   probe read IS the whole file — `full == head`, persisted as a
///   double-`Some` [`HashUpdate`], no matter whether the hash was
///   computed now or reused) and candidates carrying a persisted
///   `hash_full`.
/// * `pending` — computed [`HashUpdate`]s not yet persisted. Chunk
///   boundaries flush whenever a [`PERSIST_BATCH`] has accumulated
///   (short lock, hashing never holds it); the residue travels back so
///   the caller persists once after the later gates. A CANCELLED gate
///   has already flushed everything before returning.
/// * `cancelled` — true when `cancel` fired and gate② stopped between
///   files: unprocessed candidates keep [`NO_HASH`] and the last
///   progress call reports `done < total`.
// No Debug derive: `pending` holds `HashUpdate`, which carries none.
#[derive(Default)]
#[allow(dead_code)] // Task 4-6 wiring: run_gates consumes it.
pub(crate) struct HeadOutcome {
    pub(crate) head: Vec<u64>,
    pub(crate) full: Vec<u64>,
    pub(crate) counters: OutcomeCounters,
    /// First-4KiB hashes computed by this gate (reuses not counted).
    pub(crate) hashed_first: u64,
    pub(crate) pending: Vec<HashUpdate>,
    pub(crate) cancelled: bool,
    pub(crate) cand: Vec<u32>,
}

/// Candidates per chunk: gate② streams in directory-order batches of
/// this size. A chunk boundary is a persist flush + progress tick +
/// cancel checkpoint; 64 Ki candidates keep the per-worker batches small
/// while amortizing the boundary costs.
#[allow(dead_code)] // Task 4-6 wiring: gate_head reads it.
const CHUNK: usize = 65_536;

/// Why a candidate left gate② without a hash — the tally helper's tag
/// (the reason counters themselves live on [`OutcomeCounters`]).
#[derive(Debug, Clone, Copy)]
enum Skip {
    Error,
    Locked,
    Fresh,
    Placeholder,
    Recycle,
}

/// One worker's PRIVATE chunk state: results indexed by chunk position
/// (disjoint across workers), computed hashes queued for persistence,
/// counters. Nothing is shared while a chunk runs — the store lock is
/// taken only at the boundary flush.
#[derive(Default)]
#[allow(dead_code)] // Task 4-6 wiring: gate_head consumes it.
struct GateWorker {
    /// `(chunk position, hash)` for candidates that got a first hash
    /// (computed now or reused).
    head_hits: Vec<(usize, u64)>,
    /// `(chunk position, hash)` for TERMINAL candidates.
    full_hits: Vec<(usize, u64)>,
    pending: Vec<HashUpdate>,
    hashed_first: u64,
    kills_files: u64,
    kills_bytes: u64,
    errors: u64,
    locked: u64,
    fresh: u64,
    placeholders: u64,
    recycle: u64,
    done: u64,
}

impl GateWorker {
    /// Records one skipped candidate: the reason counter, the gate-kill
    /// tally (its size leaves the grouping population) and `done`.
    fn skipped(&mut self, skip: Skip, size: u64) {
        match skip {
            Skip::Error => self.errors += 1,
            Skip::Locked => self.locked += 1,
            Skip::Fresh => self.fresh += 1,
            Skip::Placeholder => self.placeholders += 1,
            Skip::Recycle => self.recycle += 1,
        }
        self.kills_files += 1;
        self.kills_bytes += size;
        self.done += 1;
    }
}

/// gate① — the size gate. Returns the candidate slots whose snapshot
/// size is carried by >= 2 candidates (the only ones that can ever pair),
/// in size order; singletons are counted into
/// [`OutcomeCounters::size_gate`] with their byte tally and dropped.
#[allow(dead_code)] // Task 4-6 wiring: run_gates calls it.
pub(crate) fn gate_size(c: &Candidates, counters: &mut OutcomeCounters) -> Vec<u32> {
    let mut order: Vec<u32> = (0..c.sizes.len() as u32).collect();
    order.sort_unstable_by_key(|&slot| c.sizes[slot as usize]);
    let mut kept = Vec::new();
    let mut run = 0usize;
    for w in 1..=order.len() {
        if w < order.len() && c.sizes[order[w] as usize] == c.sizes[order[run] as usize] {
            continue;
        }
        if w - run >= 2 {
            kept.extend_from_slice(&order[run..w]);
        } else {
            counters.size_gate.files += 1;
            counters.size_gate.bytes += c.sizes[order[run] as usize];
        }
        run = w;
    }
    kept
}

/// gate② — the first-4KiB probe. Walks `cand` (candidate slots, the
/// output of [`gate_size`]) in DIRECTORY ORDER in [`CHUNK`]-sized
/// chunks, obtaining a first-4KiB hash per candidate per the skip
/// matrix on [`gate_head_step`], and returns the parallel triplet
/// (`head`/`full`/`cand`, Ruling 1 — see [`HeadOutcome`]).
///
/// * Reuse: a persisted `hash_first` ([`NO_HASH`] = absent) is reused
///   without opening the file and without the mtime grace window —
///   already-hashed files do zero I/O, fresh mtimes notwithstanding.
/// * Workers: one atomic cursor hands out chunk positions; every worker
///   hashes with PRIVATE state only and the store lock is taken at
///   chunk boundaries (and on cancel) to flush accumulated
///   [`PERSIST_BATCH`] batches via `flush_persist`. `workers <= 1` runs
///   inline, single-threaded (the tests' default: deterministic open
///   and pending order).
/// * Progress: `(done, total)` where total is EVERY candidate handed in
///   — skips and reuses advance `done` too, matching the old pipeline's
///   UX. Ticks at least every [`PROGRESS_EVERY`] processed candidates
///   and once more at the end (or at the cancel point).
/// * Cancel: checked at file boundaries (between opens); on cancel the
///   gate stops promptly, flushes the in-flight batch and reports
///   [`HeadOutcome::cancelled`]. Whether the run continues or aborts is
///   the caller's (`run_gates`') decision.
#[allow(dead_code)] // Task 4-6 wiring: run_gates calls it.
#[allow(clippy::too_many_arguments)] // the brief's pinned signature.
pub(crate) fn gate_head(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
) -> HeadOutcome {
    gate_head_chunked(
        c, dirs, cand, roots, reader, workers, cancel, now, progress, store, CHUNK,
    )
}

/// [`gate_head`]'s body with the chunk size injected — the test seam that
/// lets the suite cross chunk boundaries with small fixtures (production
/// always goes through [`gate_head`], i.e. [`CHUNK`]).
#[allow(clippy::too_many_arguments)]
fn gate_head_chunked(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
    chunk_size: usize,
) -> HeadOutcome {
    let mut order = cand.to_vec();
    // Directory order, ONCE at the gate's entry: (parent dir slot, row
    // id) is a total order (ids are unique). `head`/`full` stay parallel
    // to THIS order and it travels back in `HeadOutcome::cand`.
    order.sort_unstable_by_key(|&slot| (c.parents[slot as usize], c.ids[slot as usize]));

    let total = order.len() as u64;
    let mut out = HeadOutcome {
        head: vec![NO_HASH; order.len()],
        full: vec![NO_HASH; order.len()],
        counters: OutcomeCounters::default(),
        hashed_first: 0,
        pending: Vec::new(),
        cancelled: false,
        cand: Vec::new(),
    };
    let mut done: u64 = 0;
    let mut tick: u64 = 0;

    let mut pos = 0usize;
    while pos < order.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end = (pos + chunk_size).min(order.len());
        let chunk = &order[pos..end];
        let next = AtomicUsize::new(0);
        // A `move` closure captures this reference by value (the
        // cursor itself must stay shared, not moved into one worker).
        let cursor: &AtomicUsize = &next;

        let (tx, rx) = mpsc::channel::<GateWorker>();
        if workers <= 1 {
            let mut worker = GateWorker::default();
            gate_head_chunk(&mut worker, chunk, c, dirs, roots, reader, cancel, now, &next);
            let _ = tx.send(worker);
        } else {
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let mut worker = GateWorker::default();
                        gate_head_chunk(
                            &mut worker, chunk, c, dirs, roots, reader, cancel, now, cursor,
                        );
                        let _ = tx.send(worker);
                    });
                }
            });
        }
        drop(tx); // scope has joined; the channel drains and closes
        while let Ok(worker) = rx.recv() {
            // `pos` is this chunk's base in the global directory order —
            // the workers deal chunk-local positions, the parallel
            // arrays are global (Ruling 1).
            merge_worker(&mut out, &mut done, pos, worker);
        }

        // Chunk boundary: batched persist (one short lock per flush)
        // and progress ticks.
        if out.pending.len() >= PERSIST_BATCH {
            flush_persist(store, &mut out.pending);
        }
        let crossed = done / PROGRESS_EVERY;
        if crossed > tick {
            flush_persist(store, &mut out.pending);
            progress(done, total);
            tick = crossed;
        }
        pos = end;
    }

    // Cancel may have fired mid-chunk or between chunks — workers stop
    // at file boundaries either way: flag it and flush the in-flight
    // batch so nothing dangles. A clean end leaves the last (sub-batch)
    // residue in `pending` for the caller to persist after the later
    // gates.
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        flush_persist(store, &mut out.pending);
    }
    progress(done, total);
    out.cand = order;
    out
}

/// The worker loop over one chunk: an atomic cursor hands out chunk
/// positions (each candidate is processed exactly once); cancel is
/// checked at every file boundary, so a cancelled run stops promptly.
#[allow(clippy::too_many_arguments)] // the gate's context, passed through.
fn gate_head_chunk(
    worker: &mut GateWorker,
    chunk: &[u32],
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    cancel: &AtomicBool,
    now: i64,
    next: &AtomicUsize,
) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let k = next.fetch_add(1, Ordering::Relaxed);
        if k >= chunk.len() {
            return;
        }
        gate_head_step(worker, k, chunk[k] as usize, c, dirs, roots, reader, now);
    }
}

/// The per-candidate skip matrix, in the pinned order:
/// 1. recycle bin — EVERY candidate, persisted-hash ones included: a
///    recycle-bin file is not a candidate at all;
/// 2. persisted `hash_first` — reused verbatim, no open, no grace
///    ([`NO_HASH`] = not persisted, i.e. steps 3-7 apply);
/// 3. resolve (broken dir chain / unknown root segment → error);
/// 4. placeholder attributes — consulted BEFORE the open (never
///    touched; unstatable is an error, like the old pipeline);
/// 5. mtime grace ([`is_fresh`]) — may still be mid-write;
/// 6. open (a sharing violation is [`OpenSkip::Locked`], the rest an
///    error);
/// 7. read the first 4KiB (a read failure is an error).
///
/// A computed hash on a size <= 4KiB row is TERMINAL: the probe read IS
/// the whole file, so one double-`Some` [`HashUpdate`] persists both
/// hashes and `full` is recorded for the later gates. Every processed
/// candidate advances `done` — skips and reuses included.
#[allow(clippy::too_many_arguments)]
fn gate_head_step(
    worker: &mut GateWorker,
    k: usize,
    slot: usize,
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    now: i64,
) {
    let size = c.sizes[slot];

    // 1. Recycle bin — up front, every candidate.
    let parent_slot = c.parents[slot] as usize;
    if parent_slot < dirs.recycle.len() && dirs.recycle[parent_slot] {
        worker.skipped(Skip::Recycle, size);
        return;
    }

    // 2. Persisted hash_first: reuse (no open, no grace).
    let persisted = c.hash_first[slot];
    if persisted != NO_HASH {
        worker.head_hits.push((k, persisted));
        // Terminal bookkeeping: a <= 4KiB row's persisted first hash
        // covers the whole file; otherwise a persisted full hash passes
        // through as-is.
        if size <= FIRST_CHUNK as u64 {
            worker.full_hits.push((k, persisted));
        } else if c.hash_full[slot] != NO_HASH {
            worker.full_hits.push((k, c.hash_full[slot]));
        }
        worker.done += 1;
        return;
    }

    // 3. Resolve the absolute disk path.
    let Some(path) = c
        .index_path(slot, dirs)
        .and_then(|index_path| resolve_absolute_path(roots, &index_path))
    else {
        worker.skipped(Skip::Error, size);
        return;
    };

    // 4. Cloud placeholder — before the open (never touched).
    match reader.attributes(&path) {
        None => {
            // Unstatable (vanished/unreadable): an error, not a
            // placeholder — the old pipeline's precedent.
            worker.skipped(Skip::Error, size);
            return;
        }
        Some(attrs) if crate::scanner::is_placeholder_attr(attrs) => {
            worker.skipped(Skip::Placeholder, size);
            return;
        }
        Some(_) => {}
    }

    // 5. mtime grace — may still be mid-write.
    if is_fresh(c.mtimes[slot], now) {
        worker.skipped(Skip::Fresh, size);
        return;
    }

    // 6. Open (deny-write on Windows).
    let mut file = match reader.open(&path) {
        Ok(file) => file,
        Err(err) => {
            match classify_open_err(&err) {
                OpenSkip::Locked => worker.skipped(Skip::Locked, size),
                OpenSkip::Error => worker.skipped(Skip::Error, size),
            }
            return;
        }
    };

    // 7. First 4KiB — a short read hashes what came back (the mtime +
    //    size persist guard covers a file that changed under us).
    let mut chunk_buf = [0u8; FIRST_CHUNK];
    let mut filled = 0;
    let mut failed = false;
    while filled < chunk_buf.len() {
        match file.read(&mut chunk_buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    if failed {
        worker.skipped(Skip::Error, size);
        return;
    }
    let hash = xxh64(&chunk_buf[..filled], 0);
    worker.hashed_first += 1;
    worker.head_hits.push((k, hash));
    let terminal = size <= FIRST_CHUNK as u64;
    worker.pending.push(HashUpdate {
        id: FileId::from(c.ids[slot]),
        mtime: c.mtimes[slot],
        size,
        hash_first: Some(hash),
        // Terminal: the probe read IS the whole file.
        hash_full: terminal.then_some(hash),
    });
    if terminal {
        worker.full_hits.push((k, hash));
    }
    worker.done += 1;
}

/// Folds one worker's chunk results into the shared outcome: hits
/// scatter into the parallel arrays at `base + k` (`k` is the CHUNK-
/// local position the cursor dealt, `base` the chunk's start in the
/// global directory order — positions are disjoint across workers),
/// pending joins the batch, counters accumulate.
fn merge_worker(out: &mut HeadOutcome, done: &mut u64, base: usize, worker: GateWorker) {
    for (k, hash) in worker.head_hits {
        out.head[base + k] = hash;
    }
    for (k, hash) in worker.full_hits {
        out.full[base + k] = hash;
    }
    out.pending.extend(worker.pending);
    out.hashed_first += worker.hashed_first;
    let counters = &mut out.counters;
    counters.head_gate.files += worker.kills_files;
    counters.head_gate.bytes += worker.kills_bytes;
    counters.errors += worker.errors;
    counters.locked += worker.locked;
    counters.fresh += worker.fresh;
    counters.placeholders += worker.placeholders;
    counters.recycle += worker.recycle;
    *done += worker.done;
}

// ---------------------------------------------------------------------
// Gate ③ (pipeline rewrite, Task 5): the sampling layer. Consumes the
// (size, head)-grouped survivors of gate② and decides who pays gate④'s
// full read. Sample keys are NEVER persisted (zero schema migration);
// the only hash gate③ computes and stores is the ≤64KB terminal path's
// whole-file hash.

/// Sampled-read window in bytes: every sample window is exactly this
/// much, taken at the size-derived offsets of [`sample_window_offsets`].
pub(crate) const SAMPLE_WINDOW: usize = 64 * 1024;

/// The sampling-window offsets for a candidate of snapshot `size`:
/// `n = max(2, min(16, ceil(size / 256MB)))` windows of
/// [`SAMPLE_WINDOW`] bytes at `i × (size − 64KB) / n` for i = 1..=n —
/// i = n is the tail window, exactly at `size − 64KB`.
///
/// THE COMPARABILITY CORE of the sampling layer: `n` (hence the whole
/// offset set) is a pure function of `size`, so two candidates of equal
/// size always sample the SAME byte ranges and their sample keys (see
/// [`sample_key`]) are comparable. Ruling 2's table: 64KB+1 → 2, 256MB
/// → 2 (the boundary ceil of 1 lifts to the minimum), 256MB+1 → 2,
/// 512MB+1 → 3, 13.5GB → 16.
///
/// Precondition: `size > SAMPLE_WINDOW` (only sampled candidates call
/// this); smaller sizes degenerate to the single window `[0]` rather
/// than underflow the span.
pub(crate) fn sample_window_offsets(size: u64) -> Vec<u64> {
    if size <= SAMPLE_WINDOW as u64 {
        return vec![0];
    }
    const UNIT: u64 = 256 * 1024 * 1024; // 256MB — the formula's ceil unit
    let units = size.div_ceil(UNIT);
    let n = units.clamp(2, 16);
    let span = size - SAMPLE_WINDOW as u64;
    // `i × span / n` without overflow: span = q·n + r makes
    // floor(i·span/n) = q·i + floor(r·i/n), and r < n ≤ 16 keeps every
    // intermediate far below u64.
    let (q, r) = (span / n, span % n);
    (1..=n).map(|i| q * i + r * i / n).collect()
}

/// The sample key of a sampled candidate: its window hashes (in OFFSET
/// order) concatenated as little-endian u64s and xxh64'd (seed 0, the
/// crate-wide convention). A fixed 8 bytes, so the (size, key) run pass
/// groups without allocation. The offset order is load-bearing: same-
/// size candidates enumerate the same offsets (see
/// [`sample_window_offsets`]), so equal keys mean equal sampled bytes.
pub(crate) fn sample_key(hashes: &[u64]) -> u64 {
    let mut cascade = Vec::with_capacity(hashes.len() * 8);
    for hash in hashes {
        cascade.extend_from_slice(&hash.to_le_bytes());
    }
    xxh64(&cascade, 0)
}

/// gate③'s result — what Task 6's `run_gates` hands to gate④.
///
/// * `full_todo` — the candidate slots gate④ receives, IN THE INPUT
///   ORDER (the `(size, head)` run order of the `cand` argument):
///   certified members (never touched — their full hash is already
///   known from [`HeadOutcome::full`]/the SoA), short-circuited new
///   members (their run held a certified member), ≤64KB-terminal
///   members (full hash computed here — find it in [`SampleOutcome::small_full`]),
///   and sampled survivors (`(size, sample_key)` runs ≥2 — gate④
///   full-reads them). Skipped and singleton-killed members are absent.
/// * `small_full` — PARALLEL to `full_todo` (Ruling 1 shape): the
///   ≤64KB-terminal members' whole-file hashes computed by THIS gate,
///   [`NO_HASH`] for every other member (certified/short-circuited/
///   sampled carry no gate③-computed full). This side array is the
///   authoritative record of the terminal reads — it survives every
///   boundary flush, unlike `pending`, which is persistence-only and
///   DRAINED by each [`PERSIST_BATCH`]/[`PROGRESS_EVERY`] flush; the
///   caller merges fulls from HERE, never from `pending`.
/// * `counters` — gate③'s tally: `sample_gate` counts every candidate
///   that left without `full_todo` membership (open/read skips +
///   singleton-key kills — `files` equals `errors + locked + fresh +
///   placeholders + singleton kills`, mirroring gate②'s inclusive
///   `head_gate` equation); the skip-reason counters accumulate THIS
///   gate's skips into the same run-wide [`OutcomeCounters`] fields
///   (the caller folds gates ②③④ into the run totals);
///   `size_gate`/`head_gate` stay zero (gate② owns them).
/// * `pending` — computed [`HashUpdate`]s not yet persisted. ONLY the
///   ≤64KB-terminal path produces updates: `hash_first: None` (gate②
///   already persisted or queued it — the store's COALESCE write leaves
///   the column untouched), `hash_full: Some`. Sample keys are never
///   persisted. Chunk boundaries flush at [`PERSIST_BATCH`]; a
///   CANCELLED gate has flushed before returning; otherwise the
///   residue travels back for the caller's end-of-run flush. The
///   persistence record only — the VALUES for the merge live in
///   [`SampleOutcome::small_full`].
/// * `hashed_full_small` — full hashes computed by the ≤64KB terminal
///   path this gate (the sampled path computes none — that is gate④'s
///   job).
/// * `cancelled` — true when `cancel` fired and gate③ stopped between
///   files: unprocessed participants keep no result and the last
///   progress call reports `done < total`.
#[derive(Default)]
#[allow(dead_code)] // Task 6 wiring: run_gates consumes it.
pub(crate) struct SampleOutcome {
    pub(crate) full_todo: Vec<u32>,
    /// Parallel to `full_todo`: the gate③-computed terminal fulls,
    /// [`NO_HASH`] where the member's full is not this gate's product.
    pub(crate) small_full: Vec<u64>,
    pub(crate) counters: OutcomeCounters,
    pub(crate) pending: Vec<HashUpdate>,
    pub(crate) hashed_full_small: u64,
    pub(crate) cancelled: bool,
}

/// What gate③ does with one input member — the run-level classification
/// (Ruling 3), computed in one pass before any I/O. See [`gate_sample`].
#[derive(Clone, Copy)]
enum SampleKind {
    /// `full` known (gate② terminal or persisted): to `full_todo` free.
    Certified,
    /// NEW member of a run holding a certified member: short-circuit to
    /// `full_todo` without sampling — its sample could not be compared
    /// to a certified member's (neither computed nor stored), so the
    /// only sound path is gate④'s full read.
    ShortCircuit,
    /// NEW member of a clean run, `size ≤ 64KB`: whole-read terminal.
    TerminalSmall,
    /// NEW member of a clean run, `size > 64KB`: multi-window sample.
    Sampled,
}

/// gate③ — the sampling layer.
///
/// INPUT CONTRACT (the caller's — Task 6's `run_gates`): `cand` holds
/// the members of gate②'s `(size, head)` runs with ≥2 members,
/// FLATTENED IN RUN ORDER — equal `(size, head)` contiguous, sizes
/// ascending, `head == NO_HASH` members (gate② skips) excluded. The
/// sort is load-bearing for the singleton kills below: equal
/// `(size, sample_key)` members must be contiguous or true pairs are
/// lost. Values resolve through `head`'s triplet (Ruling 1): the gate
/// builds a slot → position map over [`HeadOutcome::cand`], and
/// `head.head[p]`/`head.full[p]` belong to candidate slot
/// `head.cand[p]`.
///
/// PER-MEMBER CLASSIFICATION (Ruling 3): a member is CERTIFIED when
/// `head.full[p] != NO_HASH || c.hash_full[slot] != NO_HASH` (gate②
/// terminal or a persisted full hash), NEW otherwise.
///
/// * Certified members go to [`SampleOutcome::full_todo`] untouched —
///   they never open.
/// * A run holding ANY certified member short-circuits: its NEW
///   members go straight to `full_todo` WITHOUT sampling (the
///   increment crowd — restored/rewritten files next to their
///   unchanged, already-hashed twins).
/// * Clean runs put their NEW members to work: `size ≤ 64KB` → one
///   open, whole read, terminal full hash (recorded in
///   [`SampleOutcome::small_full`], persisted via `pending`);
///   `size > 64KB` → one open, one seek +
///   [`SAMPLE_WINDOW`] read per window offset ([`sample_window_offsets`]),
///   `sample_key` per [`sample_key`], then ONE `(size, sample_key)`
///   contiguous-run grouping over ALL sampled members: runs ≥2 survive
///   to `full_todo`, singleton runs are killed into
///   [`OutcomeCounters::sample_gate`]. (Same-size candidates from
///   different `(size, head)` runs CAN merge here — a shared sample key
///   with differing first-4KB just means gate④ arbitrates; never
///   unsound.)
///
/// The open matrix applies IN FULL here (Ruling 4) — resolve
/// ([`resolve_absolute_path`], fail → error) → placeholder attributes →
/// mtime grace ([`is_fresh`]) → open (a sharing violation is
/// [`OpenSkip::Locked`]) → read — because a candidate carrying a
/// persisted `hash_first` first touches disk HERE (gate② reused it
/// without opening), while files already CERTIFIED never open at all.
/// The recycle-bin check is NOT repeated: gate② excluded those for
/// every candidate before anything else.
///
/// Workers/chunks/cancel/progress mirror gate②: participant chunks of
/// [`CHUNK`], an atomic cursor per chunk, worker-private state merged
/// at chunk boundaries, persist flushes at [`PERSIST_BATCH`], cancel
/// checked between files, `progress` gets phase-agnostic `(done,
/// total)` where total = the participants that actually enter gate③
/// work (terminal-small + sampled; certified/short-circuited members
/// are free and uncounted), ticked every [`PROGRESS_EVERY`] and once at
/// the end (or at the cancel point).
#[allow(dead_code)] // Task 6 wiring: run_gates calls it.
#[allow(clippy::too_many_arguments)] // the brief's pinned signature.
pub(crate) fn gate_sample(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    head: &HeadOutcome,
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
) -> SampleOutcome {
    gate_sample_chunked(
        c, dirs, cand, head, roots, reader, workers, cancel, now, progress, store, CHUNK,
    )
}

/// [`gate_sample`]'s body with the chunk size injected — the test seam
/// (mirrors [`gate_head_chunked`]).
#[allow(clippy::too_many_arguments)]
fn gate_sample_chunked(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    head: &HeadOutcome,
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
    chunk_size: usize,
) -> SampleOutcome {
    // Slot → position in gate②'s triplet (Ruling 1): the input's slots
    // address `head`/`full` through it.
    let mut slot_pos = vec![usize::MAX; c.ids.len()];
    for (p, &slot) in head.cand.iter().enumerate() {
        slot_pos[slot as usize] = p;
    }
    let certified_at = |k: usize| -> bool {
        let slot = cand[k] as usize;
        let p = slot_pos[slot];
        head.full[p] != NO_HASH || c.hash_full[slot] != NO_HASH
    };

    // Run classification (Ruling 3), one pass over the input's
    // contiguous (size, head) runs — no I/O yet.
    let mut kinds = Vec::with_capacity(cand.len());
    let mut k = 0usize;
    while k < cand.len() {
        let slot = cand[k] as usize;
        let size = c.sizes[slot];
        let head_hash = head.head[slot_pos[slot]];
        let mut end = k + 1;
        let mut run_certified = certified_at(k);
        while end < cand.len()
            && c.sizes[cand[end] as usize] == size
            && head.head[slot_pos[cand[end] as usize]] == head_hash
        {
            run_certified |= certified_at(end);
            end += 1;
        }
        for i in k..end {
            kinds.push(if certified_at(i) {
                SampleKind::Certified
            } else if run_certified {
                SampleKind::ShortCircuit
            } else if size <= SAMPLE_WINDOW as u64 {
                SampleKind::TerminalSmall
            } else {
                SampleKind::Sampled
            });
        }
        k = end;
    }

    // The participants: members that actually work (open/read), in
    // input order — the chunk machinery walks THIS list.
    let mut participants: Vec<usize> = Vec::new();
    let mut participant_of = vec![usize::MAX; cand.len()];
    for (i, kind) in kinds.iter().enumerate() {
        if matches!(kind, SampleKind::TerminalSmall | SampleKind::Sampled) {
            participant_of[i] = participants.len();
            participants.push(i);
        }
    }
    let total = participants.len() as u64;

    let mut out = SampleOutcome::default();
    let mut results_key: Vec<Option<u64>> = vec![None; participants.len()];
    let mut removed = vec![false; participants.len()];
    // Participant-indexed terminal fulls — the flush-proof record of
    // the ≤64KB whole reads (pending is persistence-only and DRAINED
    // by every boundary flush; these values must outlive it).
    let mut terminal_full = vec![NO_HASH; participants.len()];
    let mut done: u64 = 0;
    let mut tick: u64 = 0;

    let mut pos = 0usize;
    while pos < participants.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end = (pos + chunk_size).min(participants.len());
        let chunk = &participants[pos..end];
        let next = AtomicUsize::new(0);
        // A `move` closure captures these references by value (the
        // cursor and the shared classification must stay shared, not
        // be moved into one worker).
        let cursor: &AtomicUsize = &next;
        let kinds_ref: &[SampleKind] = &kinds;

        let (tx, rx) = mpsc::channel::<SampleWorker>();
        if workers <= 1 {
            let mut worker = SampleWorker::default();
            gate_sample_chunk(
                &mut worker, chunk, cand, kinds_ref, c, dirs, roots, reader, cancel, now, cursor,
            );
            let _ = tx.send(worker);
        } else {
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let mut worker = SampleWorker::default();
                        gate_sample_chunk(
                            &mut worker, chunk, cand, kinds_ref, c, dirs, roots, reader, cancel,
                            now, cursor,
                        );
                        let _ = tx.send(worker);
                    });
                }
            });
        }
        drop(tx); // scope has joined; the channel drains and closes
        while let Ok(worker) = rx.recv() {
            // `pos` is this chunk's base in the participant list — the
            // workers deal chunk-local participant positions, the
            // result arrays are global (the gate② scatter rule).
            merge_sample_worker(
                &mut out, &mut results_key, &mut removed, &mut terminal_full, &mut done, pos,
                worker,
            );
        }

        // Chunk boundary: batched persist (one short lock per flush)
        // and progress ticks.
        if out.pending.len() >= PERSIST_BATCH {
            flush_persist(store, &mut out.pending);
        }
        let crossed = done / PROGRESS_EVERY;
        if crossed > tick {
            flush_persist(store, &mut out.pending);
            progress(done, total);
            tick = crossed;
        }
        pos = end;
    }

    // The (size, sample_key) grouping over the sampled results:
    // removed members drop out first, the rest keep their input order
    // (sizes ascend), so equal keys are contiguous. Runs ≥2 survive;
    // singleton runs are killed into sample_gate.
    let mut survived = vec![false; participants.len()];
    let valid: Vec<usize> = (0..participants.len())
        .filter(|&j| !removed[j] && results_key[j].is_some())
        .collect();
    let run_key = |j: usize| -> (u64, u64) {
        let slot = cand[participants[j]] as usize;
        (c.sizes[slot], results_key[j].unwrap())
    };
    let mut run = 0usize;
    for w in 1..=valid.len() {
        if w < valid.len() && run_key(valid[w]) == run_key(valid[run]) {
            continue;
        }
        if w - run >= 2 {
            for &j in &valid[run..w] {
                survived[j] = true;
            }
        } else {
            let slot = cand[participants[valid[run]]] as usize;
            out.counters.sample_gate.files += 1;
            out.counters.sample_gate.bytes += c.sizes[slot];
        }
        run = w;
    }

    // full_todo in input order: the free members, the survivors, and
    // the terminal small reads; skips and singleton kills stay out.
    // `small_full` rides PARALLEL to it (the gate③-computed terminal
    // fulls, NO_HASH for every other member) — the lockstep push is the
    // Ruling-1 contract, so a desync would break the caller's merge.
    for (i, &slot) in cand.iter().enumerate() {
        match kinds[i] {
            SampleKind::Certified | SampleKind::ShortCircuit => {
                out.full_todo.push(slot);
                out.small_full.push(NO_HASH);
            }
            SampleKind::TerminalSmall => {
                let full = terminal_full[participant_of[i]];
                if full != NO_HASH {
                    // Read succeeded — the terminal hash is recorded in
                    // `terminal_full` (and persisted via `pending`).
                    // (A skip or a member a cancel left unprocessed
                    // stays out.)
                    out.full_todo.push(slot);
                    out.small_full.push(full);
                }
            }
            SampleKind::Sampled => {
                if survived[participant_of[i]] {
                    out.full_todo.push(slot);
                    out.small_full.push(NO_HASH);
                }
            }
        }
    }
    debug_assert_eq!(
        out.full_todo.len(),
        out.small_full.len(),
        "small_full rides parallel to full_todo"
    );

    // Cancel may have fired mid-chunk or between chunks — workers stop
    // at file boundaries either way: flag it and flush the in-flight
    // batch so nothing dangles (the gate② contract). A clean end keeps
    // the last (sub-batch) residue in `pending` for the caller.
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        flush_persist(store, &mut out.pending);
    }
    progress(done, total);
    out
}

/// One sampling worker's PRIVATE chunk state: results indexed by
/// CHUNK-LOCAL participant position (disjoint across workers; the merge
/// adds the chunk's base — the gate② scatter rule), computed updates
/// queued for persistence. Nothing is shared while a chunk runs; the
/// store lock is taken only at the boundary flush.
#[derive(Default)]
struct SampleWorker {
    /// `(chunk-local participant, sample key)` — the sampled path's
    /// successes.
    keys: Vec<(usize, u64)>,
    /// `(chunk-local participant, whole-file hash)` — the ≤64KB
    /// terminal path's successes. Success is marked EXPLICITLY with the
    /// hash itself (a member a cancel left unprocessed must not be
    /// mistaken for a success — the default is "not done"), and the
    /// hash must NOT be read back out of `pending`: the boundary
    /// flushes drain it mid-gate at scale.
    small_fulls: Vec<(usize, u64)>,
    /// `(chunk-local participant, reason, snapshot size)` — every skip;
    /// the merge folds these into the reason counters and the
    /// `sample_gate` kill tally.
    removed: Vec<(usize, Skip, u64)>,
    pending: Vec<HashUpdate>,
    hashed_full_small: u64,
    done: u64,
}

impl SampleWorker {
    /// Records one skipped participant (the reason counter itself is
    /// folded at the merge) and advances `done`.
    fn skipped(&mut self, skip: Skip, j: usize, size: u64) {
        self.removed.push((j, skip, size));
        self.done += 1;
    }
}

/// The worker loop over one participant chunk: an atomic cursor hands
/// out chunk-local participant positions (each is processed exactly
/// once); cancel is checked at every file boundary, so a cancelled run
/// stops promptly.
#[allow(clippy::too_many_arguments)]
fn gate_sample_chunk(
    worker: &mut SampleWorker,
    chunk: &[usize],
    cand: &[u32],
    kinds: &[SampleKind],
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    cancel: &AtomicBool,
    now: i64,
    next: &AtomicUsize,
) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let j = next.fetch_add(1, Ordering::Relaxed);
        if j >= chunk.len() {
            return;
        }
        let input_pos = chunk[j];
        gate_sample_step(
            worker,
            j,
            cand[input_pos] as usize,
            matches!(kinds[input_pos], SampleKind::Sampled),
            c,
            dirs,
            roots,
            reader,
            now,
        );
    }
}

/// The per-participant step. The skip matrix is gate②'s WITHOUT the
/// recycle and reuse steps (Ruling 4): 1. resolve; 2. placeholder
/// attributes (BEFORE the open — never hydrate; unstatable is an
/// error); 3. mtime grace; 4. open (a sharing violation is
/// [`OpenSkip::Locked`], the rest an error); 5. read — the ≤64KB path
/// streams the whole file into one terminal full hash (the update
/// carries `hash_first: None`: gate② already persisted or queued it),
/// the sampled path seeks to each window offset and reads
/// [`SAMPLE_WINDOW`] bytes (a short tail read hashes what came back —
/// the persist guard covers a file that changed under us). Any failure
/// removes the participant — nothing partial ever reaches the key
/// grouping.
#[allow(clippy::too_many_arguments)]
fn gate_sample_step(
    worker: &mut SampleWorker,
    j: usize,
    slot: usize,
    sampled: bool,
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    now: i64,
) {
    let size = c.sizes[slot];

    // 1. Resolve the absolute disk path.
    let Some(path) = c
        .index_path(slot, dirs)
        .and_then(|index_path| resolve_absolute_path(roots, &index_path))
    else {
        worker.skipped(Skip::Error, j, size);
        return;
    };

    // 2. Cloud placeholder — before the open (never touched).
    match reader.attributes(&path) {
        None => {
            worker.skipped(Skip::Error, j, size);
            return;
        }
        Some(attrs) if crate::scanner::is_placeholder_attr(attrs) => {
            worker.skipped(Skip::Placeholder, j, size);
            return;
        }
        Some(_) => {}
    }

    // 3. mtime grace — may still be mid-write.
    if is_fresh(c.mtimes[slot], now) {
        worker.skipped(Skip::Fresh, j, size);
        return;
    }

    // 4. Open (deny-write on Windows).
    let mut file = match reader.open(&path) {
        Ok(file) => file,
        Err(err) => {
            worker.skipped(
                match classify_open_err(&err) {
                    OpenSkip::Locked => Skip::Locked,
                    OpenSkip::Error => Skip::Error,
                },
                j,
                size,
            );
            return;
        }
    };

    if sampled {
        // 5b. One open, one seek + window read per offset; the key
        // hashes the windows in OFFSET order (the comparability
        // contract). A failed seek or read skips the file — no partial
        // keys.
        let mut buf = vec![0u8; SAMPLE_WINDOW];
        let mut hashes = Vec::new();
        for offset in sample_window_offsets(size) {
            match read_window(&mut file, offset, &mut buf) {
                Some(filled) => hashes.push(xxh64(&buf[..filled], 0)),
                None => {
                    worker.skipped(Skip::Error, j, size);
                    return;
                }
            }
        }
        worker.keys.push((j, sample_key(&hashes)));
    } else {
        // 5a. Whole read → terminal full hash (persist-guarded).
        let mut hasher = Xxh64::new(0);
        let mut buffer = vec![0u8; READ_BUFFER];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buffer[..n]),
                Err(_) => {
                    worker.skipped(Skip::Error, j, size);
                    return;
                }
            }
        }
        let hash = hasher.digest();
        worker.hashed_full_small += 1;
        worker.small_fulls.push((j, hash));
        worker.pending.push(HashUpdate {
            id: FileId::from(c.ids[slot]),
            mtime: c.mtimes[slot],
            size,
            hash_first: None,
            hash_full: Some(hash),
        });
    }
    worker.done += 1;
}

/// Seeks to `offset` and reads up to `buf.len()` bytes (early EOF
/// allowed — a file that shrank below its snapshot size hashes the
/// short window). `None` on seek or read failure.
fn read_window(file: &mut std::fs::File, offset: u64, buf: &mut [u8]) -> Option<usize> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    Some(filled)
}

/// Folds one worker's chunk results into the shared outcome: sample
/// keys scatter into the participant-indexed array at `base + j` (`j`
/// is the CHUNK-LOCAL participant position, `base` the chunk's start —
/// positions are disjoint across workers), terminal-small successes
/// scatter their whole-file hashes into the participant-indexed
/// `terminal_full` array EXPLICITLY (a cancel-left-unprocessed member
/// must not pass, and the value must not be recovered from `pending` —
/// the boundary flushes drain it), removals fold into the reason
/// counters AND the `sample_gate` kill tally (the inclusive equation),
/// pending and `done` accumulate.
fn merge_sample_worker(
    out: &mut SampleOutcome,
    results_key: &mut [Option<u64>],
    removed: &mut [bool],
    terminal_full: &mut [u64],
    done: &mut u64,
    base: usize,
    worker: SampleWorker,
) {
    for (j, key) in worker.keys {
        results_key[base + j] = Some(key);
    }
    for (j, hash) in worker.small_fulls {
        terminal_full[base + j] = hash;
    }
    for (j, skip, size) in worker.removed {
        removed[base + j] = true;
        let counters = &mut out.counters;
        match skip {
            Skip::Error => counters.errors += 1,
            Skip::Locked => counters.locked += 1,
            Skip::Fresh => counters.fresh += 1,
            Skip::Placeholder => counters.placeholders += 1,
            Skip::Recycle => counters.recycle += 1,
        }
        counters.sample_gate.files += 1;
        counters.sample_gate.bytes += size;
    }
    out.pending.extend(worker.pending);
    out.hashed_full_small += worker.hashed_full_small;
    *done += worker.done;
}

/// The regroup between gate② and gate③ — the input [`gate_sample`]
/// documents, and run_gates (Task 6) owns: gate②'s triplet re-sorted by
/// `(size, head)` (sizes ascending), `head == NO_HASH` members (gate②
/// skips) dropped, and only the contiguous `(size, head)` runs of two
/// or more members kept, flattened in run order. This exact shape used
/// to live only as the gate③ tests' helper; it IS the production path.
fn sample_input(c: &Candidates, head: &HeadOutcome) -> Vec<u32> {
    let mut order: Vec<usize> = (0..head.cand.len())
        .filter(|&p| head.head[p] != NO_HASH)
        .collect();
    order.sort_by_key(|&p| (c.sizes[head.cand[p] as usize], head.head[p]));
    let mut out = Vec::new();
    let key = |&p: &usize| (c.sizes[head.cand[p] as usize], head.head[p]);
    let mut run = 0usize;
    for w in 1..=order.len() {
        if w < order.len() && key(&order[w]) == key(&order[run]) {
            continue;
        }
        if w - run >= 2 {
            out.extend(order[run..w].iter().map(|&p| head.cand[p]));
        }
        run = w;
    }
    out
}

// ---------------------------------------------------------------------
// Gate ④ (pipeline rewrite, Task 6): full-hash certification of gate③'s
// `full_todo`, and `run_gates` — the assembly chaining gates ①→②→③→④
// into the complete pipeline the engine swaps onto (Task 7).

/// gate④'s result — the gate②/③ result shape: the full hashes THIS gate
/// computed (parallel to the `cand` slice it was handed), the gate's
/// counters, its pending updates, its cancel flag.
///
/// * `full[k]` — whole-file xxh64 of `cand[k]`, [`NO_HASH`] when not
///   obtained (skip, or a file a cancel left unprocessed). Fulls the
///   caller already knew before gate④ (the `known` slice) are NOT
///   echoed here — the caller holds them already.
/// * `pending` — computed [`HashUpdate`]s not yet persisted
///   (`hash_first: None`: gate② already persisted or queued the first
///   hash; the store's COALESCE write leaves the column untouched).
///   Chunk boundaries flush at [`PERSIST_BATCH`]; a CANCELLED gate has
///   flushed before returning.
/// * `hashed_full` — full hashes computed by this gate (the known-full
///   reuses are not counted).
/// * `cancelled` — true when `cancel` fired and gate④ stopped.
#[derive(Default)]
#[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
struct FullOutcome {
    full: Vec<u64>,
    counters: OutcomeCounters,
    pending: Vec<HashUpdate>,
    hashed_full: u64,
    cancelled: bool,
}

/// One gate④ worker's PRIVATE chunk state (the gate②/③ worker shape):
/// hits indexed by CHUNK-LOCAL position, pending updates, skip-reason
/// counters, done. No kill tally: [`OutcomeCounters`] has no gate④ kill
/// field — a full-certification skip is a skip reason only, and the
/// file simply stays out of the final grouping.
#[derive(Default)]
#[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
struct FullWorker {
    /// `(chunk-local position, hash)` for files whose full read
    /// completed.
    hits: Vec<(usize, u64)>,
    pending: Vec<HashUpdate>,
    hashed_full: u64,
    errors: u64,
    locked: u64,
    fresh: u64,
    placeholders: u64,
    done: u64,
}

impl FullWorker {
    /// Records one skipped file (the reason counter) and advances
    /// `done`. No kill tally: gate④ has no `*_gate` counter — the file
    /// just stays out of the final grouping. The [`Skip::Recycle`] arm
    /// is unreachable — gate④ never checks the recycle bit (gate②
    /// excluded those candidates before anything else, and every
    /// full_todo member passed it).
    fn skipped(&mut self, skip: Skip) {
        match skip {
            Skip::Error => self.errors += 1,
            Skip::Locked => self.locked += 1,
            Skip::Fresh => self.fresh += 1,
            Skip::Placeholder => self.placeholders += 1,
            Skip::Recycle => {}
        }
        self.done += 1;
    }
}

/// The per-file gate④ step: the full open matrix (resolve → placeholder
/// attributes → mtime grace → open → read) applied uniformly — most
/// full_todo members were never opened this run (certified members
/// never are), and state can change between gates. NO recycle step (see
/// [`FullWorker::skipped`]). The open carries
/// [`FileReader::open_sequential`] (deny-write + the Windows
/// sequential-scan hint). The read streams [`READ_BUFFER`] steps and
/// CANCEL IS CHECKED AT EVERY STEP — the design's sub-second cancel for
/// giant files. A cancelled step abandons the file: a partial hash is
/// not the file's full hash and must never be persisted or grouped
/// (the worker loop's next boundary check stops the pull).
#[allow(clippy::too_many_arguments)]
fn gate_full_step(
    worker: &mut FullWorker,
    k: usize,
    slot: usize,
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    cancel: &AtomicBool,
    now: i64,
) {
    let size = c.sizes[slot];

    // 1. Resolve the absolute disk path.
    let Some(path) = c
        .index_path(slot, dirs)
        .and_then(|index_path| resolve_absolute_path(roots, &index_path))
    else {
        worker.skipped(Skip::Error);
        return;
    };

    // 2. Cloud placeholder — before the open (never touched).
    match reader.attributes(&path) {
        None => {
            // Unstatable (vanished/unreadable): an error, not a
            // placeholder — the old pipeline's precedent.
            worker.skipped(Skip::Error);
            return;
        }
        Some(attrs) if crate::scanner::is_placeholder_attr(attrs) => {
            worker.skipped(Skip::Placeholder);
            return;
        }
        Some(_) => {}
    }

    // 3. mtime grace — may still be mid-write.
    if is_fresh(c.mtimes[slot], now) {
        worker.skipped(Skip::Fresh);
        return;
    }

    // 4. Open (deny-write + sequential-scan hint on Windows).
    let mut file = match reader.open_sequential(&path) {
        Ok(file) => file,
        Err(err) => {
            worker.skipped(match classify_open_err(&err) {
                OpenSkip::Locked => Skip::Locked,
                OpenSkip::Error => Skip::Error,
            });
            return;
        }
    };

    // 5. Streaming whole read — cancel at every buffer step.
    let mut hasher = Xxh64::new(0);
    let mut buffer = vec![0u8; READ_BUFFER];
    loop {
        if cancel.load(Ordering::Relaxed) {
            // Abandoned mid-file: no hash, no update, no `done` — the
            // run reports `cancelled` and keeps the group list empty.
            return;
        }
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(_) => {
                worker.skipped(Skip::Error);
                return;
            }
        }
    }
    let hash = hasher.digest();
    worker.hashed_full += 1;
    worker.hits.push((k, hash));
    worker.pending.push(HashUpdate {
        id: FileId::from(c.ids[slot]),
        mtime: c.mtimes[slot],
        size,
        hash_first: None,
        hash_full: Some(hash),
    });
    worker.done += 1;
}

/// The worker loop over one gate④ chunk (the gate②/③ loop): an atomic
/// cursor hands out CHUNK-LOCAL positions; cancel is checked at every
/// file boundary AND inside each read ([`gate_full_step`]). Members with
/// an already-known full (`known_chunk[j] != NO_HASH`) advance `done`
/// without any I/O — the incremental crowd pays nothing, exactly like
/// gate②'s persisted-hash reuses.
#[allow(clippy::too_many_arguments)]
fn gate_full_chunk(
    worker: &mut FullWorker,
    cand_chunk: &[u32],
    known_chunk: &[u64],
    c: &Candidates,
    dirs: &Dirs,
    roots: &[String],
    reader: &dyn FileReader,
    cancel: &AtomicBool,
    now: i64,
    next: &AtomicUsize,
) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let k = next.fetch_add(1, Ordering::Relaxed);
        if k >= cand_chunk.len() {
            return;
        }
        if known_chunk[k] != NO_HASH {
            worker.done += 1; // known full: no open, progress advances
            continue;
        }
        gate_full_step(
            worker, k, cand_chunk[k] as usize, c, dirs, roots, reader, cancel, now,
        );
    }
}

/// [`gate_full`]'s body with the chunk size injected — the test seam
/// (mirrors [`gate_head_chunked`]/[`gate_sample_chunked`]). `cand` is
/// gate③'s `full_todo` IN ITS OWN ORDER (the `(size, head)` run order —
/// sizes ascending), consumed as-is; `known` is parallel to it with the
/// full hashes the caller already holds.
#[allow(clippy::too_many_arguments)]
fn gate_full_chunked(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    known: &[u64],
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
    chunk_size: usize,
) -> FullOutcome {
    debug_assert_eq!(cand.len(), known.len(), "the known slice rides parallel to cand");
    let total = cand.len() as u64;
    let mut out = FullOutcome {
        full: vec![NO_HASH; cand.len()],
        ..FullOutcome::default()
    };
    let mut done: u64 = 0;
    let mut tick: u64 = 0;

    let mut pos = 0usize;
    while pos < cand.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let end = (pos + chunk_size).min(cand.len());
        let next = AtomicUsize::new(0);
        // A `move` closure captures this reference by value (the
        // cursor itself must stay shared, not moved into one worker).
        let cursor: &AtomicUsize = &next;

        let (tx, rx) = mpsc::channel::<FullWorker>();
        if workers <= 1 {
            let mut worker = FullWorker::default();
            gate_full_chunk(
                &mut worker,
                &cand[pos..end],
                &known[pos..end],
                c, dirs, roots, reader, cancel, now, cursor,
            );
            let _ = tx.send(worker);
        } else {
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let tx = tx.clone();
                    scope.spawn(move || {
                        let mut worker = FullWorker::default();
                        gate_full_chunk(
                            &mut worker,
                            &cand[pos..end],
                            &known[pos..end],
                            c, dirs, roots, reader, cancel, now, cursor,
                        );
                        let _ = tx.send(worker);
                    });
                }
            });
        }
        drop(tx); // scope has joined; the channel drains and closes
        while let Ok(worker) = rx.recv() {
            // `pos` is this chunk's base in the global full_todo order —
            // the workers deal chunk-local positions, the result arrays
            // are global (the gate② scatter rule).
            merge_full_worker(&mut out, &mut done, pos, worker);
        }

        // Chunk boundary: batched persist (one short lock per flush)
        // and progress ticks.
        if out.pending.len() >= PERSIST_BATCH {
            flush_persist(store, &mut out.pending);
        }
        let crossed = done / PROGRESS_EVERY;
        if crossed > tick {
            flush_persist(store, &mut out.pending);
            progress(done, total);
            tick = crossed;
        }
        pos = end;
    }

    // Cancel may have fired mid-read (per buffer step) or between
    // files — flag it and flush the in-flight batch: hashes already
    // computed land even on a cancelled run. A clean end leaves the
    // last (sub-batch) residue in `pending` for the caller.
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        flush_persist(store, &mut out.pending);
    }
    progress(done, total);
    out
}

/// gate④ — full-hash certification of gate③'s `full_todo` (the plan's
/// "仅闸③幸存者"): 64KB streaming xxh64 with the cancel checkpoint at
/// EVERY buffer step, workers/chunks/persist/progress mirroring gate②
/// ([`CHUNK`] chunks, an atomic cursor per chunk, worker-private state
/// merged at boundaries, [`PERSIST_BATCH`] flushes, [`PROGRESS_EVERY`]
/// ticks, `workers <= 1` inline). Progress total is EVERY member handed
/// in — members whose full is already known (the `known` slice,
/// run_gates' merge of the persisted/gate②/gate③ values) advance `done`
/// without I/O, so incremental runs report a full pass at zero opens.
#[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
#[allow(clippy::too_many_arguments)] // the gate's context, passed through.
fn gate_full(
    c: &Candidates,
    dirs: &Dirs,
    cand: &[u32],
    known: &[u64],
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(u64, u64),
    store: &Mutex<Store>,
) -> FullOutcome {
    gate_full_chunked(
        c, dirs, cand, known, roots, reader, workers, cancel, now, progress, store, CHUNK,
    )
}

/// Folds one gate④ worker's chunk results into the shared outcome: hits
/// scatter into the parallel array at `base + k` (`k` is the CHUNK-
/// local position the cursor dealt, `base` the chunk's start — positions
/// are disjoint across workers), pending joins the batch, counters and
/// `done` accumulate.
fn merge_full_worker(out: &mut FullOutcome, done: &mut u64, base: usize, worker: FullWorker) {
    for (k, hash) in worker.hits {
        out.full[base + k] = hash;
    }
    out.pending.extend(worker.pending);
    out.hashed_full += worker.hashed_full;
    let counters = &mut out.counters;
    counters.errors += worker.errors;
    counters.locked += worker.locked;
    counters.fresh += worker.fresh;
    counters.placeholders += worker.placeholders;
    *done += worker.done;
}

/// Folds the run's hash counts into the outcome, mirrors the legacy
/// scalar skips (engine.rs reads them until Task 7 swaps the engine),
/// marks the run cancelled and reports [`DedupPhase::Cancelled`] as the
/// FINAL progress report (no `Done`; groups stay empty — the engine
/// keeps serving the last completed run's list).
fn finish_cancelled(
    outcome: &mut DedupOutcome,
    hashed_first: u64,
    hashed_full: u64,
    last: (u64, u64),
    progress: &dyn Fn(DedupPhase, u64, u64),
) {
    outcome.hashed_first = hashed_first;
    outcome.hashed_full = hashed_full;
    outcome.errors = outcome.counters.errors;
    outcome.locked = outcome.counters.locked;
    outcome.placeholders = outcome.counters.placeholders;
    outcome.cancelled = true;
    progress(DedupPhase::Cancelled, last.0, last.1);
}

/// The rewritten pipeline (Task 6): gates ①→②→③→④ chained over the
/// streamed [`Candidates`] snapshot — what Task 7 swaps the engine off
/// [`run_pipeline`] onto. The store is locked only for the candidate
/// load and short persist batches (hashing never holds it); disk access
/// goes through `reader`; `workers` sizes the gates' worker pools
/// (`<= 1` runs inline); `now` injects the clock (the mtime grace);
/// `cancel` stops the run at any gate's file boundary and, in gate④,
/// at every 64KB read step.
///
/// Phases & totals (the engine's progress events):
/// * [`DedupPhase::Grouping`] — gate①, total = every loaded candidate;
/// * [`DedupPhase::HashingFirst`] — gate②, total = every size-gate
///   survivor (persisted-hash reuses included);
/// * [`DedupPhase::Sampling`] — gate③, total = the participants that
///   actually work (terminal-small + sampled; certified and
///   short-circuited members are free) — only gate③ knows that count,
///   so run_gates' phase-start marker for this gate reads `(0, 0)`;
/// * [`DedupPhase::HashingFull`] — gate④, total = every full_todo
///   member (known-full reuses advance `done` too);
/// * [`DedupPhase::Done`] — done = total = first + full hashes computed
///   THIS run (0 = fully incremental).
///
/// FULL-MERGE SEMANTICS (for the Task 7 implementer): a candidate's
/// authoritative full hash for the final grouping is the FIRST hit of —
/// gate④'s freshly computed value, then gate③'s terminal-small value
/// (computed this run; found in the [`SampleOutcome::small_full`]
/// side array — flush-proof, unlike `pending`),
/// then gate②'s [`HeadOutcome::full`] (the ≤4KiB terminal or the
/// persisted-full passthrough), then the SoA `hash_full` column
/// (persisted) — the sources agree wherever they overlap. The grouping
/// POPULATION is gate②'s survivors only (`head != NO_HASH`): files
/// killed or skipped at any gate stay out even when a stale persisted
/// full exists (a recycle-bin row must never group). Known-full members
/// are grouped by `(size, full)` contiguous runs >= 2 into
/// [`StoredGroup`]s — members id-ascending, groups size-descending /
/// hash-ascending, byte-for-byte the old pipeline's order (the Task 12
/// 对拍 baseline).
///
/// Cancel: a hit at ANY checkpoint (gate④'s per-buffer-step checks
/// included) ends the run right there — each gate flushes its own
/// pending on cancel and run_gates flushes each completed gate's
/// residue, so every computed hash is persisted; `cancelled` is
/// reported, the group list comes back EMPTY, and the final progress
/// report carries [`DedupPhase::Cancelled`] (no `Done`).
///
/// Errors are never fatal: a candidate load failure (store error, a
/// files-row id past `u32::MAX`) yields the trivial run — every phase
/// reported at `(0, 0)`, empty groups, not cancelled.
#[allow(dead_code)] // Task 7 wiring: the engine swap onto run_gates consumes it.
#[allow(clippy::too_many_arguments)] // the plan's pinned signature.
pub(crate) fn run_gates(
    store: &Mutex<Store>,
    roots: &[String],
    reader: &dyn FileReader,
    workers: usize,
    cancel: &AtomicBool,
    now: i64,
    progress: &dyn Fn(DedupPhase, u64, u64),
) -> DedupOutcome {
    let mut outcome = DedupOutcome::default();
    let Some((cands, dirs)) = load_candidates(store) else {
        // Trivial run (mirrors the old load_snapshot semantics): the
        // phase walk still happens, so subscribers see a complete cycle.
        progress(DedupPhase::Grouping, 0, 0);
        progress(DedupPhase::HashingFirst, 0, 0);
        progress(DedupPhase::Sampling, 0, 0);
        progress(DedupPhase::HashingFull, 0, 0);
        progress(DedupPhase::Done, 0, 0);
        return outcome;
    };

    // The last (done, total) the gates reported — the Cancelled finish
    // re-reports it under its own phase. Progress only ever fires from
    // this orchestration thread (the gates tick at chunk boundaries,
    // after their workers joined), so a plain Cell carries it.
    let last = std::cell::Cell::new((0u64, 0u64));
    let head_progress = |done: u64, total: u64| {
        last.set((done, total));
        progress(DedupPhase::HashingFirst, done, total);
    };
    let sample_progress = |done: u64, total: u64| {
        last.set((done, total));
        progress(DedupPhase::Sampling, done, total);
    };
    let full_progress = |done: u64, total: u64| {
        last.set((done, total));
        progress(DedupPhase::HashingFull, done, total);
    };

    let total_all = cands.ids.len() as u64;

    // ---- gate①: the size gate (in-memory — a file can only pair with
    // an equal-size one). ----
    progress(DedupPhase::Grouping, 0, total_all);
    let mut size_counters = OutcomeCounters::default();
    let sized = gate_size(&cands, &mut size_counters);
    progress(DedupPhase::Grouping, total_all, total_all);

    // ---- gate②: the first-4KiB probe. Total counts EVERY survivor —
    // skips AND reuses advance `done` (gate②'s UX contract). ----
    progress(DedupPhase::HashingFirst, 0, sized.len() as u64);
    let mut head = gate_head(
        &cands, &dirs, &sized, roots, reader, workers, cancel, now, &head_progress, store,
    );
    // The gate flushed its own pending on cancel; a clean end left the
    // sub-batch residue — either way persist it before anything else.
    flush_persist(store, &mut head.pending);
    if head.cancelled {
        outcome.counters = size_counters;
        outcome.counters.accumulate(&head.counters);
        finish_cancelled(&mut outcome, head.hashed_first, 0, last.get(), progress);
        return outcome;
    }

    // ---- gate③: the sampling layer over gate②'s `(size, head)` runs.
    // run_gates owns the input assembly ([`sample_input`]). ----
    progress(DedupPhase::Sampling, 0, 0); // marker; gate③ reports its own total
    let input = sample_input(&cands, &head);
    let mut sample = gate_sample(
        &cands, &dirs, &input, &head, roots, reader, workers, cancel, now, &sample_progress,
        store,
    );
    // Persist the terminal-small updates; the VALUES for the merge are
    // NOT read from here — `pending` is persistence-only and was
    // already drained by gate③'s own boundary flushes at scale. The
    // flush-proof record is the `SampleOutcome::small_full` side array.
    flush_persist(store, &mut sample.pending);
    if sample.cancelled {
        outcome.counters = size_counters;
        outcome.counters.accumulate(&head.counters);
        outcome.counters.accumulate(&sample.counters);
        finish_cancelled(
            &mut outcome,
            head.hashed_first,
            sample.hashed_full_small,
            last.get(),
            progress,
        );
        return outcome;
    }

    // ---- gate④: full-hash certification. Merge the fulls run_gates
    // already holds (see the doc comment's merge order) into a slice
    // parallel to full_todo — members with a known full skip the read
    // (the incremental crowd and gate③'s terminal smalls, whose values
    // live in the `SampleOutcome::small_full` side array), the rest get
    // the streaming pass. ----
    let mut slot_pos = vec![usize::MAX; cands.ids.len()];
    for (p, &slot) in head.cand.iter().enumerate() {
        slot_pos[slot as usize] = p;
    }
    debug_assert_eq!(
        sample.full_todo.len(),
        sample.small_full.len(),
        "gate③'s terminal fulls ride parallel to full_todo"
    );
    let mut small_full: HashMap<FileId, u64> = HashMap::new();
    for (&slot, &hash) in sample.full_todo.iter().zip(sample.small_full.iter()) {
        if hash != NO_HASH {
            small_full.insert(FileId::from(cands.ids[slot as usize]), hash);
        }
    }
    let known: Vec<u64> = sample
        .full_todo
        .iter()
        .map(|&slot| {
            let id = FileId::from(cands.ids[slot as usize]);
            small_full
                .get(&id)
                .copied()
                .or_else(|| {
                    let h = head.full[slot_pos[slot as usize]];
                    (h != NO_HASH).then_some(h)
                })
                .or_else(|| {
                    let h = cands.hash_full[slot as usize];
                    (h != NO_HASH).then_some(h)
                })
                .unwrap_or(NO_HASH)
        })
        .collect();

    progress(DedupPhase::HashingFull, 0, sample.full_todo.len() as u64);
    let mut full = gate_full(
        &cands,
        &dirs,
        &sample.full_todo,
        &known,
        roots,
        reader,
        workers,
        cancel,
        now,
        &full_progress,
        store,
    );
    flush_persist(store, &mut full.pending);
    if full.cancelled {
        outcome.counters = size_counters;
        outcome.counters.accumulate(&head.counters);
        outcome.counters.accumulate(&sample.counters);
        outcome.counters.accumulate(&full.counters);
        finish_cancelled(
            &mut outcome,
            head.hashed_first,
            sample.hashed_full_small + full.hashed_full,
            last.get(),
            progress,
        );
        return outcome;
    }

    // ---- Final grouping: gate② survivors with a known full (see the
    // doc comment's FULL-MERGE SEMANTICS), run-grouped by (size, full).
    // Rows carry (size, full, id) and sort as a tuple, so equal
    // (size, full) runs are contiguous AND their members come out
    // id-ascending. ----
    let fresh_full: HashMap<u32, u64> = sample
        .full_todo
        .iter()
        .zip(full.full.iter())
        .filter(|(_, &hash)| hash != NO_HASH)
        .map(|(&slot, &hash)| (slot, hash))
        .collect();
    let mut rows: Vec<(u64, u64, FileId)> = Vec::new();
    for (p, &slot) in head.cand.iter().enumerate() {
        if head.head[p] == NO_HASH {
            continue; // a gate② kill/skip stays out, stale persisted full or not
        }
        let id = FileId::from(cands.ids[slot as usize]);
        let full_hash = fresh_full
            .get(&slot)
            .copied()
            .or_else(|| small_full.get(&id).copied())
            .or({
                let h = head.full[p];
                (h != NO_HASH).then_some(h)
            })
            .or({
                let h = cands.hash_full[slot as usize];
                (h != NO_HASH).then_some(h)
            });
        let Some(full_hash) = full_hash else {
            continue; // killed later in the chain, or the full read failed
        };
        rows.push((cands.sizes[slot as usize], full_hash, id));
    }
    rows.sort_unstable();
    let mut groups: Vec<StoredGroup> = Vec::new();
    let mut run = 0usize;
    for w in 1..=rows.len() {
        if w < rows.len() && (rows[w].0, rows[w].1) == (rows[run].0, rows[run].1) {
            continue;
        }
        if w - run >= 2 {
            groups.push(StoredGroup {
                size: rows[run].0,
                hash: rows[run].1,
                members: rows[run..w].iter().map(|&(_, _, id)| id).collect(),
            });
        }
        run = w;
    }
    // Size descending for the stable paging contract; hash ascending as
    // the deterministic tie-break — the old pipeline's exact order.
    groups.sort_unstable_by(|a, b| b.size.cmp(&a.size).then(a.hash.cmp(&b.hash)));
    outcome.groups = groups;

    // ---- Outcome: the four gates' counters summed, the legacy scalar
    // mirrors folded from them (engine.rs reads those until Task 7). ----
    let mut counters = size_counters;
    counters.accumulate(&head.counters);
    counters.accumulate(&sample.counters);
    counters.accumulate(&full.counters);
    outcome.counters = counters;
    outcome.hashed_first = head.hashed_first;
    outcome.hashed_full = sample.hashed_full_small + full.hashed_full;
    outcome.errors = outcome.counters.errors;
    outcome.locked = outcome.counters.locked;
    outcome.placeholders = outcome.counters.placeholders;

    let hashed = outcome.hashed_first + outcome.hashed_full;
    progress(DedupPhase::Done, hashed, hashed);
    outcome
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
    /// paths) through `attributes`, fails configured paths' `open` with a
    /// sharing violation, logs every `open` call.
    struct FakeReader {
        placeholders: Vec<PathBuf>,
        missing: Vec<PathBuf>,
        locked: Vec<PathBuf>,
        opened: StdMutex<Vec<PathBuf>>,
    }

    impl FakeReader {
        fn new() -> FakeReader {
            FakeReader {
                placeholders: Vec::new(),
                missing: Vec::new(),
                locked: Vec::new(),
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
            if self.locked.iter().any(|p| p == path) {
                // ERROR_SHARING_VIOLATION: `classify_open_err` files it
                // under OpenSkip::Locked (on Windows; an error elsewhere).
                return Err(std::io::Error::from_raw_os_error(32));
            }
            self.opened.lock().unwrap().push(path.to_path_buf());
            std::fs::File::open(path)
        }
    }

    fn noop_progress(_: DedupPhase, _: u64, _: u64) {}

    // ------------------------------------------------------------------
    // Task 2: candidate streaming load (SoA + name arena + recycle bits).
    // The old-pipeline tests above keep exercising `load_snapshot`.

    /// Upserts a fully specified row — size, `is_dir` and persisted hashes
    /// chosen by the test — under `dir_path`; returns
    /// `(files row id, containing dir id)`.
    #[allow(clippy::too_many_arguments)]
    fn upsert_row(
        store: &StdMutex<Store>,
        dir_path: &[&str],
        name: &str,
        size: u64,
        is_dir: bool,
        hash_first: Option<u64>,
        hash_full: Option<u64>,
    ) -> (FileId, FileId) {
        let mut rec = FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size,
            mtime: 1_700_000_000,
            ctime: 1_700_000_001,
            atime: 1_700_000_002,
            ext_id: 0,
            is_dir,
            hash_first,
            hash_full,
            py_full: String::new(),
            py_init: String::new(),
        };
        crate::engine::lock_unpoisoned(store)
            .upsert_file(dir_path, &mut rec)
            .unwrap();
        (rec.id, rec.parent)
    }

    /// A size>0 non-directory row and a persisted-hash row load; the
    /// size=0 row and the directory row do not. `None` hashes come back
    /// as [`NO_HASH`], `Some` pass through verbatim, names resolve from
    /// the arena, and slot order mirrors the id-ascending stream.
    #[test]
    fn candidates_load_filters_and_no_hash_sentinel() {
        let (_tmp, store) = test_store();
        let (a, _) = upsert_row(&store, &["fixture"], "a.bin", 5, false, None, None);
        let (b, _) = upsert_row(&store, &["fixture"], "b.bin", 5, false, Some(0x11), Some(0x22));
        let _zero = upsert_row(&store, &["fixture"], "zero.bin", 0, false, None, None);
        // A dir row with size>0: excluded by the is_dir filter alone.
        let _dir = upsert_row(&store, &["fixture"], "sub", 5, true, None, None);

        let Some((cands, _dirs)) = load_candidates(&store) else {
            panic!("fixture store must load");
        };
        assert_eq!(
            cands.ids.len(),
            2,
            "size=0 row and dir row are not candidates"
        );
        assert_eq!(cands.ids[0], a as u32, "stream is id-ascending");
        assert_eq!(cands.ids[1], b as u32);
        assert_eq!(cands.sizes, vec![5, 5]);
        assert_eq!(cands.mtimes, vec![1_700_000_000, 1_700_000_000]);
        assert_eq!(cands.parents[0], cands.parents[1], "same dir -> same slot");
        // None <-> NO_HASH round trip; persisted hashes pass through.
        assert_eq!(cands.hash_first[0], NO_HASH);
        assert_eq!(cands.hash_full[0], NO_HASH);
        assert_eq!(cands.hash_first[1], 0x11);
        assert_eq!(cands.hash_full[1], 0x22);
        assert_eq!(cands.name(0), "a.bin");
        assert_eq!(cands.name(1), "b.bin");
    }

    /// `index_path` rebuilds the `MAIN_SEPARATOR`-joined chain from the
    /// dir arena, root segment first — the contract the old
    /// `Snapshot::index_path` had.
    #[test]
    fn candidates_index_path_rebuilds_chain() {
        let (_tmp, store) = test_store();
        upsert_row(
            &store,
            &["fixture", "docs", "inner"],
            "memo.md",
            3,
            false,
            None,
            None,
        );

        let Some((cands, dirs)) = load_candidates(&store) else {
            panic!("fixture store must load");
        };
        assert_eq!(cands.ids.len(), 1);
        let path = cands.index_path(0, &dirs).unwrap();
        assert_eq!(
            path,
            format!("fixture{MAIN_SEPARATOR}docs{MAIN_SEPARATOR}inner{MAIN_SEPARATOR}memo.md")
        );
    }

    /// The `$Recycle.Bin` root carries the bit by name; `a` and `b`
    /// inherit it through the id-ascending propagation; an ordinary tree
    /// does not; and a candidate stored under the deepest dir points at a
    /// recycle dir.
    #[test]
    fn candidates_recycle_bits_propagate_to_nested_dirs() {
        let (_tmp, store) = test_store();
        let (twin, _b_dir) = upsert_row(
            &store,
            &["$Recycle.Bin", "a", "b"],
            "twin.bin",
            5,
            false,
            None,
            None,
        );
        let (lone_id, _fixture_dir) =
            upsert_row(&store, &["fixture"], "lone.bin", 5, false, None, None);

        let Some((cands, dirs)) = load_candidates(&store) else {
            panic!("fixture store must load");
        };
        let rows = crate::engine::lock_unpoisoned(&store).all_dirs().unwrap();
        let slot_of_name = |name: &str| {
            dirs.by_id[&rows.iter().find(|(_, _, n)| n == name).unwrap().0] as usize
        };
        assert!(
            dirs.recycle[slot_of_name("$Recycle.Bin")],
            "the root dir carries the bit by name"
        );
        assert!(dirs.recycle[slot_of_name("a")], "nested child inherits it");
        assert!(dirs.recycle[slot_of_name("b")], "so does the deepest level");
        assert!(!dirs.recycle[slot_of_name("fixture")], "an ordinary tree does not");
        assert_eq!(dirs.name(slot_of_name("b")), "b");
        // twin.bin's parent slot is b, and b is recycle; lone.bin's is not.
        assert_eq!(cands.ids, vec![twin as u32, lone_id as u32], "both size>0 files load");
        assert_eq!(cands.parents[0] as usize, slot_of_name("b"));
        assert!(dirs.recycle[cands.parents[0] as usize]);
        assert!(
            !dirs.recycle[cands.parents[1] as usize],
            "the ordinary tree's candidate is not recycle"
        );
    }

    /// A `files` row id beyond `u32::MAX` (only possible in a hand-edited
    /// database) is an explicit checked-conversion error, never a silent
    /// truncation — and the run-facing load degrades to `None` (the
    /// trivial-run semantics of `load_snapshot`).
    #[test]
    fn candidates_reject_file_id_beyond_u32() {
        let (_tmp, store) = test_store();
        let (_id, fixture_dir) = upsert_row(&store, &["fixture"], "twin_a.bin", 5, false, None, None);
        {
            let mut guard = crate::engine::lock_unpoisoned(&store);
            guard
                .conn_mut()
                .execute(
                    "INSERT INTO files(id, dir, name, size, mtime, ctime, atime, ext_id, is_dir,
                                       py_full, py_init, scan_gen)
                     VALUES (?1, ?2, 'huge.bin', 5, 0, 0, 0, 0, 0, '', '', 0)",
                    rusqlite::params![i64::MAX, fixture_dir],
                )
                .unwrap();
        }

        let err = load_candidates_checked(&store).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("9223372036854775807"),
            "the error must name the offending id, got: {text}"
        );
        assert!(
            load_candidates(&store).is_none(),
            "a store-side error means an empty run"
        );
    }

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

    // ------------------------------------------------------------------
    // Task 3: open semantics — deny-write sharing, Locked/Error
    // classification, mtime grace window.

    /// Sharing violations are the only Locked kind — and that code exists
    /// (with that meaning) only on Windows; every other open failure,
    /// OS-code or not, stays an error everywhere.
    #[test]
    fn open_semantics_classifies_open_errors() {
        #[cfg(windows)]
        assert_eq!(
            classify_open_err(&std::io::Error::from_raw_os_error(32)),
            OpenSkip::Locked,
            "ERROR_SHARING_VIOLATION is a Locked skip"
        );
        // Off Windows no failure can mean "locked by another handle".
        #[cfg(not(windows))]
        assert_eq!(
            classify_open_err(&std::io::Error::from_raw_os_error(32)),
            OpenSkip::Error
        );
        assert_eq!(
            classify_open_err(&std::io::Error::from_raw_os_error(5)),
            OpenSkip::Error,
            "access denied is an error, not a lock"
        );
        assert_eq!(
            classify_open_err(&std::io::Error::new(
                std::io::ErrorKind::Other,
                "not an os error"
            )),
            OpenSkip::Error
        );
    }

    /// The grace window boundary: a candidate modified 299s ago may still
    /// be mid-write (fresh, skipped); at exactly 300s the grace is over
    /// and the file is read. A future mtime (clock skew) is inside the
    /// window — the arithmetic is a plain difference, no abs() tricks.
    #[test]
    fn open_semantics_fresh_window_boundary() {
        let now = 1_800_000_000;
        assert!(is_fresh(now - 299, now));
        assert!(!is_fresh(now - 300, now));
        assert!(!is_fresh(now - 3_600, now), "long-settled files are read");
        assert!(is_fresh(now + 5, now), "clock skew stays in the window");
    }

    /// A real file held with NO sharing flags (the [`crate::engine`] tests'
    /// precedent) must fail our deny-write open as a Locked skip — the
    /// open-path helper returns no file (nothing is ever read), the skip
    /// lands in `locked`, and the end-to-end run hashes only the unlocked
    /// sibling. Releasing the handle makes the file openable again.
    #[cfg(windows)]
    #[test]
    fn open_semantics_real_exclusive_lock_is_locked_skip() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
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

        // The open-path helper: no file comes back (so no content can be
        // read), the failure is a Locked skip, not an error.
        let reader = RealFileReader;
        let mut outcome = DedupOutcome::default();
        assert!(open_or_skip(&reader, &root.join("locked.txt"), &mut outcome).is_none());
        assert_eq!(outcome.locked, 1);
        assert_eq!(outcome.errors, 0);

        // End-to-end through the pipeline (whose deny-write open hits the
        // same exclusive holder): the locked member never enters a group,
        // never reaches a hash, and is not counted as an error.
        let (_tstore, store) = test_store();
        upsert_fixture_row(&store, "locked.txt");
        upsert_fixture_row(&store, "free.txt");
        let roots = vec![root.to_string_lossy().into_owned()];
        let outcome = run_pipeline(&store, &roots, &reader, &noop_progress);
        assert_eq!(outcome.locked, 1, "the sharing violation is a Locked skip");
        assert_eq!(outcome.errors, 0, "not an error");
        assert_eq!(outcome.hashed_first, 1, "only free.txt was read");
        assert!(outcome.groups.is_empty(), "one hash never pairs");

        // Release the handle: the file opens like any other.
        drop(held);
        let mut after = DedupOutcome::default();
        assert!(open_or_skip(&reader, &root.join("locked.txt"), &mut after).is_some());
        assert_eq!(after.locked, 0);
    }

    // ------------------------------------------------------------------
    // Task 4: gate① (size) + gate② (first-4KiB probe: directory-order
    // chunking, worker pool, terminal small files, hash reuse).

    /// The gate tests' clock: every fixture measures against this `now`.
    const NOW: i64 = 1_800_000_000;

    /// `upsert_row` with a chosen mtime — the gate tests' clock control.
    #[allow(clippy::too_many_arguments)]
    fn upsert_row_at(
        store: &StdMutex<Store>,
        dir_path: &[&str],
        name: &str,
        size: u64,
        mtime: i64,
        hash_first: Option<u64>,
        hash_full: Option<u64>,
    ) -> (FileId, FileId) {
        let mut rec = FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size,
            mtime,
            ctime: mtime,
            atime: mtime,
            ext_id: 0,
            is_dir: false,
            hash_first,
            hash_full,
            py_full: String::new(),
            py_init: String::new(),
        };
        crate::engine::lock_unpoisoned(store)
            .upsert_file(dir_path, &mut rec)
            .unwrap();
        (rec.id, rec.parent)
    }

    /// Loads the SoA snapshot for a gate fixture (fixtures always load).
    fn load_cands(store: &StdMutex<Store>) -> (Candidates, Dirs) {
        load_candidates(store).expect("fixture store must load")
    }

    /// Every candidate slot in load order — the gate fixtures' `cand`.
    fn all_slots(c: &Candidates) -> Vec<u32> {
        (0..c.ids.len() as u32).collect()
    }

    /// The disk roots a gate fixture resolves against.
    fn gate_roots(root: &Path) -> Vec<String> {
        vec![root.to_string_lossy().into_owned()]
    }

    /// The position of a files-row id in the gate's directory-ordered
    /// `cand` — `head`/`full` are parallel to that order.
    fn pos_of(out: &HeadOutcome, cands: &Candidates, id: FileId) -> usize {
        out.cand
            .iter()
            .position(|&s| cands.ids[s as usize] == id as u32)
            .unwrap()
    }

    /// A progress collector for gate tests.
    fn progress_log() -> std::rc::Rc<StdMutex<Vec<(u64, u64)>>> {
        std::rc::Rc::new(StdMutex::new(Vec::new()))
    }

    /// gate① keeps every size carried by >= 2 candidates (both members)
    /// and kills the singleton with its byte tally.
    #[test]
    fn gate_size_keeps_shared_sizes_and_counts_kills() {
        let (_tmp, store) = test_store();
        let (a, _) = upsert_row(&store, &["fixture"], "a.bin", 5, false, None, None);
        let (b, _) = upsert_row(&store, &["fixture"], "b.bin", 5, false, None, None);
        let _lone = upsert_row(&store, &["fixture"], "lone.bin", 7, false, None, None);
        let (cands, _dirs) = load_cands(&store);

        let mut counters = OutcomeCounters::default();
        let kept = gate_size(&cands, &mut counters);

        assert_eq!(kept.len(), 2, "the shared size keeps both members");
        let mut kept_ids: Vec<u32> = kept.iter().map(|&i| cands.ids[i as usize]).collect();
        kept_ids.sort_unstable();
        assert_eq!(kept_ids, vec![a as u32, b as u32]);
        assert_eq!(counters.size_gate.files, 1, "the singleton is killed");
        assert_eq!(counters.size_gate.bytes, 7, "the singleton's bytes are tallied");
        assert_eq!(counters.head_gate.files, 0, "gate① touched only its own tally");
    }

    /// A persisted hash_first is reused WITHOUT opening the file and
    /// WITHOUT the mtime grace window (reuse exemption); the sibling
    /// without one is opened and hashed; progress totals count the
    /// reused candidate too.
    #[test]
    fn gate_head_reuses_persisted_hash_without_open_or_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        let big = vec![b'x'; 8192];
        fs::write(root.join("b.bin"), &big).unwrap();

        let (_tstore, store) = test_store();
        // a.bin: persisted first hash AND an mtime inside the grace
        // window — reuse must ignore both.
        let (a, _) = upsert_row_at(&store, &["fixture"], "a.bin", 4, NOW - 10, Some(0xABCD), None);
        let (b, _) = upsert_row_at(&store, &["fixture"], "b.bin", 8192, NOW - 10_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let log = progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |done: u64, total: u64| {
            sink.lock().unwrap().push((done, total));
        };
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &progress,
            &store,
        );

        let opened = reader.opened.lock().unwrap().clone();
        assert_eq!(opened, vec![root.join("b.bin")], "a.bin is never opened");
        assert_eq!(out.counters.fresh, 0, "the grace window does not apply to reuse");
        assert_eq!(out.hashed_first, 1, "only b.bin was computed");
        let pa = pos_of(&out, &cands, a);
        let pb = pos_of(&out, &cands, b);
        assert_eq!(out.head[pa], 0xABCD, "the persisted hash is reused verbatim");
        let b_hash = xxh64(&big[..4096], 0);
        assert_eq!(out.head[pb], b_hash);
        // a.bin is <= 4KiB: its persisted first hash IS its whole-file
        // hash — terminal without any I/O.
        assert_eq!(out.full[pa], 0xABCD);
        assert_eq!(out.full[pb], NO_HASH, "a >4KiB file is not terminal");
        // pending carries only the computed update — nothing is
        // re-persisted for a reused hash.
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].id, b);
        assert_eq!(out.pending[0].hash_first, Some(b_hash));
        assert_eq!(out.pending[0].hash_full, None);
        assert_eq!(out.counters.head_gate.files, 0, "nothing was skipped");
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            calls.last(),
            Some(&(2, 2)),
            "the total counts every candidate, reused included"
        );
    }

    /// No persisted hash + mtime inside the grace window: counted
    /// `fresh`, never opened, no hash obtained.
    #[test]
    fn gate_head_counts_fresh_without_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let (f, _) = upsert_row_at(&store, &["fixture"], "f.bin", 5, NOW - 100, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.counters.fresh, 1);
        assert!(reader.opened.lock().unwrap().is_empty(), "never opened");
        let pf = pos_of(&out, &cands, f);
        assert_eq!(out.head[pf], NO_HASH);
        assert_eq!(out.full[pf], NO_HASH);
        assert!(out.pending.is_empty());
        assert_eq!(out.hashed_first, 0);
        assert_eq!(out.counters.head_gate.files, 1, "a fresh skip leaves the grouping population");
        assert_eq!(out.counters.head_gate.bytes, 5);
    }

    /// A cloud placeholder is counted and never opened (no hydration
    /// download), exactly like the old pipeline's pre-open check.
    #[test]
    fn gate_head_counts_placeholders_before_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("p.bin"), b"would hydrate").unwrap();

        let (_tstore, store) = test_store();
        let (p, _) = upsert_row_at(&store, &["fixture"], "p.bin", 13, NOW - 10_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let mut reader = FakeReader::new();
        reader.placeholders.push(root.join("p.bin"));
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.counters.placeholders, 1);
        assert!(reader.opened.lock().unwrap().is_empty(), "never opened");
        assert_eq!(out.head[pos_of(&out, &cands, p)], NO_HASH);
        assert_eq!(out.counters.head_gate.files, 1);
    }

    /// A recycle-bin candidate is not a candidate at all: the up-front
    /// check runs BEFORE hash reuse, so even a persisted hash cannot
    /// save it, and nothing is ever opened.
    #[test]
    fn gate_head_counts_recycle_candidates_before_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("kept.bin"), b"kept bytes").unwrap();

        let (_tstore, store) = test_store();
        let (r, _) = upsert_row_at(
            &store,
            &["$Recycle.Bin"],
            "recycled.bin",
            5,
            NOW - 10_000,
            Some(0xBEEF),
            None,
        );
        let (k, _) = upsert_row_at(&store, &["fixture"], "kept.bin", 10, NOW - 10_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.counters.recycle, 1);
        assert_eq!(
            *reader.opened.lock().unwrap(),
            vec![root.join("kept.bin")],
            "only the ordinary candidate is opened"
        );
        let pr = pos_of(&out, &cands, r);
        assert_eq!(out.head[pr], NO_HASH, "the persisted hash does not save it");
        assert_eq!(out.full[pr], NO_HASH);
        assert_eq!(out.head[pos_of(&out, &cands, k)], xxh64(b"kept bytes", 0));
        assert_eq!(out.counters.head_gate.files, 1, "only the recycle kill");
    }

    /// Candidates whose path cannot be resolved (no configured root
    /// carries the segment) are errors; nothing is opened.
    #[test]
    fn gate_head_counts_unresolvable_paths_as_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();

        let (_tstore, store) = test_store();
        let (a, _) = upsert_row_at(&store, &["fixture"], "a.bin", 5, NOW - 10_000, None, None);
        let (b, _) = upsert_row_at(&store, &["fixture"], "b.bin", 5, NOW - 10_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&elsewhere);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.counters.errors, 2);
        assert!(reader.opened.lock().unwrap().is_empty());
        assert_eq!(out.head[pos_of(&out, &cands, a)], NO_HASH);
        assert_eq!(out.head[pos_of(&out, &cands, b)], NO_HASH);
        assert_eq!(out.counters.head_gate.files, 2);
        assert_eq!(out.counters.head_gate.bytes, 10);
    }

    /// A sharing-violation open failure is a Locked skip (not an error):
    /// nothing is read, the next run retries.
    #[cfg(windows)]
    #[test]
    fn gate_head_counts_locked_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("l.bin"), b"locked bytes").unwrap();

        let (_tstore, store) = test_store();
        let (l, _) = upsert_row_at(&store, &["fixture"], "l.bin", 12, NOW - 10_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let mut reader = FakeReader::new();
        reader.locked.push(root.join("l.bin"));
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.counters.locked, 1);
        assert_eq!(out.counters.errors, 0, "a lock is not an error");
        assert_eq!(out.head[pos_of(&out, &cands, l)], NO_HASH);
        assert_eq!(out.counters.head_gate.files, 1);
    }

    /// size <= 4KiB files are TERMINAL: one probe read yields both
    /// hashes — each pending HashUpdate carries hash_first == hash_full
    /// (double Some) and the side arrays mark head == full.
    #[test]
    fn gate_head_terminal_small_files_persist_double_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.bin"), b"0123456789abcdef").unwrap();
        fs::write(root.join("b.bin"), b"fedcba9876543210").unwrap();

        let (_tstore, store) = test_store();
        let (a, _) = upsert_row_at(&store, &["fixture"], "a.bin", 16, NOW - 1_000, None, None);
        let (b, _) = upsert_row_at(&store, &["fixture"], "b.bin", 16, NOW - 1_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(reader.opened.lock().unwrap().len(), 2);
        assert_eq!(out.hashed_first, 2);
        assert_eq!(out.pending.len(), 2);
        // Directory order: same dir, id-ascending (a was upserted first).
        assert_eq!(out.pending[0].id, a);
        assert_eq!(out.pending[1].id, b);
        let a_hash = xxh64(b"0123456789abcdef", 0);
        let b_hash = xxh64(b"fedcba9876543210", 0);
        assert_eq!(out.pending[0].hash_first, Some(a_hash));
        assert_eq!(out.pending[0].hash_full, Some(a_hash), "terminal: one read yields both");
        assert_eq!(out.pending[1].hash_first, Some(b_hash));
        assert_eq!(out.pending[1].hash_full, Some(b_hash));
        // The side arrays agree with the updates.
        let pa = pos_of(&out, &cands, a);
        let pb = pos_of(&out, &cands, b);
        assert_eq!(out.head[pa], a_hash);
        assert_eq!(out.full[pa], a_hash, "terminal files are marked in `full`");
        assert_eq!(out.head[pb], b_hash);
        assert_eq!(out.full[pb], b_hash);
        assert_eq!(out.counters.head_gate.files, 0);
    }

    /// A >4KiB file is not terminal: head is the first-4KiB hash, the
    /// side array leaves `full` unknown and the pending update carries
    /// hash_full = None (the full gate reads it later).
    #[test]
    fn gate_head_large_files_leave_full_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        let big = vec![b'q'; 8192];
        fs::write(root.join("big.bin"), &big).unwrap();

        let (_tstore, store) = test_store();
        let (g, _) = upsert_row_at(&store, &["fixture"], "big.bin", 8192, NOW - 1_000, None, None);
        let (cands, dirs) = load_cands(&store);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        let hash = xxh64(&big[..4096], 0);
        assert_eq!(out.head[pos_of(&out, &cands, g)], hash);
        assert_eq!(out.full[pos_of(&out, &cands, g)], NO_HASH);
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].hash_first, Some(hash));
        assert_eq!(out.pending[0].hash_full, None);
    }

    /// The gate's output is directory-ordered: `out.cand` is sorted by
    /// (parent dir slot, files-row id), is a permutation of the input,
    /// and head/full stay parallel to it.
    #[test]
    fn gate_head_orders_candidates_by_parent_then_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("d1")).unwrap();
        fs::create_dir_all(root.join("d2")).unwrap();
        fs::write(root.join("d1").join("a.bin"), b"aaa").unwrap();
        fs::write(root.join("d1").join("c.bin"), b"ccc").unwrap();
        fs::write(root.join("d2").join("b.bin"), b"bbb").unwrap();

        let (_tstore, store) = test_store();
        // Insertion order must not follow directory order for the
        // contract to matter: d2's file first, then d1's.
        upsert_row_at(&store, &["fixture", "d2"], "b.bin", 3, NOW - 1_000, None, None);
        upsert_row_at(&store, &["fixture", "d1"], "a.bin", 3, NOW - 1_000, None, None);
        upsert_row_at(&store, &["fixture", "d1"], "c.bin", 3, NOW - 1_000, None, None);
        let (cands, dirs) = load_cands(&store);
        let input = all_slots(&cands);

        let reader = FakeReader::new();
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let out = gate_head(
            &cands,
            &dirs,
            &input,
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &|_, _| {},
            &store,
        );

        assert_eq!(out.head.len(), out.cand.len());
        assert_eq!(out.full.len(), out.cand.len());
        let key = |&slot: &u32| (cands.parents[slot as usize], cands.ids[slot as usize]);
        let keys: Vec<_> = out.cand.iter().map(key).collect();
        assert!(
            keys.windows(2).all(|w| w[0] <= w[1]),
            "out.cand is (parent slot, id)-ascending: {keys:?}"
        );
        let mut sorted_out = out.cand.clone();
        sorted_out.sort_unstable();
        let mut sorted_in = input.clone();
        sorted_in.sort_unstable();
        assert_eq!(sorted_out, sorted_in, "a permutation of the input slots");
    }

    /// Multi-chunk runs scatter correctly: workers deal CHUNK-LOCAL
    /// positions, `head`/`full` are GLOBAL directory-order arrays — with
    /// distinct contents every candidate's hash must land at ITS OWN
    /// position, across the boundary. Chunk size 2 over 5 candidates =
    /// chunks [0,1] [2,3] [4]; before the base-offset fix, chunk 2's
    /// first candidate clobbered chunk 1's slot 0.
    #[test]
    fn gate_head_multi_chunk_scatters_every_head_to_its_own_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("d1")).unwrap();
        fs::create_dir_all(root.join("d2")).unwrap();

        let (_tstore, store) = test_store();
        // Distinct contents -> distinct hashes, so a swapped slot cannot
        // pass unnoticed. Every file <= 4KiB: terminal, full == head.
        let contents: Vec<Vec<u8>> = (0..5)
            .map(|i| format!("content number {i:02} padding").into_bytes())
            .collect();
        for (i, content) in contents.iter().enumerate() {
            let dir = if i < 3 { "d1" } else { "d2" };
            fs::write(root.join(dir).join(format!("f{i}.bin")), content).unwrap();
            upsert_row_at(
                &store,
                &["fixture", dir],
                &format!("f{i}.bin"),
                content.len() as u64,
                NOW - 1_000,
                None,
                None,
            );
        }
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        for (workers, chunk_size) in [(1, 2), (2, 2), (1, 3), (2, 4)] {
            let reader = FakeReader::new();
            let cancel = AtomicBool::new(false);
            let out = gate_head_chunked(
                &cands,
                &dirs,
                &all_slots(&cands),
                &roots,
                &reader,
                workers,
                &cancel,
                NOW,
                &|_, _| {},
                &store,
                chunk_size,
            );

            assert_eq!(out.cand.len(), 5, "every candidate present");
            for (i, content) in contents.iter().enumerate() {
                let id = cands.ids[i];
                let expected = xxh64(content, 0);
                let pos = pos_of(&out, &cands, id as FileId);
                assert_eq!(
                    out.head[pos], expected,
                    "chunk_size={chunk_size} workers={workers}: f{i}'s head must sit at its own slot {pos}"
                );
                assert_eq!(
                    out.full[pos], expected,
                    "chunk_size={chunk_size} workers={workers}: f{i} is terminal, full == head at slot {pos}"
                );
            }
            assert!(
                out.head.iter().all(|&h| h != NO_HASH),
                "chunk_size={chunk_size} workers={workers}: no slot may stay NO_HASH"
            );
            assert!(
                out.full.iter().all(|&h| h != NO_HASH),
                "chunk_size={chunk_size} workers={workers}: all candidates terminal here"
            );
            // Persisted nothing is lost either way: 5 computed updates.
            assert_eq!(out.pending.len(), 5);
            assert_eq!(out.hashed_first, 5);
        }
    }

    /// workers = 2 and workers = 1 agree: same head/full arrays, same
    /// counters, same pending batch (order aside), same open set, same
    /// hashes.
    #[test]
    fn gate_head_worker_counts_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("d1")).unwrap();
        fs::create_dir_all(root.join("d2")).unwrap();
        let twin = b"0123456789abcdef";
        fs::write(root.join("d1").join("t.bin"), twin).unwrap();
        fs::write(root.join("d2").join("t.bin"), twin).unwrap();
        let big = vec![b'q'; 8192];
        fs::write(root.join("big.bin"), &big).unwrap();

        let (_tstore, store) = test_store();
        upsert_row_at(&store, &["fixture", "d1"], "t.bin", 16, NOW - 1_000, None, None);
        upsert_row_at(&store, &["fixture", "d2"], "t.bin", 16, NOW - 1_000, None, None);
        upsert_row_at(&store, &["fixture"], "big.bin", 8192, NOW - 1_000, None, None);
        // A fresh file: skipped identically under both worker counts.
        upsert_row_at(&store, &["fixture"], "hot.bin", 7, NOW - 10, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let run = |workers: usize| {
            let reader = FakeReader::new();
            let cancel = AtomicBool::new(false);
            let out = gate_head(
                &cands,
                &dirs,
                &all_slots(&cands),
                &roots,
                &reader,
                workers,
                &cancel,
                NOW,
                &|_, _| {},
                &store,
            );
            let mut opened = reader.opened.lock().unwrap().clone();
            opened.sort();
            (out, opened)
        };

        let (one, opened1) = run(1);
        let (two, opened2) = run(2);

        assert_eq!(one.head, two.head, "same hashes in the same order");
        assert_eq!(one.full, two.full);
        assert_eq!(one.cand, two.cand);
        assert_eq!(one.counters, two.counters);
        assert_eq!(one.hashed_first, two.hashed_first);
        assert_eq!(one.cancelled, two.cancelled);
        assert_eq!(opened1, opened2, "the same files were opened");
        let mut p1: Vec<_> = one
            .pending
            .iter()
            .map(|u| (u.id, u.hash_first, u.hash_full))
            .collect();
        p1.sort_by_key(|t| t.0);
        let mut p2: Vec<_> = two
            .pending
            .iter()
            .map(|u| (u.id, u.hash_first, u.hash_full))
            .collect();
        p2.sort_by_key(|t| t.0);
        assert_eq!(p1, p2, "the same computed updates");
        // And the fixture itself behaved: 3 computed, 1 fresh skip.
        assert_eq!(one.hashed_first, 3);
        assert_eq!(one.counters.fresh, 1);
        assert_eq!(one.counters.head_gate.files, 1);
    }

    /// Cancel stops gate② between files: the file in flight finishes,
    /// later candidates are refused at the boundary, the in-flight batch
    /// is persisted (flush first) and `cancelled` is reported.
    #[test]
    fn gate_head_cancel_stops_between_files_and_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.bin"), b"0123456789abcdef").unwrap();
        fs::write(root.join("b.bin"), b"fedcba9876543210").unwrap();

        let (_tstore, store) = test_store();
        let (a, _) = upsert_row_at(&store, &["fixture"], "a.bin", 16, NOW - 1_000, None, None);
        let (b, _) = upsert_row_at(&store, &["fixture"], "b.bin", 16, NOW - 1_000, None, None);
        let (cands, dirs) = load_cands(&store);

        /// Sets `cancel` as soon as an open succeeds — the run stops at
        /// the next file boundary.
        struct CancelAfterOpen<'a> {
            inner: FakeReader,
            cancel: &'a AtomicBool,
        }
        impl FileReader for CancelAfterOpen<'_> {
            fn attributes(&self, path: &Path) -> Option<u32> {
                self.inner.attributes(path)
            }
            fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
                let file = self.inner.open(path)?;
                self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(file)
            }
        }

        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);
        let reader = CancelAfterOpen {
            inner: FakeReader::new(),
            cancel: &cancel,
        };
        let log = progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |done: u64, total: u64| {
            sink.lock().unwrap().push((done, total));
        };
        let out = gate_head(
            &cands,
            &dirs,
            &all_slots(&cands),
            &roots,
            &reader,
            1,
            &cancel,
            NOW,
            &progress,
            &store,
        );

        let opened = reader.inner.opened.lock().unwrap().clone();
        assert_eq!(opened, vec![root.join("a.bin")], "only the first file in dir order");
        assert!(out.cancelled, "the cancel is reported");
        let pa = pos_of(&out, &cands, a);
        let pb = pos_of(&out, &cands, b);
        let a_hash = xxh64(b"0123456789abcdef", 0);
        assert_eq!(out.head[pa], a_hash, "the in-flight file finished");
        assert_eq!(out.full[pa], a_hash, "it was small: terminal");
        assert_eq!(out.head[pb], NO_HASH, "the next file was refused");
        assert!(out.pending.is_empty(), "the cancel flushed the batch first");
        let calls = log.lock().unwrap().clone();
        assert_eq!(calls.last(), Some(&(1, 2)), "done stops at the processed count");
        // And the flush really landed: a.bin's row carries both hashes.
        let rows = crate::engine::lock_unpoisoned(&store).all_files().unwrap();
        let row_a = rows.iter().find(|r| r.id == a).unwrap();
        assert_eq!(row_a.hash_first, Some(a_hash));
        assert_eq!(row_a.hash_full, Some(a_hash));
        let row_b = rows.iter().find(|r| r.id == b).unwrap();
        assert_eq!(row_b.hash_first, None, "the cancelled candidate persisted nothing");
    }

    /// Ruling 7: the loader's recycle-name compare is case-insensitive —
    /// Windows volumes may create the dir as `$RECYCLE.BIN`. The `$`
    /// prefix stays part of the rule.
    #[test]
    fn candidates_recycle_bin_name_match_is_case_insensitive() {
        let (_tmp, store) = test_store();
        upsert_row(&store, &["$recycle.bin", "x"], "t.bin", 5, false, None, None);
        upsert_row(&store, &["$RECYCLE.BIN", "y"], "t.bin", 5, false, None, None);
        upsert_row(&store, &["Recycle.Bin"], "t.bin", 5, false, None, None);
        let (cands, dirs) = load_cands(&store);

        let rows = crate::engine::lock_unpoisoned(&store).all_dirs().unwrap();
        let slot_of_name = |name: &str| {
            dirs.by_id[&rows.iter().find(|(_, _, n)| n == name).unwrap().0] as usize
        };
        assert!(dirs.recycle[slot_of_name("$recycle.bin")], "lowercase matches");
        assert!(dirs.recycle[slot_of_name("$RECYCLE.BIN")], "uppercase matches");
        assert!(
            !dirs.recycle[slot_of_name("Recycle.Bin")],
            "the $ prefix is part of the rule"
        );
        // The candidates under the differently-cased recycle roots sit
        // in recycle dirs — gate② will exclude them; the $-less tree's
        // candidate does not.
        for slot in 0..2 {
            assert!(dirs.recycle[cands.parents[slot] as usize]);
        }
        assert!(!dirs.recycle[cands.parents[2] as usize]);
    }

    // ------------------------------------------------------------------
    // Task 5: gate ③ — the sampling layer. The chain under test is
    // gate② → the (size, head) regroup → gate_sample; the regroup
    // helper below is exactly what Task 6's run_gates must do between
    // the gates.

    /// Terminal-path fixture size: 4KB < size ≤ 64KB.
    const SAMPLE_SMALL: usize = 16 * 1024;
    /// Sampled-path fixture size: > 64KB (n = 2, offsets [8KB, 16KB]).
    const SAMPLE_BIG: usize = 80 * 1024;

    /// Writes `content` under `root/name`, upserts the row with the
    /// chosen persisted hashes and an mtime outside the grace window,
    /// and returns the files-row id — the gate③ fixtures' builder.
    fn sample_fixture(
        store: &StdMutex<Store>,
        root: &Path,
        name: &str,
        content: &[u8],
        hash_first: Option<u64>,
        hash_full: Option<u64>,
    ) -> FileId {
        fs::write(root.join(name), content).unwrap();
        let (id, _) = upsert_row_at(
            store,
            &["fixture"],
            name,
            content.len() as u64,
            NOW - 10_000,
            hash_first,
            hash_full,
        );
        id
    }

    /// The candidate slot of a files-row id (the SoA slot gate③'s
    /// `full_todo` entries address).
    fn slot_of(cands: &Candidates, id: FileId) -> u32 {
        cands.ids.iter().position(|&s| s == id as u32).unwrap() as u32
    }

    /// The position of a files-row id in gate③'s input / `full_todo`.
    fn sample_pos(input: &[u32], cands: &Candidates, id: FileId) -> Option<usize> {
        input
            .iter()
            .position(|&s| cands.ids[s as usize] == id as u32)
    }

    // `sample_input` — the Task-6 regroup between the gates — lives in
    // the production code now (run_gates owns the assembly); the gate③
    // tests call it from there.

    /// Runs gate② then the regroup then gate③ over one reader,
    /// workers=1, no cancel, silent progress — the tests' default chain.
    fn run_sample_chain(
        cands: &Candidates,
        dirs: &Dirs,
        roots: &[String],
        reader: &dyn FileReader,
        store: &StdMutex<Store>,
    ) -> (HeadOutcome, SampleOutcome, Vec<u32>) {
        let cancel = AtomicBool::new(false);
        let head = gate_head(
            cands, dirs, &all_slots(cands), roots, reader, 1, &cancel, NOW, &|_, _| {}, store,
        );
        let input = sample_input(cands, &head);
        let out = gate_sample(
            cands, dirs, &input, &head, roots, reader, 1, &cancel, NOW, &|_, _| {}, store,
        );
        (head, out, input)
    }

    /// Ruling 2's window-count table (the brief's `256MB+1 → 3` was
    /// wrong) plus the offset formula: `i × (size − 64KB) / n`, with i
    /// = n the tail window exactly at `size − 64KB`.
    #[test]
    fn sample_window_count_table_and_tail_window() {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;
        const GB: u64 = 1024 * MB;
        assert_eq!(sample_window_offsets(64 * KB + 1).len(), 2);
        assert_eq!(
            sample_window_offsets(256 * MB).len(),
            2,
            "the boundary ceil(=1) lifts to the minimum 2"
        );
        assert_eq!(
            sample_window_offsets(256 * MB + 1).len(),
            2,
            "Ruling 2: ceil((256MB+1)/256MB) is 2, not 3"
        );
        assert_eq!(sample_window_offsets(512 * MB + 1).len(), 3);
        assert_eq!(sample_window_offsets(13 * GB + 500 * MB).len(), 16);

        // Offsets follow i × (size − 64KB) / n; the last is the tail.
        for size in [80 * KB, 512 * MB + 1, 13 * GB + 500 * MB] {
            let offs = sample_window_offsets(size);
            let n = offs.len() as u64;
            let span = size - 64 * KB;
            for (i, &off) in offs.iter().enumerate() {
                assert_eq!(off, (i as u64 + 1) * span / n, "size {size}, offset {i}");
            }
            assert_eq!(*offs.last().unwrap(), span, "i = n is the tail window");
        }
        // The fixture size's concrete window set (the sampled-path
        // tests rely on these exact offsets).
        assert_eq!(
            sample_window_offsets(SAMPLE_BIG as u64),
            vec![8 * 1024, 16 * 1024]
        );
    }

    /// The sample key: window hashes concatenated little-endian in
    /// OFFSET order, then xxh64 (seed 0) — order-sensitive by
    /// construction, which is what makes same-size keys comparable.
    #[test]
    fn sample_key_is_the_ordered_cascade() {
        assert_ne!(
            sample_key(&[1, 2]),
            sample_key(&[2, 1]),
            "offset order is load-bearing"
        );
        let mut cascade = Vec::new();
        cascade.extend_from_slice(&1u64.to_le_bytes());
        cascade.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(sample_key(&[1, 2]), xxh64(&cascade, 0));
        assert_eq!(sample_key(&[]), xxh64(b"", 0), "degenerates gracefully");
    }

    /// `4KB < size ≤ 64KB`: the whole read happens at gate③ (the
    /// second open per file), the full hash lands terminal in `pending`
    /// with `hash_first` untouched, and flushing persists both columns.
    #[test]
    fn sample_small_terminal_whole_read_persists_full_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b's'; SAMPLE_SMALL];
        let a = sample_fixture(&store, &root, "a.bin", &content, None, None);
        let b = sample_fixture(&store, &root, "b.bin", &content, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let reader = FakeReader::new();
        let (head, out, input) = run_sample_chain(&cands, &dirs, &roots, &reader, &store);

        assert_eq!(input.len(), 2, "one (size, head) run of two");
        // gate② probed both; gate③ whole-read both — one open each.
        assert_eq!(reader.opened.lock().unwrap().len(), 4);
        let full = xxh64(&content, 0);
        assert_eq!(out.hashed_full_small, 2);
        assert_eq!(out.pending.len(), 2);
        for (i, update) in out.pending.iter().enumerate() {
            assert_eq!(update.id, if i == 0 { a } else { b }, "input order");
            assert_eq!(update.hash_first, None, "gate② already carried hash_first");
            assert_eq!(update.hash_full, Some(full));
        }
        // Terminal: both reach gate④ via full_todo, in input order —
        // with their fulls riding the flush-proof side array (pending
        // alone cannot be trusted: boundary flushes drain it).
        assert_eq!(out.full_todo.len(), 2);
        assert_eq!(sample_pos(&out.full_todo, &cands, a), Some(0));
        assert_eq!(sample_pos(&out.full_todo, &cands, b), Some(1));
        assert_eq!(out.small_full, vec![full, full], "parallel to full_todo");
        // Nothing killed, nothing skipped, gate②'s tally untouched.
        assert_eq!(out.counters.sample_gate.files, 0);
        assert_eq!(out.counters.errors + out.counters.locked + out.counters.fresh, 0);
        assert_eq!(out.counters.head_gate.files, 0);
        // 落库终局: flushing both gates' residues persists both columns.
        let (mut head, mut out) = (head, out);
        flush_persist(&store, &mut head.pending);
        flush_persist(&store, &mut out.pending);
        let rows = crate::engine::lock_unpoisoned(&store).all_files().unwrap();
        for id in [a, b] {
            let row = rows.iter().find(|r| r.id == id).unwrap();
            assert_eq!(row.hash_first, Some(xxh64(&content[..4096], 0)));
            assert_eq!(row.hash_full, Some(full));
        }
    }

    /// Certified-partner short-circuit (Ruling 3): a run holding a
    /// certified member sends its NEW member straight to full_todo —
    /// zero gate③ opens, zero sampling, zero progress participants.
    #[test]
    fn sample_certified_partner_short_circuits_without_sampling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'c'; SAMPLE_BIG]; // > 64KB: it WOULD sample
        let head_hash = xxh64(&content[..4096], 0);
        // A: the certified member — persisted first AND full hash.
        let a = sample_fixture(&store, &root, "a.bin", &content, Some(head_hash), Some(0xC0FFEE));
        // F: the new twin — no persisted hashes at all.
        let f = sample_fixture(&store, &root, "f.bin", &content, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let reader = FakeReader::new();
        let cancel = AtomicBool::new(false);
        let head = gate_head(
            &cands, &dirs, &all_slots(&cands), &roots, &reader, 1, &cancel, NOW, &|_, _| {},
            &store,
        );
        let input = sample_input(&cands, &head);
        let log = progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |done: u64, total: u64| {
            sink.lock().unwrap().push((done, total));
        };
        let out = gate_sample(
            &cands, &dirs, &input, &head, &roots, &reader, 1, &cancel, NOW, &progress, &store,
        );

        // gate② probed F only (A reused); gate③ opened NOTHING —
        // neither the certified member nor the short-circuited twin.
        assert_eq!(*reader.opened.lock().unwrap(), vec![root.join("f.bin")]);
        assert_eq!(out.full_todo.len(), 2, "both reach gate④");
        assert_eq!(sample_pos(&out.full_todo, &cands, a), Some(0));
        assert_eq!(sample_pos(&out.full_todo, &cands, f), Some(1));
        assert_eq!(
            out.small_full,
            vec![NO_HASH, NO_HASH],
            "neither member's full is gate③'s product"
        );
        assert!(out.pending.is_empty(), "gate③ computed nothing");
        assert_eq!(out.hashed_full_small, 0);
        assert_eq!(out.counters.sample_gate.files, 0);
        // No sampling participants: the short-circuit is free.
        assert_eq!(log.lock().unwrap().last(), Some(&(0, 0)));
    }

    /// The full open matrix applies at gate③ (Ruling 4): a candidate
    /// with a persisted hash_first (gate② reused it WITHOUT the grace
    /// check) hits the mtime grace HERE — skipped fresh, never opened,
    /// while its run partner (old mtime) goes terminal.
    #[test]
    fn sample_first_open_respects_mtime_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'g'; SAMPLE_SMALL];
        let head_hash = xxh64(&content[..4096], 0);
        fs::write(root.join("g.bin"), &content).unwrap();
        let (_g, _) = upsert_row_at(
            &store,
            &["fixture"],
            "g.bin",
            SAMPLE_SMALL as u64,
            NOW - 10, // inside the grace window
            Some(head_hash),
            None,
        );
        let p = sample_fixture(&store, &root, "p.bin", &content, Some(head_hash), None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let reader = FakeReader::new();
        let cancel = AtomicBool::new(false);
        let head = gate_head(
            &cands, &dirs, &all_slots(&cands), &roots, &reader, 1, &cancel, NOW, &|_, _| {},
            &store,
        );
        assert!(
            head.pending.is_empty(),
            "both candidates were reused at gate②: no gate② updates"
        );
        let input = sample_input(&cands, &head);
        assert_eq!(input.len(), 2, "same (size, persisted head) run");
        let log = progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |done: u64, total: u64| {
            sink.lock().unwrap().push((done, total));
        };
        let out = gate_sample(
            &cands, &dirs, &input, &head, &roots, &reader, 1, &cancel, NOW, &progress, &store,
        );

        // gate③ opened only the old partner; the fresh one was refused
        // before its open.
        assert_eq!(*reader.opened.lock().unwrap(), vec![root.join("p.bin")]);
        assert_eq!(out.counters.fresh, 1);
        // The skip leaves the population: sample_gate counts it (the
        // inclusive equation, mirroring gate②'s head_gate).
        assert_eq!(out.counters.sample_gate.files, 1);
        assert_eq!(out.counters.sample_gate.bytes, SAMPLE_SMALL as u64);
        // The partner went terminal.
        assert_eq!(out.full_todo, vec![slot_of(&cands, p)]);
        assert_eq!(out.hashed_full_small, 1);
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].id, p);
        assert_eq!(out.pending[0].hash_full, Some(xxh64(&content, 0)));
        // Participants: both entered gate③ work and BOTH advanced done
        // — the fresh skip is a processed file, like gate②'s skips.
        assert_eq!(log.lock().unwrap().last(), Some(&(2, 2)));
    }

    /// The placeholder check runs at gate③ too (Ruling 4's order):
    /// a persisted-hash_first candidate that is a cloud placeholder is
    /// counted and never opened — no hydration download.
    #[test]
    fn sample_placeholder_attrs_checked_before_first_open() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'd'; SAMPLE_SMALL];
        let head_hash = xxh64(&content[..4096], 0);
        let _ph = sample_fixture(&store, &root, "ph.bin", &content, Some(head_hash), None);
        let p = sample_fixture(&store, &root, "p.bin", &content, Some(head_hash), None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let mut reader = FakeReader::new();
        reader.placeholders.push(root.join("ph.bin"));
        let (_head, out, _input) = run_sample_chain(&cands, &dirs, &roots, &reader, &store);

        // gate② reused both (no opens); gate③ never touched ph.bin.
        assert_eq!(*reader.opened.lock().unwrap(), vec![root.join("p.bin")]);
        assert_eq!(out.counters.placeholders, 1);
        assert_eq!(out.counters.sample_gate.files, 1);
        assert_eq!(out.counters.sample_gate.bytes, SAMPLE_SMALL as u64);
        assert_eq!(out.full_todo, vec![slot_of(&cands, p)]);
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.pending[0].id, p);
    }

    /// A sharing-violation open at gate③ is a Locked skip: persisted
    /// hash_first candidates reach gate③ unopened, so the sharing
    /// violation fires HERE, counts `locked`, and skips the file.
    #[cfg(windows)]
    #[test]
    fn sample_locked_open_counts_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'l'; SAMPLE_BIG];
        let head_hash = xxh64(&content[..4096], 0);
        let _l1 = sample_fixture(&store, &root, "l1.bin", &content, Some(head_hash), None);
        let _l2 = sample_fixture(&store, &root, "l2.bin", &content, Some(head_hash), None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let mut reader = FakeReader::new();
        reader.locked = vec![root.join("l1.bin"), root.join("l2.bin")];
        let (_head, out, _input) = run_sample_chain(&cands, &dirs, &roots, &reader, &store);

        assert_eq!(out.counters.locked, 2);
        assert_eq!(out.counters.errors, 0, "a lock is not an error");
        assert!(reader.opened.lock().unwrap().is_empty(), "nothing was opened");
        assert_eq!(out.counters.sample_gate.files, 2);
        assert_eq!(out.counters.sample_gate.bytes, 2 * SAMPLE_BIG as u64);
        assert!(out.full_todo.is_empty());
    }

    /// Two same-size candidates whose sampled windows differ form two
    /// singleton `(size, key)` runs: both are killed into sample_gate
    /// with their byte tallies, and nothing is persisted (sample keys
    /// are never stored).
    #[test]
    fn sample_distinct_windows_kill_singletons_into_sample_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content_a = vec![b'a'; SAMPLE_BIG];
        let mut content_b = content_a.clone();
        content_b[70_000] = b'b'; // inside BOTH windows: [8KB, 73.7KB), [16KB, 80KB)
        let a = sample_fixture(&store, &root, "a.bin", &content_a, None, None);
        let b = sample_fixture(&store, &root, "b.bin", &content_b, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        let reader = FakeReader::new();
        let (_head, out, _input) = run_sample_chain(&cands, &dirs, &roots, &reader, &store);

        // Same head (first 4KB identical) → sampled together; distinct
        // windows → distinct keys → two singleton kills.
        assert_eq!(out.counters.sample_gate.files, 2);
        assert_eq!(out.counters.sample_gate.bytes, 2 * SAMPLE_BIG as u64);
        assert!(out.full_todo.is_empty());
        assert!(out.pending.is_empty(), "sample keys are never persisted");
        assert_eq!(out.hashed_full_small, 0);
        assert_eq!(
            out.counters.errors + out.counters.fresh + out.counters.locked,
            0,
            "killed, not skipped"
        );
        // Both were sampled: gate② probe + gate③ window pass each.
        assert_eq!(reader.opened.lock().unwrap().len(), 4);
        let _ = (a, b);
    }

    /// Two same-size candidates with identical sampled bytes share the
    /// sample key and BOTH survive to full_todo — the comparability
    /// core: the window set is a pure function of the row's size.
    #[test]
    fn sample_matching_keys_survive_to_full_todo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'm'; SAMPLE_BIG];
        let a = sample_fixture(&store, &root, "a.bin", &content, None, None);
        let b = sample_fixture(&store, &root, "b.bin", &content, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        // Both rows' own sizes yield the SAME window set — the property
        // the shared key depends on.
        let sa = cands.sizes[slot_of(&cands, a) as usize];
        let sb = cands.sizes[slot_of(&cands, b) as usize];
        assert_eq!(sample_window_offsets(sa), sample_window_offsets(sb));

        let reader = FakeReader::new();
        let (_head, out, _input) = run_sample_chain(&cands, &dirs, &roots, &reader, &store);

        assert_eq!(out.full_todo.len(), 2);
        assert_eq!(sample_pos(&out.full_todo, &cands, a), Some(0));
        assert_eq!(sample_pos(&out.full_todo, &cands, b), Some(1));
        assert_eq!(out.counters.sample_gate.files, 0);
        assert!(out.pending.is_empty(), "full hashes are gate④'s job now");
        assert_eq!(out.hashed_full_small, 0);
    }

    /// Multi-chunk scatter (the gate② bug class): participants split
    /// across chunks and workers — every terminal full hash must land
    /// on ITS OWN file's pending update, whatever the chunking.
    #[test]
    fn sample_multi_chunk_scatters_results_to_their_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let mut contents: Vec<Vec<u8>> = Vec::new();
        let mut ids: Vec<FileId> = Vec::new();
        for i in 0..5u8 {
            let mut content = vec![b'A'; SAMPLE_SMALL]; // equal first 4KB → one run
            content[4096 + i as usize] = b'0' + i; // distinct whole-file hashes
            let name = format!("f{i}.bin");
            fs::write(root.join(&name), &content).unwrap();
            let (id, _) = upsert_row_at(
                &store,
                &["fixture"],
                &name,
                SAMPLE_SMALL as u64,
                NOW - 10_000,
                None,
                None,
            );
            ids.push(id);
            contents.push(content);
        }
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);
        let expected_slots: Vec<u32> = ids.iter().map(|&id| slot_of(&cands, id)).collect();

        for (workers, chunk_size) in [(1, 2), (2, 2), (1, 3), (2, 4)] {
            let reader = FakeReader::new();
            let cancel = AtomicBool::new(false);
            let head = gate_head(
                &cands, &dirs, &all_slots(&cands), &roots, &reader, workers, &cancel, NOW,
                &|_, _| {}, &store,
            );
            let input = sample_input(&cands, &head);
            assert_eq!(input.len(), 5, "one run of five");
            let out = gate_sample_chunked(
                &cands, &dirs, &input, &head, &roots, &reader, workers, &cancel, NOW, &|_, _| {},
                &store, chunk_size,
            );

            assert_eq!(out.hashed_full_small, 5);
            assert_eq!(out.full_todo, expected_slots, "input order preserved");
            assert_eq!(out.counters.sample_gate.files, 0);
            // The side array rides parallel to full_todo across every
            // chunk/worker split: each file's hash at its own position.
            for (i, content) in contents.iter().enumerate() {
                assert_eq!(
                    out.small_full[i],
                    xxh64(content, 0),
                    "chunk_size={chunk_size} workers={workers}: f{i}'s full at its own side-array slot"
                );
            }
            let mut updates: Vec<(FileId, Option<u64>)> = out
                .pending
                .iter()
                .map(|u| (u.id, u.hash_full))
                .collect();
            updates.sort_unstable();
            for (i, id) in ids.iter().enumerate() {
                let got = updates.iter().find(|(fid, _)| fid == id).map(|(_, h)| *h);
                assert_eq!(
                    got,
                    Some(Some(xxh64(&contents[i], 0))),
                    "chunk_size={chunk_size} workers={workers}: f{i}'s full hash on its own update"
                );
            }
        }
    }

    /// Cancel at gate③'s file boundary: the in-flight file finishes
    /// (its terminal update is FLUSHED to the store), later
    /// participants are refused, `cancelled` is reported and done
    /// stops at the processed count.
    #[test]
    fn sample_cancel_stops_between_files_and_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let content = vec![b'z'; SAMPLE_SMALL];
        let a = sample_fixture(&store, &root, "a.bin", &content, None, None);
        let b = sample_fixture(&store, &root, "b.bin", &content, None, None);
        let (cands, dirs) = load_cands(&store);
        let roots = gate_roots(&root);

        // gate② completes normally (own reader); gate③ gets a reader
        // that arms its cancel after the first successful open.
        let cancel = AtomicBool::new(false);
        let head = gate_head(
            &cands, &dirs, &all_slots(&cands), &roots, &FakeReader::new(), 1, &cancel, NOW,
            &|_, _| {}, &store,
        );
        let input = sample_input(&cands, &head);
        assert_eq!(input.len(), 2);

        struct CancelAfterOpen<'a> {
            inner: FakeReader,
            cancel: &'a AtomicBool,
        }
        impl FileReader for CancelAfterOpen<'_> {
            fn attributes(&self, path: &Path) -> Option<u32> {
                self.inner.attributes(path)
            }
            fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
                let file = self.inner.open(path)?;
                self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(file)
            }
        }

        let cancel3 = AtomicBool::new(false);
        let reader = CancelAfterOpen {
            inner: FakeReader::new(),
            cancel: &cancel3,
        };
        let log = progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |done: u64, total: u64| {
            sink.lock().unwrap().push((done, total));
        };
        let out = gate_sample(
            &cands, &dirs, &input, &head, &roots, &reader, 1, &cancel3, NOW, &progress, &store,
        );

        assert!(out.cancelled, "the cancel is reported");
        assert_eq!(
            *reader.inner.opened.lock().unwrap(),
            vec![root.join("a.bin")],
            "gate③ stopped after the first open"
        );
        assert_eq!(log.lock().unwrap().last(), Some(&(1, 2)));
        assert_eq!(out.full_todo, vec![slot_of(&cands, a)]);
        assert!(
            out.small_full == vec![xxh64(&content, 0)],
            "a's full rides the flush-proof side array even though the \
             cancel flushed (drained) pending — the exact hazard the \
             array exists for"
        );
        assert!(out.pending.is_empty(), "the cancel flushed the batch first");
        // And the flush really landed: a.bin's row now carries hash_full.
        let rows = crate::engine::lock_unpoisoned(&store).all_files().unwrap();
        let row_a = rows.iter().find(|r| r.id == a).unwrap();
        assert_eq!(row_a.hash_full, Some(xxh64(&content, 0)));
        let row_b = rows.iter().find(|r| r.id == b).unwrap();
        assert_eq!(row_b.hash_full, None, "the cancelled candidate persisted nothing");
    }

    // ------------------------------------------------------------------
    // Task 6: gate④ (full certification) + run_gates — the four-gate
    // assembly the engine swaps onto. The fixtures run the WHOLE
    // pipeline: 80KB same-content copies survive ①→②→③ into gate④'s
    // full read and out into real groups.

    /// A run_gates fixture file: writes `content` under
    /// `<root>/<dir>/<name>` (`dir` may be empty for the root), upserts
    /// the row with no persisted hashes and a settled mtime, returns the
    /// row id.
    fn gates_fixture(
        store: &StdMutex<Store>,
        root: &Path,
        dir: &str,
        name: &str,
        content: &[u8],
    ) -> FileId {
        let full = if dir.is_empty() {
            root.join(name)
        } else {
            root.join(dir).join(name)
        };
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, content).unwrap();
        let mut segments = vec!["fixture"];
        if !dir.is_empty() {
            segments.push(dir);
        }
        let (id, _) = upsert_row_at(
            store,
            &segments,
            name,
            content.len() as u64,
            NOW - 10_000,
            None,
            None,
        );
        id
    }

    /// A phase-tagged progress collector for the run_gates tests.
    fn gates_progress_log() -> std::rc::Rc<StdMutex<Vec<(DedupPhase, u64, u64)>>> {
        std::rc::Rc::new(StdMutex::new(Vec::new()))
    }

    /// Runs `run_gates` (workers = 1, `NOW`) with a phase-collecting
    /// progress closure — the tests' default runner.
    fn gates_run(
        store: &StdMutex<Store>,
        roots: &[String],
        reader: &dyn FileReader,
        cancel: &AtomicBool,
    ) -> (DedupOutcome, Vec<(DedupPhase, u64, u64)>) {
        let log = gates_progress_log();
        let sink = std::rc::Rc::clone(&log);
        let progress = move |phase: DedupPhase, done: u64, total: u64| {
            sink.lock().unwrap().push((phase, done, total));
        };
        let outcome = run_gates(store, roots, reader, 1, cancel, NOW, &progress);
        let calls = log.lock().unwrap().clone();
        (outcome, calls)
    }

    /// THE end-to-end contract: three identical 80KB files in three
    /// different directories survive all four gates into ONE group of
    /// three members; every phase was reported at least once, in gate
    /// order; and the counters came back clean.
    #[test]
    fn gates_end_to_end_three_copies_form_one_group() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b'e'; SAMPLE_BIG]; // > 64KB: the sampled path
        let (_tstore, store) = test_store();
        let mut ids = Vec::new();
        for (i, dir) in ["d1", "d2", "d3"].iter().enumerate() {
            ids.push(gates_fixture(&store, &root, dir, &format!("f{i}.bin"), &content));
        }
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let (outcome, log) = gates_run(&store, &roots, &FakeReader::new(), &cancel);

        assert!(!outcome.cancelled);
        assert_eq!(outcome.groups.len(), 1, "three identical files: one group");
        let group = &outcome.groups[0];
        assert_eq!(group.size, SAMPLE_BIG as u64);
        assert_eq!(group.hash, xxh64(&content, 0));
        let mut expected = ids.clone();
        expected.sort_unstable();
        assert_eq!(group.members, expected, "members are the row ids, ascending");

        // Every phase reported at least once, in pipeline order, with
        // the run's hash counts as the final Done report.
        let first_of = |phase: DedupPhase| {
            log.iter()
                .position(|(p, _, _)| *p == phase)
                .unwrap_or_else(|| panic!("phase {phase:?} never reported: {log:?}"))
        };
        let (g, h, s, f, d) = (
            first_of(DedupPhase::Grouping),
            first_of(DedupPhase::HashingFirst),
            first_of(DedupPhase::Sampling),
            first_of(DedupPhase::HashingFull),
            first_of(DedupPhase::Done),
        );
        assert!(g < h && h < s && s < f && f < d, "phases in gate order: {log:?}");
        assert_eq!(
            *log.last().unwrap(),
            (DedupPhase::Done, 6, 6),
            "3 first + 3 full hashes computed this run"
        );

        assert_eq!(outcome.hashed_first, 3);
        assert_eq!(outcome.hashed_full, 3, "gate④ computed one full per file");
        let c = &outcome.counters;
        assert_eq!(c.size_gate.files + c.head_gate.files + c.sample_gate.files, 0);
        assert_eq!(c.errors + c.locked + c.fresh + c.placeholders + c.recycle, 0);
        // The legacy scalar mirrors (engine.rs reads them until Task 7).
        assert_eq!(outcome.errors, 0);
        assert_eq!(outcome.locked, 0);
        assert_eq!(outcome.placeholders, 0);
    }

    /// Same size, first 4KB differs: the `(size, head)` regroup
    /// dissolves the pair BEFORE gate③ — nothing sampled, nothing fully
    /// read, no group, and no gate kill counted (both members carry a
    /// head; the kill is the regroup's, like the old pipeline's).
    #[test]
    fn gates_same_size_different_head_never_reaches_sampling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content_a = vec![b'a'; SAMPLE_BIG];
        let mut content_b = content_a.clone();
        content_b[100] = b'b'; // inside the first 4KB
        let (_tstore, store) = test_store();
        gates_fixture(&store, &root, "", "a.bin", &content_a);
        gates_fixture(&store, &root, "", "b.bin", &content_b);
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, _log) = gates_run(&store, &roots, &reader, &cancel);

        assert!(outcome.groups.is_empty());
        assert_eq!(
            *reader.opened.lock().unwrap(),
            vec![root.join("a.bin"), root.join("b.bin")],
            "one open each — the gate② probes; gates ③④ never ran"
        );
        assert_eq!(outcome.hashed_full, 0);
        assert_eq!(outcome.counters.sample_gate.files, 0, "gate③ saw nobody");
        assert_eq!(outcome.counters.head_gate.files, 0, "both carry a head");
    }

    /// Same size, same first 4KB, different tail: sampled together,
    /// sample keys differ, BOTH are shot down as singleton
    /// `(size, key)` runs — no group, no full hash ever computed.
    #[test]
    fn gates_same_head_different_tail_shot_down_by_sampling() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content_a = vec![b'a'; SAMPLE_BIG];
        let mut content_b = content_a.clone();
        content_b[70_000] = b'b'; // inside BOTH windows
        let (_tstore, store) = test_store();
        gates_fixture(&store, &root, "", "a.bin", &content_a);
        gates_fixture(&store, &root, "", "b.bin", &content_b);
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, _log) = gates_run(&store, &roots, &reader, &cancel);

        assert!(outcome.groups.is_empty());
        assert_eq!(outcome.hashed_full, 0, "shot down before gate④");
        assert_eq!(outcome.counters.sample_gate.files, 2);
        assert_eq!(outcome.counters.sample_gate.bytes, 2 * SAMPLE_BIG as u64);
        assert_eq!(
            outcome.counters.errors + outcome.counters.locked + outcome.counters.fresh,
            0,
            "shot down, not skipped"
        );
        assert_eq!(
            reader.opened.lock().unwrap().len(),
            4,
            "2 gate② probes + 2 gate③ window passes"
        );
    }

    /// The certified-partner crowd, end to end: a member with REAL
    /// persisted first+full hashes and a new same-content twin form a
    /// group — the twin was short-circuited into gate④ (gate③ never
    /// sampled it), the certified member was never opened, and the
    /// group's hash is the persisted (real) full. The Task-5 test used
    /// a fake persisted full because gate③ alone never groups; here it
    /// must be the content's true hash or the pair could not group.
    #[test]
    fn gates_certified_partner_groups_with_new_twin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b'c'; SAMPLE_BIG];
        let first = xxh64(&content[..4096], 0);
        let full = xxh64(&content, 0);
        let (_tstore, store) = test_store();
        let a = gates_fixture(&store, &root, "", "a.bin", &content);
        let (f, _) = upsert_row_at(
            &store,
            &["fixture"],
            "twin.bin",
            content.len() as u64,
            NOW - 10_000,
            Some(first),
            Some(full),
        );
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, _log) = gates_run(&store, &roots, &reader, &cancel);

        // Only the new twin was ever opened: its gate② probe, then its
        // gate④ full read. The certified member lived on persisted
        // hashes alone.
        assert_eq!(
            *reader.opened.lock().unwrap(),
            vec![root.join("a.bin"), root.join("a.bin")],
            "twin.bin twice (probe + gate④), certified member never"
        );
        assert_eq!(outcome.groups.len(), 1);
        let group = &outcome.groups[0];
        assert_eq!(group.size, SAMPLE_BIG as u64);
        assert_eq!(group.hash, full);
        let mut members = vec![a, f];
        members.sort_unstable();
        assert_eq!(group.members, members, "ids ascending");
        assert_eq!(outcome.hashed_first, 1, "the twin's probe");
        assert_eq!(outcome.hashed_full, 1, "the twin's gate④ read");
        assert_eq!(outcome.counters.head_gate.files, 0);
        assert_eq!(outcome.counters.sample_gate.files, 0, "short-circuit is free");
    }

    /// Group order: size descending; equal sizes break by hash
    /// ascending; members are files-row ids ascending. Two same-size
    /// groups with different content pin the tiebreak (the expected
    /// order is computed from the hashes — the test must not assume
    /// which content hashes smaller).
    #[test]
    fn gates_group_ordering_size_desc_hash_asc_members_asc() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let big = vec![b'X'; 100 * 1024];
        let y = vec![b'Y'; SAMPLE_SMALL + 54 * 1024]; // 70KB, distinct from big
        let z = vec![b'Z'; y.len()];
        let (_tstore, store) = test_store();
        let mut big_ids = Vec::new();
        let mut y_ids = Vec::new();
        let mut z_ids = Vec::new();
        big_ids.push(gates_fixture(&store, &root, "d1", "b1.bin", &big));
        big_ids.push(gates_fixture(&store, &root, "d1", "b2.bin", &big));
        y_ids.push(gates_fixture(&store, &root, "d2", "y1.bin", &y));
        y_ids.push(gates_fixture(&store, &root, "d2", "y2.bin", &y));
        z_ids.push(gates_fixture(&store, &root, "d3", "z1.bin", &z));
        z_ids.push(gates_fixture(&store, &root, "d3", "z2.bin", &z));
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let (outcome, _log) = gates_run(&store, &roots, &FakeReader::new(), &cancel);

        assert_eq!(outcome.groups.len(), 3);
        // Size descending: the 100KB group first...
        assert_eq!(outcome.groups[0].size, big.len() as u64);
        assert_eq!(outcome.groups[0].hash, xxh64(&big, 0));
        big_ids.sort_unstable();
        assert_eq!(outcome.groups[0].members, big_ids);
        // ...then the two 70KB groups, hash ascending.
        let (hy, hz) = (xxh64(&y, 0), xxh64(&z, 0));
        let (lo, hi) = if hy <= hz { (hy, hz) } else { (hz, hy) };
        assert_eq!(outcome.groups[1].size, y.len() as u64);
        assert_eq!(outcome.groups[1].hash, lo, "equal sizes: hash ascending");
        assert_eq!(outcome.groups[2].size, z.len() as u64);
        assert_eq!(outcome.groups[2].hash, hi);
        for group in &outcome.groups[1..] {
            let ids = if group.hash == hy { &y_ids } else { &z_ids };
            let mut ids = ids.clone();
            ids.sort_unstable();
            assert_eq!(group.members, ids, "members id ascending");
        }
    }

    /// The brief's pinned cancel case: the progress callback arms
    /// `cancel` at the FIRST HashingFull report — gate④ never runs, the
    /// run finishes cancelled with an empty group list, and the hashes
    /// the earlier gates computed are already persisted (visible after
    /// the store is REOPENED — cancel never loses computed work).
    #[test]
    fn gates_cancel_at_hashing_full_flushes_computed_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b'e'; SAMPLE_BIG];
        // The STORE's tempdir (test_store opens `index.db` in it) — the
        // reopened database below must be THIS file, not a fresh one.
        let (store_tmp, store) = test_store();
        gates_fixture(&store, &root, "d1", "a.bin", &content);
        gates_fixture(&store, &root, "d2", "b.bin", &content);
        let roots = gate_roots(&root);

        let cancel = AtomicBool::new(false);
        let log = gates_progress_log();
        let sink = std::rc::Rc::clone(&log);
        // A borrowing closure: it arms `cancel` and logs synchronously
        // while run_gates runs, and both locals outlive the call.
        let progress = |phase: DedupPhase, done: u64, total: u64| {
            if phase == DedupPhase::HashingFull {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            sink.lock().unwrap().push((phase, done, total));
        };
        let outcome = run_gates(&store, &roots, &FakeReader::new(), 1, &cancel, NOW, &progress);
        let log = log.lock().unwrap().clone();

        assert!(outcome.cancelled);
        assert!(outcome.groups.is_empty(), "a cancelled run reports no groups");
        assert_eq!(
            *log.last().unwrap(),
            (DedupPhase::Cancelled, 0, 2),
            "the final report is the cancelled one"
        );
        assert!(
            !log.iter().any(|(p, _, _)| *p == DedupPhase::Done),
            "cancel replaces Done"
        );

        // The store REOPENED sees the computed first hashes: gate②
        // completed and flushed before the cancel point.
        let db = store_tmp.path().join("index.db");
        drop(store);
        let reopened = Store::open(&db).unwrap();
        let rows = reopened.all_files().unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(
                row.hash_first,
                Some(xxh64(&content[..4096], 0)),
                "gate②'s computed hash survived the cancel"
            );
            assert_eq!(row.hash_full, None, "gate④ never ran");
        }
    }

    /// Cancel armed at the first report of EACH phase — Grouping,
    /// HashingFirst, Sampling, HashingFull — always ends the run
    /// cancelled with an empty group list and a Cancelled final report.
    #[test]
    fn gates_cancel_at_any_phase_ends_cancelled() {
        for arm in [
            DedupPhase::Grouping,
            DedupPhase::HashingFirst,
            DedupPhase::Sampling,
            DedupPhase::HashingFull,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("fixture");
            let content = vec![b'e'; SAMPLE_BIG];
            let (_tstore, store) = test_store();
            gates_fixture(&store, &root, "d1", "a.bin", &content);
            gates_fixture(&store, &root, "d2", "b.bin", &content);
            let roots = gate_roots(&root);

            let cancel = AtomicBool::new(false);
            let log = gates_progress_log();
            let sink = std::rc::Rc::clone(&log);
            let progress = |phase: DedupPhase, done: u64, total: u64| {
                if phase == arm {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                sink.lock().unwrap().push((phase, done, total));
            };
            let outcome = run_gates(&store, &roots, &FakeReader::new(), 1, &cancel, NOW, &progress);
            let log = log.lock().unwrap().clone();

            assert!(outcome.cancelled, "arming at {arm:?} must cancel the run");
            assert!(outcome.groups.is_empty(), "arming at {arm:?}: no groups");
            assert_eq!(
                log.last().map(|(p, _, _)| *p),
                Some(DedupPhase::Cancelled),
                "arming at {arm:?}: the final report is Cancelled: {log:?}"
            );
            assert!(
                !log.iter().any(|(p, _, _)| *p == DedupPhase::Done),
                "arming at {arm:?}: cancel replaces Done"
            );
        }
    }

    /// Arms `cancel` when `target` is opened for the `n`-th time. A full
    /// pipeline run opens each fixture file exactly THREE times (gate②
    /// probe, gate③ window pass, gate④ sequential read), so n = 3 on
    /// the second file arms the cancel exactly at gate④'s open of it.
    struct ArmOnNthOpen<'a> {
        inner: FakeReader,
        target: PathBuf,
        n: usize,
        opens: StdMutex<usize>,
        cancel: &'a AtomicBool,
    }

    impl FileReader for ArmOnNthOpen<'_> {
        fn attributes(&self, path: &Path) -> Option<u32> {
            self.inner.attributes(path)
        }
        fn open(&self, path: &Path) -> std::io::Result<std::fs::File> {
            let file = self.inner.open(path)?;
            if path == self.target {
                let mut opens = self.opens.lock().unwrap();
                *opens += 1;
                if *opens == self.n {
                    self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Ok(file)
        }
    }

    /// Gate④ checks cancel at EVERY 64KB buffer step, not only between
    /// files: `a` completes its full read (its hash lands — 已算哈希照常
    /// 落库); `b`'s gate④ open arms the cancel and its FIRST buffer step
    /// abandons it, so `b` never gets a full hash. A between-files-only
    /// check would have completed `b` too and failed the `b` assertion.
    #[test]
    fn gates_full_gate_checks_cancel_every_buffer_step() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b'e'; SAMPLE_BIG]; // 80KB: two 64KB read steps
        let (_tstore, store) = test_store();
        gates_fixture(&store, &root, "d1", "a.bin", &content);
        gates_fixture(&store, &root, "d2", "b.bin", &content);
        let roots = gate_roots(&root);

        let cancel = AtomicBool::new(false);
        let reader = ArmOnNthOpen {
            inner: FakeReader::new(),
            target: root.join("d2").join("b.bin"),
            n: 3,
            opens: StdMutex::new(0),
            cancel: &cancel,
        };
        let (outcome, log) = gates_run(&store, &roots, &reader, &cancel);

        assert!(outcome.cancelled);
        assert!(outcome.groups.is_empty());
        assert_eq!(
            log.last().map(|(p, _, _)| *p),
            Some(DedupPhase::Cancelled)
        );

        // `a` finished before the armed open; `b` was abandoned at its
        // first buffer step.
        let full = xxh64(&content, 0);
        let rows = crate::engine::lock_unpoisoned(&store).all_files().unwrap();
        let row_a = rows.iter().find(|r| r.name == "a.bin").unwrap();
        assert_eq!(row_a.hash_full, Some(full), "a's computed hash landed");
        let row_b = rows.iter().find(|r| r.name == "b.bin").unwrap();
        assert_eq!(
            row_b.hash_full, None,
            "b was abandoned mid-read: no partial hash may persist"
        );
    }

    /// Gate④ multi-chunk scatter (the gate②/③ bug class): members split
    /// across chunks and workers — every computed full hash must land on
    /// ITS OWN slot in the outcome array, whatever the chunking.
    #[test]
    fn gate_full_multi_chunk_scatters_every_full_to_its_own_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();

        let (_tstore, store) = test_store();
        let mut contents: Vec<Vec<u8>> = Vec::new();
        for i in 0..5u8 {
            // > 64KB so gate④ really streams: distinct tails, identical
            // first 4KB (one (size, head) run through the sampler).
            let mut content = vec![b'A'; SAMPLE_BIG];
            content[4096 + i as usize] = b'0' + i;
            let name = format!("f{i}.bin");
            fs::write(root.join(&name), &content).unwrap();
            let _ = upsert_row_at(
                &store,
                &["fixture"],
                &name,
                content.len() as u64,
                NOW - 10_000,
                None,
                None,
            );
            contents.push(content);
        }
        let (cands, dirs) = load_cands(&store);
        // The candidate stream is id-ascending, so slots are 0..5 in
        // insertion order.
        let slots: Vec<u32> = (0..5u32).collect();
        let roots = gate_roots(&root);

        for (workers, chunk_size) in [(1, 2), (2, 2), (1, 3), (2, 4)] {
            let reader = FakeReader::new();
            let cancel = AtomicBool::new(false);
            let known = vec![NO_HASH; slots.len()];
            let log = progress_log();
            let sink = std::rc::Rc::clone(&log);
            let progress = move |done: u64, total: u64| {
                sink.lock().unwrap().push((done, total));
            };
            let out = gate_full_chunked(
                &cands, &dirs, &slots, &known, &roots, &reader, workers, &cancel, NOW, &progress,
                &store, chunk_size,
            );

            assert!(!out.cancelled);
            assert_eq!(out.hashed_full, 5);
            for (i, content) in contents.iter().enumerate() {
                let expected = xxh64(content, 0);
                assert_eq!(
                    out.full[i], expected,
                    "chunk_size={chunk_size} workers={workers}: f{i}'s full must sit at its own slot {i}"
                );
            }
            assert_eq!(out.pending.len(), 5);
            assert_eq!(log.lock().unwrap().last(), Some(&(5, 5)));
        }
    }

    /// Two identical files in gate③'s TERMINAL-SMALL class (4KB < size
    /// ≤ 64KB, e.g. ~32KB) group from the fulls gate③ computed and
    /// queued in `SampleOutcome.pending` — and gate④ must NOT re-read
    /// them. This pins the small_full merge ordering: run_gates must
    /// build its merge map BEFORE flushing the pending residue (the
    /// flush drains the vec), or the small-terminal source dies (gate④
    /// re-opens every such file, `hashed_full` double-counts, and a
    /// gate④ re-open failure would silently dissolve certified groups).
    #[test]
    fn gates_sample_small_terminal_group_without_gate4_rereads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b't'; 32 * 1024]; // 4KB < 32KB ≤ 64KB
        let (_tstore, store) = test_store();
        gates_fixture(&store, &root, "d1", "a.bin", &content);
        gates_fixture(&store, &root, "d2", "b.bin", &content);
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, log) = gates_run(&store, &roots, &reader, &cancel);

        // EXACTLY four opens: gate②'s two probes, gate③'s two whole
        // reads. Gate④ contributed ZERO — the small-terminal fulls
        // reached the `known` slice, so no re-read pass.
        assert_eq!(
            *reader.opened.lock().unwrap(),
            vec![
                root.join("d1").join("a.bin"),
                root.join("d2").join("b.bin"),
                root.join("d1").join("a.bin"),
                root.join("d2").join("b.bin"),
            ],
            "gate② probe + gate③ whole read per file; gate④ must not re-open"
        );
        // The group forms from the gate③-computed fulls.
        assert_eq!(outcome.groups.len(), 1);
        assert_eq!(outcome.groups[0].size, content.len() as u64);
        assert_eq!(outcome.groups[0].hash, xxh64(&content, 0));
        assert_eq!(outcome.groups[0].members.len(), 2);
        // Each file's full hash counted EXACTLY once (gate③'s terminal
        // path; gate④ re-reads would double it to 4).
        assert_eq!(outcome.hashed_first, 2);
        assert_eq!(outcome.hashed_full, 2);
        assert_eq!(*log.last().unwrap(), (DedupPhase::Done, 4, 4));
    }

    /// Two identical ≤4KB files group from gate②'s TERMINAL values
    /// alone (the probe read IS the whole file): full_todo members are
    /// known-full at gate④ without a single full-stage open — pinning
    /// the `HeadOutcome::full` source of run_gates' full merge.
    #[test]
    fn gates_small_terminal_files_group_from_head_fulls() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = b"tiny twin bytes"; // 15 bytes: terminal at gate②
        let (_tstore, store) = test_store();
        gates_fixture(&store, &root, "d1", "a.bin", content);
        gates_fixture(&store, &root, "d2", "b.bin", content);
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, _log) = gates_run(&store, &roots, &reader, &cancel);

        // One open per file — the gate② probes. gate③ certified them,
        // gate④ had nothing to read.
        assert_eq!(reader.opened.lock().unwrap().len(), 2);
        assert_eq!(outcome.groups.len(), 1);
        assert_eq!(outcome.groups[0].size, content.len() as u64);
        assert_eq!(outcome.groups[0].hash, xxh64(content, 0));
        assert_eq!(outcome.hashed_full, 0, "no full-stage computation at all");
        assert_eq!(outcome.hashed_first, 2);
    }

    /// THE AT-SCALE flush-boundary test: 130 identical 32KB files all
    /// take gate③'s terminal-small path, so `out.pending` reaches
    /// `PERSIST_BATCH` (128) and the chunk-boundary flush DRAINS it
    /// mid-gate — the residue run_gates used to build its merge map
    /// from comes back empty. The fulls must reach gate④ through the
    /// flush-proof `SampleOutcome::small_full` side array instead:
    /// gate④ opens ZERO files, every full hash is counted exactly
    /// once, and the whole crowd still groups. (2-file fixtures cannot
    /// see this boundary; the real library's 4KB<size≤64KB bucket is
    /// ~533K files, so EVERY real run crosses it many times over.)
    #[test]
    fn gates_sample_small_terminal_fulls_survive_boundary_flushes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fixture");
        let content = vec![b't'; 32 * 1024];
        let (_tstore, store) = test_store();
        const N: usize = 130; // > PERSIST_BATCH: the boundary flush fires
        let mut ids = Vec::new();
        for i in 0..N {
            ids.push(gates_fixture(&store, &root, "", &format!("f{i:03}.bin"), &content));
        }
        let roots = gate_roots(&root);
        let cancel = AtomicBool::new(false);

        let reader = FakeReader::new();
        let (outcome, log) = gates_run(&store, &roots, &reader, &cancel);

        // EXACTLY two opens per file — gate②'s probe and gate③'s whole
        // read. Gate④ contributed ZERO despite the drained residue.
        let mut per_path: HashMap<PathBuf, usize> = HashMap::new();
        for path in reader.opened.lock().unwrap().iter() {
            *per_path.entry(path.clone()).or_default() += 1;
        }
        assert_eq!(per_path.len(), N, "every fixture opened");
        assert!(
            per_path.values().all(|&count| count == 2),
            "each file exactly probe + whole read, never a gate④ re-read"
        );
        // The group forms from the gate③-computed fulls: ONE group of
        // all 130 members, ids ascending.
        assert_eq!(outcome.groups.len(), 1);
        assert_eq!(outcome.groups[0].size, content.len() as u64);
        assert_eq!(outcome.groups[0].hash, xxh64(&content, 0));
        let mut expected = ids.clone();
        expected.sort_unstable();
        assert_eq!(outcome.groups[0].members, expected);
        // Each full counted EXACTLY once (gate③'s terminal path; a
        // re-read pass would double it to 260).
        assert_eq!(outcome.hashed_first, N as u64);
        assert_eq!(outcome.hashed_full, N as u64);
        assert_eq!(*log.last().unwrap(), (DedupPhase::Done, 2 * N as u64, 2 * N as u64));
        // And the boundary flush really landed: every row carries its
        // full hash.
        let rows = crate::engine::lock_unpoisoned(&store).all_files().unwrap();
        assert_eq!(rows.len(), N);
        for row in &rows {
            assert_eq!(row.hash_full, Some(xxh64(&content, 0)));
        }
    }

    /// A store whose candidate load fails (an id past `u32::MAX` — the
    /// loader's checked conversion) yields the trivial run: empty
    /// groups, not cancelled, zero counters, and the complete phase
    /// walk at `(0, 0)`.
    #[test]
    fn gates_store_load_error_yields_trivial_run() {
        let (_tstore, store) = test_store();
        // One pair that would normally group — plus a row the loader
        // must reject, which turns the WHOLE run trivial.
        let (_twin_a, _twin_b) = (
            upsert_row(&store, &["fixture"], "twin_a.bin", 5, false, None, None).0,
            upsert_row(&store, &["fixture"], "twin_b.bin", 5, false, None, None).0,
        );
        let fixture_dir = crate::engine::lock_unpoisoned(&store)
            .all_dirs()
            .unwrap()
            .iter()
            .find(|(_, _, name)| name == "fixture")
            .map(|(id, _, _)| *id)
            .unwrap();
        {
            let mut guard = crate::engine::lock_unpoisoned(&store);
            guard
                .conn_mut()
                .execute(
                    "INSERT INTO files(id, dir, name, size, mtime, ctime, atime, ext_id, is_dir,
                                       py_full, py_init, scan_gen)
                     VALUES (?1, ?2, 'huge.bin', 5, 0, 0, 0, 0, 0, '', '', 0)",
                    rusqlite::params![i64::MAX, fixture_dir],
                )
                .unwrap();
        }
        let roots = vec![String::new()];
        let cancel = AtomicBool::new(false);

        let (outcome, log) = gates_run(&store, &roots, &FakeReader::new(), &cancel);

        assert!(outcome.groups.is_empty());
        assert!(!outcome.cancelled);
        assert_eq!(outcome.counters, OutcomeCounters::default());
        assert_eq!(
            log,
            vec![
                (DedupPhase::Grouping, 0, 0),
                (DedupPhase::HashingFirst, 0, 0),
                (DedupPhase::Sampling, 0, 0),
                (DedupPhase::HashingFull, 0, 0),
                (DedupPhase::Done, 0, 0),
            ],
            "the trivial run still walks every phase"
        );
    }
}
