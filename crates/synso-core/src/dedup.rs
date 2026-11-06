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
use std::io::Read;
use std::path::{Path, PathBuf, MAIN_SEPARATOR_STR};
use std::sync::Mutex;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

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
    /// Files skipped because another handle denied our open (a Windows
    /// sharing violation, [`OpenSkip::Locked`]): nothing was read, and the
    /// next run simply retries.
    #[allow(dead_code)] // Task 4-7 wiring: gate counters + engine swap read it.
    pub locked: u64,
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
        let recycle =
            name == RECYCLE_BIN_DIR || parent_slot.is_some_and(|s| dirs.recycle[s as usize]);
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
}
