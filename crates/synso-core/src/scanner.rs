//! Full-scan walker: parallel jwalk traversal feeding a single sequential
//! `Store` writer, with globset-based exclusions and a reparse-point guard
//! (symlinks/junctions are neither indexed nor descended, so the walk stays
//! acyclic on every platform).
//!
//! On Windows every root is traversed through its `\\?\`-verbatim form so
//! the walk is immune to the 260-char MAX_PATH limit on hosts without
//! LongPathsEnabled; user-visible paths (exclusion matching, dirs-table
//! segments) keep their ordinary spelling.
//!
//! Threading shape: jwalk's worker threads only read directories, prune
//! excluded/reparse-point children and prefetch metadata into each entry's
//! `client_state`; they never touch [`Store`] (which is `&mut`, not `Sync`).
//! The calling thread drains the iterator and upserts records sequentially.

use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use globset::{Glob, GlobSet};
use jwalk::WalkDirGeneric;

use crate::model::FileRecord;
use crate::store::Store;
use crate::{Result, SynsoError};

/// Records per progress batch. The progress closure is invoked with the
/// files-done count after every full batch and once more at the end of the
/// scan (the total is unknown during a walk, so batches are the only unit).
const PROGRESS_BATCH: usize = 1000;

/// What to scan. Empty `roots` means "all fixed local volumes"
/// (see [`crate::volumes::fixed_volumes`]). `exclusions` are globset
/// patterns matched against the forward-slash full path and against every
/// individual path segment, so `node_modules` excludes a directory of that
/// name anywhere in the tree. A match prunes the whole subtree; an empty
/// list indexes everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanOptions {
    pub roots: Vec<String>,
    pub exclusions: Vec<String>,
}

/// Aggregate outcome of one full scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
}

/// Walks every root (all fixed volumes when `opts.roots` is empty) and
/// upserts what it finds into `store`. Filesystem errors are counted in
/// [`ScanStats::errors`] and never abort the scan; store errors are
/// returned. `progress` receives the files-done count after every
/// [`PROGRESS_BATCH`] records and once at the end.
///
/// On Windows, traversal uses the `\\?\`-verbatim form of each root (see
/// [`traversal_root`]) while exclusion matching and the `dirs` table keep
/// the ordinary, non-verbatim spelling.
pub fn scan(store: &mut Store, opts: &ScanOptions, progress: &dyn Fn(u64)) -> Result<ScanStats> {
    let matcher = build_matcher(&opts.exclusions)?;
    let roots = effective_roots(&opts.roots);
    let mut stats = ScanStats::default();
    // (dir segments, record) pairs buffered from the walk; upserted on the
    // calling thread in batches so walker output never blocks on the store.
    let mut batch: Vec<(Vec<String>, FileRecord)> = Vec::with_capacity(PROGRESS_BATCH);

    for root in &roots {
        let root_path = PathBuf::from(root);
        // Traversal form (verbatim on Windows, identical elsewhere);
        // user-visible spelling below stays non-verbatim.
        let walk_root = traversal_root(&root_path);
        let root_segment = root_segment(&root_path);

        // The root entry itself is never indexed. A reparse-point root
        // (symlink/junction) would undermine the acyclicity guarantee, so
        // such roots are skipped and counted as errors; an excluded root is
        // skipped silently (that is caller intent, not a failure).
        if excluded(&matcher, &root_path, &root_path) {
            continue;
        }
        if is_reparse_point(&walk_root) {
            stats.errors += 1;
            continue;
        }

        let walker = WalkDirGeneric::<((), Option<Metadata>)>::new(walk_root.clone())
            .follow_links(false)
            .min_depth(1)
            .process_read_dir({
                let matcher = matcher.clone();
                let walker_root = walk_root.clone();
                move |_depth, _path, _read_dir_state, children| {
                    // Prune excluded subtrees and reparse points. Removing a
                    // directory here also stops jwalk from descending into
                    // it, which is what makes junction cycles impossible.
                    children.retain(|child| match child {
                        // Keep read errors so the main loop can count them.
                        Err(_) => true,
                        Ok(entry) => {
                            let path = entry.path();
                            !excluded(&matcher, &path, &walker_root)
                                && !entry.file_type().is_symlink()
                                && !has_reparse_attr(&path)
                        }
                    });
                    // Prefetch metadata on the walker threads; the calling
                    // thread only upserts (it owns the store).
                    for entry in children.iter_mut().flatten() {
                        entry.client_state = entry.path().symlink_metadata().ok();
                    }
                }
            });

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            // Defense in depth for the reparse guard above; `file_type` was
            // copied from the directory listing, so this is free.
            if entry.file_type().is_symlink() {
                continue;
            }

            let path = entry.path();
            let rel = match path.strip_prefix(&walk_root) {
                Ok(rel) => rel,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };
            let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
                stats.errors += 1; // non-UTF-8 names cannot be stored as TEXT
                continue;
            };
            let Some(meta) = entry.client_state.as_ref() else {
                stats.errors += 1; // stat failed (vanished/unreadable)
                continue;
            };
            let is_dir = entry.file_type().is_dir();

            let mut segments = Vec::with_capacity(entry.depth + 1);
            segments.push(root_segment.clone());
            if let Some(parent) = rel.parent() {
                let mut all_utf8 = true;
                for component in parent.components() {
                    match component.as_os_str().to_str() {
                        Some(segment) => segments.push(segment.to_string()),
                        None => {
                            all_utf8 = false;
                            break;
                        }
                    }
                }
                if !all_utf8 {
                    stats.errors += 1;
                    continue;
                }
            }

            if is_dir {
                stats.dirs += 1;
            } else {
                stats.files += 1;
            }
            batch.push((segments, new_record(name, meta, is_dir)));

            if batch.len() >= PROGRESS_BATCH {
                flush_batch(store, &mut batch)?;
                progress(stats.files);
            }
        }
    }

    flush_batch(store, &mut batch)?;
    progress(stats.files);
    Ok(stats)
}

/// One indexed entry's [`FileRecord`] from prefetched metadata — the
/// shared construction for the full scan and the watcher's subtree walk
/// (directories record size 0; unavailable timestamps become 0; hashes and
/// pinyin are filled in elsewhere: dedup later, the store at upsert).
pub(crate) fn new_record(name: &str, meta: &Metadata, is_dir: bool) -> FileRecord {
    FileRecord {
        id: 0,
        parent: 0,
        name: name.to_string(),
        size: if is_dir { 0 } else { meta.len() },
        mtime: unix_secs(meta.modified()),
        ctime: unix_secs(meta.created()),
        atime: unix_secs(meta.accessed()),
        ext_id: 0,
        is_dir,
        hash_first: None,
        hash_full: None,
        // Pinyin keys are store-assigned at upsert time; the scanner never
        // computes them itself.
        py_full: String::new(),
        py_init: String::new(),
    }
}

/// Walks the DESCENDANTS of `dir` (never `dir` itself) and upserts every
/// entry into `store`, applying the exact conventions of [`scan`]: the
/// exclusion patterns prune whole subtrees, symlinks/reparse points are
/// neither indexed nor descended (so the walk stays acyclic), directories
/// record size 0, and each descendant's `dirs` chain is `dir_segments` —
/// the chain of `dir` itself, root segment first — extended by its
/// relative path components. After each successful upsert,
/// `stored(store, record)` is invoked with the row as persisted (id,
/// parent, ext_id and pinyin assigned) — the watcher's hook for applying
/// the same row to the hot index incrementally.
///
/// Watcher-only today (Task 8): a created or renamed directory arrives as
/// a single event — ReadDirectoryChangesW/inotify report just the
/// directory entry, children generate no events — so the batch must walk
/// what moved inside it. The walk runs under the store lock the caller
/// already holds for its batch, exactly like a full scan holds it for its
/// whole walk; watcher batches queue meanwhile. Non-purging by design:
/// purging belongs to the rescan path. FS errors are counted in
/// [`ScanStats::errors`] and never abort the walk; store errors are
/// returned.
pub(crate) fn scan_subtree(
    store: &mut Store,
    dir: &Path,
    dir_segments: &[String],
    root: &Path,
    matcher: &GlobSet,
    stored: &mut dyn FnMut(&Store, &FileRecord),
) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    // Nothing sane to walk under a reparse point (cycle risk); the entry
    // itself was already indexed from its event, and the accepted
    // junction corner self-heals at the next rescan.
    if is_reparse_point(dir) {
        stats.errors += 1;
        return Ok(stats);
    }
    let mut batch: Vec<(Vec<String>, FileRecord)> = Vec::with_capacity(PROGRESS_BATCH);

    let walker = WalkDirGeneric::<((), Option<Metadata>)>::new(dir)
        .follow_links(false)
        .min_depth(1)
        .process_read_dir({
            let matcher = matcher.clone();
            let root = root.to_path_buf();
            move |_depth, _path, _read_dir_state, children| {
                // Same pruning as `scan`: excluded subtrees and reparse
                // points never yield, which is what bounds the walk and
                // keeps it out of junction cycles.
                children.retain(|child| match child {
                    // Keep read errors so the main loop can count them.
                    Err(_) => true,
                    Ok(entry) => {
                        let path = entry.path();
                        !excluded(&matcher, &path, &root)
                            && !entry.file_type().is_symlink()
                            && !has_reparse_attr(&path)
                    }
                });
                // Prefetch metadata on the walker threads; the calling
                // thread only upserts (it owns the store).
                for entry in children.iter_mut().flatten() {
                    entry.client_state = entry.path().symlink_metadata().ok();
                }
            }
        });

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        // Defense in depth for the reparse guard above.
        if entry.file_type().is_symlink() {
            continue;
        }

        let path = entry.path();
        let rel = match path.strip_prefix(dir) {
            Ok(rel) => rel,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
            stats.errors += 1; // non-UTF-8 names cannot be stored as TEXT
            continue;
        };
        let Some(meta) = entry.client_state.as_ref() else {
            stats.errors += 1; // stat failed (vanished/unreadable)
            continue;
        };
        let is_dir = entry.file_type().is_dir();

        let mut segments = Vec::with_capacity(dir_segments.len() + entry.depth + 1);
        segments.extend_from_slice(dir_segments);
        if let Some(parent) = rel.parent() {
            let mut all_utf8 = true;
            for component in parent.components() {
                match component.as_os_str().to_str() {
                    Some(segment) => segments.push(segment.to_string()),
                    None => {
                        all_utf8 = false;
                        break;
                    }
                }
            }
            if !all_utf8 {
                stats.errors += 1;
                continue;
            }
        }

        if is_dir {
            stats.dirs += 1;
        } else {
            stats.files += 1;
        }
        batch.push((segments, new_record(name, meta, is_dir)));

        if batch.len() >= PROGRESS_BATCH {
            flush_subtree_batch(store, &mut batch, stored)?;
        }
    }

    flush_subtree_batch(store, &mut batch, stored)?;
    Ok(stats)
}

/// Upserts and clears one buffered [`scan_subtree`] batch on the calling
/// thread, reporting every persisted row through `stored`.
fn flush_subtree_batch(
    store: &mut Store,
    batch: &mut Vec<(Vec<String>, FileRecord)>,
    stored: &mut dyn FnMut(&Store, &FileRecord),
) -> Result<()> {
    for (segments, mut rec) in batch.drain(..) {
        let dir_path: Vec<&str> = segments.iter().map(String::as_str).collect();
        store.upsert_file(&dir_path, &mut rec)?;
        stored(store, &rec);
    }
    Ok(())
}

/// Upserts and clears one buffered batch on the calling thread.
/// One flush = ONE transaction: the batch's upserts commit atomically
/// (crash mid-batch loses at most this batch's rows — the engine's
/// half-finished-scan reconciliation story is batch-granular anyway), and
/// the journal churn is amortized over the batch instead of paid per file.
/// Task 20 measurement: per-row autocommit put a 100k-file first scan at
/// ~113 files/s on this machine — journal create/commit/delete per row,
/// further taxed by on-access scanning — against the 3300 files/s budget;
/// the single transaction per 1000-row batch is the fix.
fn flush_batch(store: &mut Store, batch: &mut Vec<(Vec<String>, FileRecord)>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = store.conn_mut().transaction()?;
    for (segments, mut rec) in batch.drain(..) {
        let dir_path: Vec<&str> = segments.iter().map(String::as_str).collect();
        crate::store::upsert_file_on(&tx, &dir_path, &mut rec)?;
    }
    tx.commit()?;
    Ok(())
}

/// `opts.roots`, or the mount points of all fixed volumes when empty.
/// Public so the engine resolves the exact same effective roots when it
/// purges each root's index subtree before a rescan.
pub fn effective_roots(roots: &[String]) -> Vec<String> {
    if roots.is_empty() {
        crate::volumes::fixed_volumes()
            .into_iter()
            .map(|volume| volume.path)
            .collect()
    } else {
        roots.to_vec()
    }
}

/// Compiles the exclusion patterns; empty patterns list matches nothing.
/// `pub(crate)`: the watcher (Task 8) compiles the same matcher so events
/// under excluded paths are ignored exactly like the scan prunes them.
pub(crate) fn build_matcher(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSet::builder();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|err| {
            SynsoError::InvalidPattern(format!("bad exclusion pattern {pattern:?}: {err}"))
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|err| SynsoError::InvalidPattern(format!("bad exclusion patterns: {err}")))
}

/// Exclusion test for one path: globset patterns are matched against the
/// forward-slash absolute path, the forward-slash path relative to the scan
/// root, and each individual relative path segment (so a bare
/// `node_modules` pattern prunes directories of that name at any depth).
/// Both paths are reduced to their ordinary (non-verbatim) spelling first,
/// since patterns are written against paths like `C:/...`, never `\\?\...`.
///
/// `pub(crate)`: the watcher (Task 8) applies the same test to event paths.
pub(crate) fn excluded(matcher: &GlobSet, abs: &Path, root: &Path) -> bool {
    if matcher.is_empty() {
        return false;
    }
    let abs = strip_verbatim_prefix(abs);
    let root = strip_verbatim_prefix(root);
    let abs_str = abs.to_string_lossy().replace('\\', "/");
    if matcher.is_match(&abs_str) {
        return true;
    }
    let Ok(rel) = abs.strip_prefix(&root) else {
        return false;
    };
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if matcher.is_match(&rel_str) {
        return true;
    }
    rel.components()
        .any(|segment| matcher.is_match(&*segment.as_os_str().to_string_lossy()))
}

/// The single `dirs`-table segment representing the scan root itself:
/// entries directly under the root live in this directory row. Uses the
/// final path component when there is one (`C:\data` -> `data`), otherwise
/// the trimmed root string (`C:\` -> `C:`, `/` -> `/`); verbatim prefixes
/// are stripped first so segments stay clean (`C:`, not `\\?\C:`).
///
/// Callers should pass non-overlapping roots: two roots sharing a final
/// path component (e.g. `C:\a` and `D:\a`) merge into the same `dirs`
/// branch.
///
/// Public because the engine reuses this to compute the subtree it purges
/// from the index before rescanning a root (`delete_subtree(&[segment])`).
pub fn root_segment(root: &Path) -> String {
    let clean = strip_verbatim_prefix(root);
    if let Some(name) = clean.file_name() {
        return name.to_string_lossy().into_owned();
    }
    let full = clean.to_string_lossy();
    let trimmed = full.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        full.into_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Traversal form of a scan root: on Windows the `\\?\`-verbatim path
/// (`\\?\C:\...`, UNC `\\server\share\...` -> `\\?\UNC\server\share\...`)
/// so directory reads and stats are not bound by the 260-char MAX_PATH
/// limit on hosts without LongPathsEnabled. Already-verbatim and relative
/// roots are returned unchanged (verbatim paths must be absolute).
#[cfg(windows)]
fn traversal_root(root: &Path) -> PathBuf {
    let original = root.to_string_lossy();
    if original.starts_with(r"\\?\") || !root.is_absolute() {
        return root.to_path_buf();
    }
    // The verbatim prefix turns off Win32 path malleability, so spell the
    // path with backslashes explicitly.
    let normalized = original.replace('/', r"\");
    if let Some(rest) = normalized.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{rest}"))
    } else {
        PathBuf::from(format!(r"\\?\{normalized}"))
    }
}

#[cfg(not(windows))]
fn traversal_root(root: &Path) -> PathBuf {
    root.to_path_buf()
}

/// Removes a `\\?\` / `\\?\UNC\` verbatim prefix, yielding the ordinary
/// path spelling used for glob matching and `dirs`-table segments.
#[cfg(windows)]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest.to_owned())
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Unix-epoch seconds for a stat timestamp; 0 when unavailable (pre-epoch
/// times or unsupported fields, e.g. `created` on some Linux filesystems).
/// `pub(crate)`: the watcher (Task 8) stats fresh records the same way.
pub(crate) fn unix_secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// True for cloud-placeholder attribute bits:
/// `FILE_ATTRIBUTE_OFFLINE` (0x1000) or
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` (0x400000). Consumed by dedup
/// (Task 9) so placeholders are never read; the scanner itself does not
/// use it — placeholders are indexed like any other file.
pub fn is_placeholder_attr(attrs: u32) -> bool {
    attrs & (0x1000 | 0x400000) != 0
}

/// Raw Windows attribute bits of `path` via `GetFileAttributesW`, or `None`
/// when the path cannot be statted (vanished mid-walk, inaccessible).
/// Shared by the scanner's reparse guard and dedup's placeholder skip
/// (Task 9), so both agree on one attribute source.
#[cfg(windows)]
pub(crate) fn file_attributes(path: &Path) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let attrs = unsafe { GetFileAttributesW(PCWSTR::from_raw(wide.as_ptr())) };
    (attrs != INVALID_FILE_ATTRIBUTES).then_some(attrs)
}

/// Non-Windows stand-in: no cloud-placeholder concept, zero attribute bits
/// (`is_placeholder_attr(0)` is false, so nothing is ever skipped as one).
#[cfg(not(windows))]
pub(crate) fn file_attributes(_path: &Path) -> Option<u32> {
    Some(0)
}

/// True when `path` itself is a symlink/junction, using stat metadata.
fn is_reparse_point(path: &Path) -> bool {
    has_reparse_attr(path)
        || path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
}

/// True when `path` carries `FILE_ATTRIBUTE_REPARSE_POINT` (0x400):
/// symlink, junction, mount point or app-exec link, queried through
/// [`file_attributes`]. An unreadable path (vanished mid-scan) reports
/// false and is later counted as an error when its metadata prefetch fails.
#[cfg(windows)]
fn has_reparse_attr(path: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    file_attributes(path).is_some_and(|attrs| attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

#[cfg(not(windows))]
fn has_reparse_attr(_path: &Path) -> bool {
    // std's `is_symlink()` covers the only reparse mechanism here.
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use rusqlite::Connection;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    /// Fresh store in its own tempdir: `(tempdir, db_path, store)`. Keep the
    /// tempdir alive for the whole test.
    fn test_store() -> (tempfile::TempDir, PathBuf, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("index.db");
        let store = Store::open(&db).unwrap();
        (tmp, db, store)
    }

    fn opts_for(root: &Path) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_string_lossy().into_owned()],
            exclusions: Vec::new(),
        }
    }

    /// `(name, size, mtime, ext_id, is_dir)` of every file row, by name.
    fn all_rows(db: &Path) -> Vec<(String, u64, i64, i64, bool)> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare("SELECT name, size, mtime, ext_id, is_dir FROM files ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    }

    /// The interned extension text for `ext_id` (`None` for sentinel 0).
    fn ext_text(db: &Path, ext_id: i64) -> Option<String> {
        if ext_id == 0 {
            return None;
        }
        Connection::open(db)
            .unwrap()
            .query_row("SELECT ext FROM extensions WHERE id = ?1", [ext_id], |r| {
                r.get(0)
            })
            .ok()
    }

    fn fs_mtime(path: &Path) -> i64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// On Windows, the `\\?\`-verbatim form of `path` so that fixtures may
    /// exceed 260 chars and use reserved device names (`CON`, `NUL`) via
    /// plain `std::fs`; unix needs no translation.
    #[cfg(windows)]
    fn longpath_form(path: &Path) -> PathBuf {
        let s = path.to_string_lossy();
        if s.starts_with("\\\\?\\") {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("\\\\?\\{s}"))
        }
    }

    #[cfg(not(windows))]
    fn longpath_form(path: &Path) -> PathBuf {
        path.to_path_buf()
    }

    /// A six-level chain whose total path length exceeds 260 characters
    /// (each level ~64 chars).
    fn deep_chain(root: &Path) -> PathBuf {
        let mut deep = root.to_path_buf();
        for i in 0..6 {
            deep = deep.join(format!("lvl{i}_{}", "d".repeat(58)));
        }
        deep
    }

    #[test]
    fn scan_records_fixture_tree() {
        let (tmp, db, mut store) = test_store();
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("docs").join("reports")).unwrap();
        fs::write(root.join("readme.md"), b"hello").unwrap();
        fs::write(root.join("docs").join("Notes.TXT"), b"0123456789").unwrap();
        fs::write(root.join("docs").join("reports").join("Final.PDF"), b"x").unwrap();
        fs::create_dir(root.join("empty_dir")).unwrap();

        let progress_calls = Mutex::new(Vec::new());
        let stats = scan(&mut store, &opts_for(&root), &|n| {
            progress_calls.lock().unwrap().push(n)
        })
        .unwrap();

        assert_eq!(stats.files, 3, "readme.md, Notes.TXT, Final.PDF");
        assert_eq!(
            stats.dirs, 3,
            "docs, docs/reports, empty_dir (the root itself is not counted)"
        );
        assert_eq!(stats.errors, 0);
        assert_eq!(
            progress_calls.lock().unwrap().last(),
            Some(&3),
            "final progress call reports the files-done count"
        );

        let rows = all_rows(&db);
        let find = |name: &str| {
            rows.iter()
                .find(|r| r.0 == name)
                .unwrap_or_else(|| panic!("{name} must be recorded"))
        };

        let (name, size, mtime, ext_id, is_dir) = find("readme.md");
        assert_eq!((name.as_str(), *size, *is_dir), ("readme.md", 5, false));
        assert_eq!(*mtime, fs_mtime(&root.join("readme.md")));
        assert_eq!(ext_text(&db, *ext_id).as_deref(), Some("md"));

        let (_, size, mtime, ext_id, is_dir) = find("Notes.TXT");
        assert_eq!((*size, *is_dir), (10, false));
        assert_eq!(*mtime, fs_mtime(&root.join("docs").join("Notes.TXT")));
        assert_eq!(
            ext_text(&db, *ext_id).as_deref(),
            Some("txt"),
            "uppercase extension is interned lowercase without a dot"
        );

        let (_, size, _, ext_id, is_dir) = find("Final.PDF");
        assert_eq!((*size, *is_dir), (1, false));
        assert_eq!(ext_text(&db, *ext_id).as_deref(), Some("pdf"));

        let (name, size, _, ext_id, is_dir) = find("empty_dir");
        assert_eq!(
            (name.as_str(), *size, *ext_id, *is_dir),
            ("empty_dir", 0, 0, true)
        );

        assert_eq!(rows.len(), 6, "3 files + 3 dirs, nothing else");
    }

    #[test]
    fn scan_respects_exclusions() {
        let (tmp, db, mut store) = test_store();
        let root = tmp.path().join("proj");
        fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
        fs::create_dir_all(root.join("sub").join("node_modules")).unwrap();
        fs::create_dir_all(root.join("old.tmp")).unwrap();
        fs::write(root.join("keep.txt"), b"k").unwrap();
        fs::write(root.join("scratch.tmp"), b"s").unwrap();
        fs::write(root.join("node_modules").join("pkg").join("x.js"), b"").unwrap();
        fs::write(root.join("sub").join("node_modules").join("y.js"), b"").unwrap();
        fs::write(root.join("old.tmp").join("inside.txt"), b"").unwrap();
        fs::write(root.join("sub").join("ok.rs"), b"").unwrap();

        let opts = ScanOptions {
            roots: vec![root.to_string_lossy().into_owned()],
            exclusions: vec!["*.tmp".into(), "node_modules".into()],
        };
        let stats = scan(&mut store, &opts, &|_| {}).unwrap();

        assert_eq!(stats.files, 2, "keep.txt and sub/ok.rs survive");
        assert_eq!(stats.dirs, 1, "only sub/ remains as a directory");
        assert_eq!(stats.errors, 0);

        let rows = all_rows(&db);
        let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        for excluded_name in [
            "scratch.tmp",
            "old.tmp",
            "inside.txt",
            "node_modules",
            "x.js",
            "y.js",
        ] {
            assert!(
                !names.contains(&excluded_name),
                "{excluded_name} must be pruned"
            );
        }
        assert!(names.contains(&"keep.txt"));
        assert!(names.contains(&"ok.rs"));
    }

    #[test]
    fn scan_handles_unicode_longpaths_reserved() {
        let (tmp, db, mut store) = test_store();
        // This fixture scans through an explicitly verbatim root (kept to
        // cover user-supplied `\\?\` roots); the plain-root long-path case
        // has its own test below.
        let root = longpath_form(&tmp.path().join("u"));

        fs::create_dir_all(root.join("文档")).unwrap();
        fs::write(root.join("文档").join("报告.docx"), b"d").unwrap();
        fs::create_dir_all(root.join("🎉party")).unwrap();
        fs::write(root.join("🎉party").join("emoji🎊.txt"), b"e").unwrap();
        fs::write(root.join("CON"), b"c").unwrap();
        fs::write(root.join("NUL"), b"n").unwrap();

        // Deep chain whose total path exceeds 260 characters.
        let deep = deep_chain(&root);
        fs::create_dir_all(&deep).unwrap();
        let deep_file = deep.join("deep.txt");
        fs::write(&deep_file, b"deep").unwrap();
        assert!(
            deep_file.as_os_str().len() > 260,
            "fixture must exceed MAX_PATH, got {}",
            deep_file.as_os_str().len()
        );

        let stats = scan(&mut store, &opts_for(&root), &|_| {}).unwrap();

        assert_eq!(
            stats.errors, 0,
            "unicode/reserved/long names are not errors"
        );
        assert_eq!(stats.files, 5, "报告.docx, emoji🎊.txt, CON, NUL, deep.txt");
        assert_eq!(stats.dirs, 8, "文档, 🎉party plus the 6 deep levels");

        let rows = all_rows(&db);
        let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        for expected in ["报告.docx", "emoji🎊.txt", "CON", "NUL", "deep.txt"] {
            assert!(names.contains(&expected), "{expected} must be recorded");
        }
    }

    /// Regression: a plain (non-verbatim) root — the form `fixed_volumes()`
    /// supplies — must still index paths deeper than 260 chars. The scan
    /// normalizes the root to its verbatim form for traversal, so this
    /// holds even on hosts without LongPathsEnabled (fixture creation goes
    /// through the verbatim form for the same reason).
    #[test]
    fn scan_long_path_under_plain_root() {
        let (tmp, db, mut store) = test_store();
        let plain_root = tmp.path().join("plain");

        let deep = deep_chain(&longpath_form(&plain_root));
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), b"deep").unwrap();

        let plain_deep_file = deep_chain(&plain_root).join("deep.txt");
        assert!(
            plain_deep_file.as_os_str().len() > 260,
            "fixture must exceed MAX_PATH, got {}",
            plain_deep_file.as_os_str().len()
        );

        let stats = scan(&mut store, &opts_for(&plain_root), &|_| {}).unwrap();

        assert_eq!(stats.errors, 0, "the deep path must not be an error");
        assert_eq!(stats.files, 1, "only deep.txt exists");
        assert_eq!(stats.dirs, 6, "the six chain levels");

        let rows = all_rows(&db);
        assert!(
            rows.iter().any(|r| r.0 == "deep.txt"),
            "the >260-char path under a plain root must be recorded"
        );
    }

    #[cfg(windows)]
    #[test]
    fn scan_terminates_on_junction_cycle() {
        let (tmp, db, mut store) = test_store();
        let root = tmp.path().join("tree");
        fs::create_dir_all(root.join("dir").join("sub")).unwrap();
        fs::write(root.join("dir").join("a.txt"), b"a").unwrap();
        fs::write(root.join("dir").join("sub").join("b.txt"), b"b").unwrap();
        fs::write(root.join("top.txt"), b"t").unwrap();

        // Baseline: counts without the junction.
        let baseline = scan(&mut store, &opts_for(&root), &|_| {}).unwrap();
        assert_eq!((baseline.files, baseline.dirs, baseline.errors), (3, 2, 0));

        // Junction pointing at an ancestor directory: dir/loop -> dir.
        junction::create(root.join("dir"), root.join("dir").join("loop")).unwrap();

        // A fresh store for the second scan; must terminate with identical
        // counts and must not index the junction itself.
        let (_tmp2, db2, mut store2) = test_store();
        let cycled = scan(&mut store2, &opts_for(&root), &|_| {}).unwrap();
        assert_eq!(
            cycled.files, baseline.files,
            "the junction cycle must not change the file count"
        );
        assert_eq!(cycled.dirs, baseline.dirs);
        assert_eq!(cycled.errors, 0);

        let rows2 = all_rows(&db2);
        let names: Vec<&str> = rows2.iter().map(|r| r.0.as_str()).collect();
        assert!(
            !names.contains(&"loop"),
            "the junction entry is not indexed"
        );
        assert_eq!(names.len(), 5, "same rows as the baseline scan");
        let _ = (db, tmp);
    }

    #[test]
    fn is_placeholder_attr_bits() {
        const OFFLINE: u32 = 0x1000; // FILE_ATTRIBUTE_OFFLINE
        const RECALL_ON_DATA: u32 = 0x400_000; // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS

        assert!(!is_placeholder_attr(0));
        assert!(!is_placeholder_attr(0x20)); // FILE_ATTRIBUTE_ARCHIVE alone
        assert!(!is_placeholder_attr(0x800)); // unrelated bit
        assert!(is_placeholder_attr(OFFLINE));
        assert!(is_placeholder_attr(RECALL_ON_DATA));
        assert!(is_placeholder_attr(OFFLINE | RECALL_ON_DATA));
        assert!(is_placeholder_attr(0x20 | OFFLINE)); // set alongside normal bits
    }

    #[test]
    fn scan_progress_reports_files_done() {
        let (tmp, _db, mut store) = test_store();
        let root = tmp.path().join("many");
        fs::create_dir_all(&root).unwrap();
        for i in 0..1002 {
            fs::write(root.join(format!("f{i:04}.txt")), b"").unwrap();
        }

        let calls = Mutex::new(Vec::new());
        let stats = scan(&mut store, &opts_for(&root), &|n| {
            calls.lock().unwrap().push(n)
        })
        .unwrap();

        assert_eq!(stats.files, 1002);
        assert_eq!(stats.errors, 0);
        assert_eq!(
            calls.lock().unwrap().first(),
            Some(&1000),
            "first batch progress fires after 1000 records"
        );
        assert_eq!(
            calls.lock().unwrap().last(),
            Some(&1002),
            "final progress reports the total"
        );
    }
}
