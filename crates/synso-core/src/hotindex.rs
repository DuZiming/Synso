//! Hot index: the in-memory mirror of the store that search runs against
//! (Task 4). The engine bulk-loads it from SQLite at startup and swaps in a
//! fresh copy after every completed scan — wholesale rebuild is the rescan
//! correctness story; between scans, the filesystem watcher (Task 8) applies
//! incremental [`HotIndex::upsert_slot`] / [`HotIndex::remove_by_dir_name`]
//! edits, so a live index never needs a per-batch reload.
//!
//! Shape: an interned directory tree keyed by the database's `dirs` row
//! ids (hot-index dir ids and store dir ids are the same numbers), plus
//! one dense slot per `files` row whose columns live in parallel vectors.
//!
//! Memory design (the arena refactor): ALL string content — lowercased
//! names, original names, both pinyin keys, dir names — lives in ONE
//! contiguous `Vec<u8>` arena, addressed per slot by `(start, len)`
//! refs. Pinyin keys that equal the lowercased name (every pure-ASCII
//! name — the overwhelming majority) alias the name's ref instead of
//! storing a second copy, and the secondary `(parent, name)` indexes key
//! on a 64-bit name hash (verified against the arena on read) rather
//! than duplicating the name string. At 1.6M files this cut true process
//! RSS from ~976MB (millions of individual `String` allocations plus
//! index-key copies) to a fraction of it; `interned_bytes` keeps
//! reporting the LOGICAL content (alias bytes counted once) for the
//! budget test.
//!
//! Visibility split (Task 20 facade audit): the type re-exports publicly
//! for readers (search/harness embedders), but every WRITE path —
//! [`HotIndex::upsert_slot`], [`HotIndex::remove_by_dir_name`] and the
//! store-backed [`HotIndex::load`] — stays `pub(crate)`: the engine owns
//! index mutation (scan reload / watcher batches / trash), and the
//! locked facade must not hand embedders write access to it. The public
//! constructors build STANDALONE indexes only.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::MAIN_SEPARATOR;

use crate::model::{FileId, FileRecord};
use crate::store::Store;
use crate::Result;

/// A string's location in the shared byte arena.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StrRef {
    start: u32,
    len: u32,
}

/// 64-bit hash of a name, used as the string-free key component of both
/// secondary `(parent, name)` indexes. Collisions between two DIFFERENT
/// names under the SAME parent would need a 64-bit birthday hit within a
/// handful of siblings (≈2⁻⁶⁴ per pair); every read still VERIFIES the
/// arena bytes, so a collision degrades that one lookup to "not found"
/// until the next wholesale reload — it can never return a wrong entry.
fn name_hash(name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// One interned `dirs` row: parent dir id (`None` for root segments) and
/// the original-case directory name (an arena ref).
#[derive(Debug)]
struct DirEntry {
    parent: Option<FileId>,
    name: StrRef,
}

// --------------------- Task 20: synthetic perf fixture -------------------

/// Deterministic xorshift64* for [`HotIndex::synthetic`] — the fixture must
/// be reproducible run to run (and dependency-free), so benchmark runs
/// compare like for like. Every bound at the call sites is a nonzero
/// constant.
struct SyntheticRng(u64);

impl SyntheticRng {
    fn below(&mut self, bound: u64) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) % bound
    }
}

/// Latin basename pool for the synthetic corpus ("report123456.pdf").
const SYNTHETIC_LATIN: [&str; 20] = [
    "report", "invoice", "photo", "song", "backup", "meeting", "notes", "design", "budget",
    "resume", "manual", "spec", "draft", "scan", "cover", "letter", "contract", "summary",
    "archive", "export",
];

/// Han basename pool for the synthetic corpus ("报告123456.pdf"); pinyin
/// keys are computed from these by the production generator, so 「bg」 and
/// 「baog」 probes match exactly the rows a real 报告-named corpus would.
const SYNTHETIC_HAN: [&str; 24] = [
    "报告", "会议", "计划", "笔记", "截图", "视频", "音乐", "图片", "资料", "合同", "发票", "简历",
    "项目", "总结", "备份", "数据", "模板", "通知", "日志", "方案", "财务", "人事", "培训", "调研",
];

/// Extension pool for the synthetic corpus; "pdf" is index 0 so the
/// `ext:pdf size:>1mb` probe filters through the interned-id fast path.
const SYNTHETIC_EXTS: [&str; 14] = [
    "pdf", "docx", "txt", "jpg", "png", "mp3", "mp4", "zip", "xlsx", "pptx", "md", "csv", "wav",
    "mov",
];

/// Root segments of the synthetic dir tree; "projects" is the `path:`
/// probe's target (no other segment contains that substring).
const SYNTHETIC_ROOTS: [&str; 8] = [
    "projects",
    "docs",
    "media",
    "downloads",
    "work",
    "archive",
    "backup",
    "photos",
];

/// Subdirectory name pool for the synthetic tree (numerically suffixed per
/// parent, so sibling names stay unique).
const SYNTHETIC_SUBDIRS: [&str; 14] = [
    "src", "docs", "assets", "drafts", "final", "old", "new", "备份", "资料", "temp", "export",
    "raw", "edit", "out",
];

/// mtime base of the synthetic corpus (2026-01-01T00:00:00Z); per-entry
/// mtimes spread deterministically over the following ~11 months.
const SYNTHETIC_MTIME_BASE: i64 = 1_767_225_600;

/// One synthetic corpus name for ordinal `i` (unique per dir because `i`
/// rides along): 40% latin, 30% Han, 20% mixed, 10% extension-less /
/// multi-dot oddballs — the name shapes the query probes target.
fn synthetic_name(i: u64) -> String {
    let ext = SYNTHETIC_EXTS[(i % SYNTHETIC_EXTS.len() as u64) as usize];
    let latin = SYNTHETIC_LATIN[(i % SYNTHETIC_LATIN.len() as u64) as usize];
    let han = SYNTHETIC_HAN[(i % SYNTHETIC_HAN.len() as u64) as usize];
    match i % 10 {
        0..=3 => format!("{latin}{i}.{ext}"),
        4..=6 => format!("{han}{i}.{ext}"),
        7..=8 => format!("{latin}-{han}{i}.{ext}"),
        _ => match i % 4 {
            0 => format!("README_{i}"),
            1 => format!("makefile_{i}"),
            2 => format!("数据备份{i}"),
            _ => format!("archive_{i}.tar.gz"),
        },
    }
}

/// One synthetic `files` row (pinyin keys from the production generator,
/// exactly as [`crate::store::Store::upsert_file`] would store them).
fn synthetic_record(
    id: FileId,
    parent: FileId,
    name: &str,
    size: u64,
    mtime: i64,
    ext_id: FileId,
    is_dir: bool,
) -> FileRecord {
    let (py_full, py_init) = crate::pinyin::pinyin_keys(name);
    FileRecord {
        id,
        parent,
        name: name.to_string(),
        size: if is_dir { 0 } else { size },
        mtime,
        ctime: mtime,
        atime: mtime,
        ext_id: if is_dir { 0 } else { ext_id },
        is_dir,
        hash_first: None,
        hash_full: None,
        py_full,
        py_init,
    }
}

/// In-memory search index over the store (see module docs).
#[derive(Debug, Default)]
pub struct HotIndex {
    /// Interned directory tree, keyed by the store's `dirs` row ids.
    dirs: HashMap<FileId, DirEntry>,
    /// Secondary index over `dirs`: (parent id, name hash) -> dir id,
    /// EXACT-case semantics like the store's `UNIQUE(parent, name)` —
    /// lookups verify the arena bytes (see [`name_hash`]).
    dir_by_parent_name: HashMap<(Option<FileId>, u64), FileId>,
    /// Secondary index: `files` row id -> slot, for incremental upserts of
    /// rows that already hold a slot (same id = update in place).
    slot_by_id: HashMap<FileId, usize>,
    /// Secondary index: (containing dir id, name hash) -> slot, EXACT-case
    /// (see [`dir_by_parent_name`]), verified against the arena on read.
    slot_by_parent_name: HashMap<(FileId, u64), usize>,
    /// THE byte arena: every string the index holds (lowercased names,
    /// original names, non-aliased pinyin keys, dir names) as one
    /// contiguous buffer — no per-string allocation, no allocator
    /// overhead, no fragmentation.
    bytes: Vec<u8>,
    /// Lowercased name per slot (arena ref; case-insensitive matching).
    lower_refs: Vec<StrRef>,
    /// Original-case name per slot (arena ref; display, rebuilds).
    orig_refs: Vec<StrRef>,
    /// Full-pinyin key per slot (arena ref; aliases the lowercased name
    /// for pure-ASCII names — pinyin_keys returns the lowercase name
    /// itself there).
    py_full_refs: Vec<StrRef>,
    /// First-letter pinyin key per slot (arena ref; aliases like
    /// [`HotIndex::py_full_refs`] — pure-ASCII initials are the full
    /// lowercase letters).
    py_init_refs: Vec<StrRef>,
    /// Logical content bytes per slot (lower + original + non-aliased
    /// pinyin), kept so `interned_bytes` stays exact through incremental
    /// updates without re-measuring refs.
    logical_bytes: Vec<u32>,
    /// `files` row id per slot (search hits expose the store id, which
    /// later tasks key on; the slot index itself is internal).
    ids: Vec<FileId>,
    /// Containing directory's `dirs` row id per slot.
    parents: Vec<FileId>,
    /// File size in bytes per slot (0 for directory entries).
    sizes: Vec<u64>,
    /// mtime in Unix-epoch seconds per slot.
    mtimes: Vec<i64>,
    /// Interned extension id per slot (0 = no extension).
    ext_ids: Vec<i64>,
    /// Extension text per ext id, loaded from the `extensions` table
    /// (`Hit.ext`); ext id 0 (no extension) has no row.
    exts: HashMap<FileId, String>,
    /// Directory-entry flag per slot.
    is_dirs: Vec<bool>,
    /// Non-directory slot count, cached for O(1) stats.
    file_count: u64,
    /// Directory-entry slot count (`files` rows with `is_dir = 1`, matching
    /// what `ScanStats::dirs` counts — not the `dirs`-table row count).
    dir_count: u64,
    /// Bytes held by the arenas: name lowercase + name original +
    /// non-aliased pinyin keys + dir names.
    interned_bytes: u64,
    /// True once an initial full load completed.
    warm: bool,
}

impl HotIndex {
    /// Appends `s` to the byte arena and returns its ref.
    fn put(&mut self, s: &str) -> StrRef {
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        StrRef {
            start,
            len: s.len() as u32,
        }
    }

    /// The arena slice a ref names (valid UTF-8 by construction — only
    /// `&str` input ever enters the arena; the `unwrap_or` keeps the
    /// library's no-panic rule even against hypothetical corruption).
    #[inline]
    fn str_of(&self, r: StrRef) -> &str {
        let end = (r.start + r.len) as usize;
        self.bytes
            .get(r.start as usize..end)
            .and_then(|slice| std::str::from_utf8(slice).ok())
            .unwrap_or("")
    }
    /// The empty, not-yet-loaded index an engine starts with.
    pub fn cold() -> HotIndex {
        HotIndex::default()
    }

    // ------------------- Task 20: synthetic perf fixture -----------------
    //
    // The validation harness (benches/search.rs, tests/perf.rs) times the
    // query pipeline against a million-entry index — a size no test fixture
    // can reach through the real scan path (a million store upserts). This
    // constructor builds the same shape in memory instead: a deterministic
    // mixed CJK/latin corpus over a small directory tree, pinyin keys from
    // the production generator, so budget numbers measure the real arena
    // layout and the real match targets.

    /// A warm, deterministic, fully in-memory index holding `n` file
    /// entries (plus the directory entries of a ~170-dir synthetic tree)
    /// of mixed CJK/latin names. NOT part of the store-backed lifecycle:
    /// no `Store` is consulted, ids come from internal counters, and every
    /// draw comes from a fixed-seed PRNG, so repeated runs build byte-
    /// identical indexes and benchmarks compare like for like. Pinyin keys
    /// go through [`crate::pinyin::pinyin_keys`] — the same generator the
    /// store uses at upsert time — so the arenas hold exactly the spellings
    /// a scanned corpus would. Names embed their ordinal, keeping
    /// `(dir, name)` unique as the corpus scales.
    pub fn synthetic(n: u64) -> HotIndex {
        let mut hot = HotIndex {
            warm: true,
            ..HotIndex::default()
        };
        let mut rng = SyntheticRng(0x53_59_4E_53_4F_00_00_01); // "SYNSO"
        let mut next_dir_id: FileId = 1;
        let mut next_file_id: FileId = 1;

        for (idx, ext) in SYNTHETIC_EXTS.iter().enumerate() {
            hot.exts.insert(idx as FileId + 1, ext.to_string());
        }

        // The interned dir tree plus each directory's own entry slot. The
        // walk root segments get no slot (production scans never index the
        // root itself — min_depth(1)); every nested dir does, mirroring how
        // a scan stores directories as `files` rows with `is_dir = 1`.
        // `file_dirs` collects the dirs files are distributed over.
        let mut file_dirs: Vec<FileId> = Vec::new();
        let mut intern_dir = |hot: &mut HotIndex,
                              parent: Option<FileId>,
                              name: &str,
                              next_dir_id: &mut FileId,
                              next_file_id: &mut FileId| {
            let id = *next_dir_id;
            *next_dir_id += 1;
            hot.intern_dir(id, parent, name.to_string());
            if let Some(parent) = parent {
                let rec = synthetic_record(*next_file_id, parent, name, 0, 0, 0, true);
                *next_file_id += 1;
                hot.push(rec);
            }
            file_dirs.push(id);
            id
        };

        for (r, root) in SYNTHETIC_ROOTS.iter().enumerate() {
            let root_id = intern_dir(&mut hot, None, root, &mut next_dir_id, &mut next_file_id);
            for k in 0..3 + rng.below(4) {
                let name = format!(
                    "{}{k}",
                    SYNTHETIC_SUBDIRS[(r * 5 + k as usize) % SYNTHETIC_SUBDIRS.len()]
                );
                let sub_id = intern_dir(
                    &mut hot,
                    Some(root_id),
                    &name,
                    &mut next_dir_id,
                    &mut next_file_id,
                );
                if rng.below(2) == 0 {
                    for g in 0..1 + rng.below(2) {
                        let gname = format!("deep{g}_{k}");
                        intern_dir(
                            &mut hot,
                            Some(sub_id),
                            &gname,
                            &mut next_dir_id,
                            &mut next_file_id,
                        );
                    }
                }
            }
        }
        // One deeper chain under the first root (projects) so path lengths
        // span a realistic depth range 2..=5.
        let mut parent = 1;
        for seg in ["alpha", "src", "include"] {
            parent = intern_dir(
                &mut hot,
                Some(parent),
                seg,
                &mut next_dir_id,
                &mut next_file_id,
            );
        }

        for i in 0..n {
            let parent = file_dirs[(i as usize) % file_dirs.len()];
            let name = synthetic_name(i);
            let rec = synthetic_record(
                next_file_id,
                parent,
                &name,
                1024u64 << rng.below(14), // 1 KiB .. 8 MiB
                SYNTHETIC_MTIME_BASE + ((i as i64 * 37) % 29_030_400),
                (i % SYNTHETIC_EXTS.len() as u64) as FileId + 1,
                false,
            );
            next_file_id += 1;
            hot.push(rec);
        }
        hot
    }

    /// Interns one synthetic `dirs` row (the shared shape of
    /// [`HotIndex::load`]'s dir loop).
    fn intern_dir(&mut self, id: FileId, parent: Option<FileId>, name: String) {
        let name_ref = self.put(&name);
        self.interned_bytes += name.len() as u64;
        self.dir_by_parent_name
            .insert((parent, name_hash(&name)), id);
        self.dirs.insert(
            id,
            DirEntry {
                parent,
                name: name_ref,
            },
        );
    }

    /// Loads the full `dirs`, `extensions` and `files` tables from `store`
    /// into a fresh, warm index. Whole-table read by design (see module
    /// docs).
    pub(crate) fn load(store: &Store) -> Result<HotIndex> {
        let mut index = HotIndex {
            warm: true,
            ..HotIndex::default()
        };
        for (id, parent, name) in store.all_dirs()? {
            let name_ref = index.put(&name);
            index.interned_bytes += name.len() as u64;
            index
                .dir_by_parent_name
                .insert((parent, name_hash(&name)), id);
            index.dirs.insert(
                id,
                DirEntry {
                    parent,
                    name: name_ref,
                },
            );
        }
        for (id, ext) in store.all_extensions()? {
            index.exts.insert(id, ext);
        }
        for rec in store.all_files()? {
            index.push(rec);
        }
        Ok(index)
    }

    /// Appends one `files` row's columns to the arenas and secondary
    /// indexes (whole-table load and incremental watcher pushes).
    /// Pinyin keys equal to the lowercased name (every pure-ASCII name)
    /// alias the name's ref — no second copy in the arena.
    fn push(&mut self, rec: FileRecord) {
        let lower = rec.name.to_lowercase();
        let lower_ref = self.put(&lower);
        let orig_ref = self.put(&rec.name);
        let py_full_ref = if rec.py_full == lower {
            lower_ref
        } else {
            self.put(&rec.py_full)
        };
        let py_init_ref = if rec.py_init == lower {
            lower_ref
        } else {
            self.put(&rec.py_init)
        };
        let logical = (lower.len()
            + rec.name.len()
            + if py_full_ref == lower_ref {
                0
            } else {
                rec.py_full.len()
            }
            + if py_init_ref == lower_ref {
                0
            } else {
                rec.py_init.len()
            }) as u64;
        self.interned_bytes += logical;
        if rec.is_dir {
            self.dir_count += 1;
        } else {
            self.file_count += 1;
        }
        let slot = self.lower_refs.len();
        self.slot_by_id.insert(rec.id, slot);
        self.slot_by_parent_name
            .insert((rec.parent, name_hash(&rec.name)), slot);
        self.lower_refs.push(lower_ref);
        self.orig_refs.push(orig_ref);
        self.py_full_refs.push(py_full_ref);
        self.py_init_refs.push(py_init_ref);
        self.logical_bytes.push(logical as u32);
        self.ids.push(rec.id);
        self.parents.push(rec.parent);
        self.sizes.push(rec.size);
        self.mtimes.push(rec.mtime);
        self.ext_ids.push(rec.ext_id);
        self.is_dirs.push(rec.is_dir);
    }

    /// Incremental upsert (watcher path, Task 8). `rec` must come straight
    /// from [`Store::upsert_file`], so `id`, `parent`, `ext_id` and both
    /// pinyin keys hold the stored values. Interns the row's ancestor
    /// `dirs` chain (path reconstruction) and extension text from `store`
    /// on demand, then updates the existing slot when the row id already
    /// holds one, or appends a new slot otherwise. Store errors abort this
    /// row's hot update (logged by the caller); the store itself stays
    /// authoritative and the next wholesale reload resyncs.
    pub(crate) fn upsert_slot(&mut self, store: &Store, rec: &FileRecord) -> Result<()> {
        // Ancestor chain first, so the new/updated slot's paths resolve.
        for (id, parent, name) in store.dir_ancestors(rec.parent)? {
            if !self.dirs.contains_key(&id) {
                let name_ref = self.put(&name);
                self.interned_bytes += name.len() as u64;
                self.dir_by_parent_name
                    .insert((parent, name_hash(&name)), id);
                self.dirs.insert(
                    id,
                    DirEntry {
                        parent,
                        name: name_ref,
                    },
                );
            }
        }
        if rec.ext_id != 0 && !self.exts.contains_key(&rec.ext_id) {
            if let Some(text) = store.ext_text_of(rec.ext_id)? {
                self.exts.insert(rec.ext_id, text);
            }
        }
        if let Some(&slot) = self.slot_by_id.get(&rec.id) {
            self.update_slot(slot, rec);
        } else {
            self.push(rec.clone());
        }
        Ok(())
    }

    /// Updates an existing slot in place (same `files` row id), keeping
    /// counters, byte budget and secondary indexes coherent. New strings
    /// are APPENDED to the arena (the old bytes become unreachable until
    /// the next wholesale reload compacts them — bounded by rename churn).
    fn update_slot(&mut self, slot: usize, rec: &FileRecord) {
        let lower = rec.name.to_lowercase();
        let lower_ref = self.put(&lower);
        let orig_ref = self.put(&rec.name);
        let py_full_ref = if rec.py_full == lower {
            lower_ref
        } else {
            self.put(&rec.py_full)
        };
        let py_init_ref = if rec.py_init == lower {
            lower_ref
        } else {
            self.put(&rec.py_init)
        };
        let logical = (lower.len()
            + rec.name.len()
            + if py_full_ref == lower_ref {
                0
            } else {
                rec.py_full.len()
            }
            + if py_init_ref == lower_ref {
                0
            } else {
                rec.py_init.len()
            }) as u64;
        self.interned_bytes = self
            .interned_bytes
            .saturating_sub(self.logical_bytes[slot] as u64)
            + logical;
        if self.is_dirs[slot] != rec.is_dir {
            self.is_dirs[slot] = rec.is_dir;
            if rec.is_dir {
                self.dir_count += 1;
                self.file_count = self.file_count.saturating_sub(1);
            } else {
                self.file_count += 1;
                self.dir_count = self.dir_count.saturating_sub(1);
            }
        }
        let old_key = (
            self.parents[slot],
            name_hash(self.str_of(self.orig_refs[slot])),
        );
        self.lower_refs[slot] = lower_ref;
        self.orig_refs[slot] = orig_ref;
        self.py_full_refs[slot] = py_full_ref;
        self.py_init_refs[slot] = py_init_ref;
        self.logical_bytes[slot] = logical as u32;
        self.sizes[slot] = rec.size;
        self.mtimes[slot] = rec.mtime;
        self.ext_ids[slot] = rec.ext_id;
        self.parents[slot] = rec.parent;
        let new_key = (rec.parent, name_hash(&rec.name));
        if old_key != new_key {
            self.slot_by_parent_name.remove(&old_key);
        }
        self.slot_by_parent_name.insert(new_key, slot);
    }

    /// Incremental removal (watcher path, Task 8): deletes the slot whose
    /// containing dir chain equals `segments` (the same spelling upsert
    /// uses: root segment first) and whose name matches exactly (event
    /// spellings carry stored casing; a drifted casing simply matches
    /// nothing and self-heals at the next rescan). When `segments + [name]`
    /// resolves to an interned `dirs` row (the removed entry was a
    /// directory), that subtree's `dirs` rows AND slots are dropped too, so
    /// `dir_tree_len` and `interned_bytes` stay honest. Returns the removed
    /// row id, or `None` when nothing matched (a normal outcome: the row
    /// may never have been indexed).
    pub(crate) fn remove_by_dir_name(&mut self, segments: &[&str], name: &str) -> Option<FileId> {
        let parent_id = self.resolve_dir(segments)?;
        let removed = self.remove_slot(parent_id, name);
        let mut with_name: Vec<&str> = segments.to_vec();
        with_name.push(name);
        if let Some(subtree_root) = self.resolve_dir(&with_name) {
            self.purge_dir_rows(subtree_root);
        }
        removed
    }

    /// Resolves a `dirs`-table segment chain (root segment first) to its
    /// dir id, exactly; `None` as soon as a link is missing. Each step is
    /// a hash hit verified against the arena (see [`name_hash`]).
    fn resolve_dir(&self, segments: &[&str]) -> Option<FileId> {
        let mut parent: Option<FileId> = None;
        let mut current: Option<FileId> = None;
        for segment in segments {
            let id = self.dir_by_parent_name.get(&(parent, name_hash(segment)))?;
            if self.str_of(self.dirs.get(id)?.name) != *segment {
                return None; // 64-bit hash collision inside one parent
            }
            parent = Some(*id);
            current = Some(*id);
        }
        current
    }

    /// Removes `root_id` and every `dirs` row whose ancestor chain crosses
    /// it from the interned tree, and sweeps every SLOT under the doomed
    /// subtree along with them. The sweep is load-bearing for convergence:
    /// a batch removes paths in sorted order, so a deleted directory's
    /// children are visited AFTER the dir rows are gone and their own
    /// `remove_by_dir_name` chain no longer resolves — without the sweep
    /// their slots would linger as phantoms. Per-entry budgets make a
    /// corrupt parent cycle terminate. Called only from
    /// [`HotIndex::remove_by_dir_name`].
    fn purge_dir_rows(&mut self, root_id: FileId) {
        let doomed: Vec<FileId> = self
            .dirs
            .iter()
            .filter(|(id, entry)| {
                if **id == root_id {
                    return true;
                }
                // A chain longer than the node count implies a cycle.
                let mut budget = self.dirs.len() + 1;
                let mut parent = entry.parent;
                loop {
                    match parent {
                        None => return false,
                        Some(p) if p == root_id => return true,
                        Some(p) => {
                            budget -= 1;
                            if budget == 0 {
                                return false;
                            }
                            parent = self.dirs.get(&p).and_then(|e| e.parent);
                        }
                    }
                }
            })
            .map(|(id, _)| *id)
            .collect();
        if doomed.is_empty() {
            return;
        }
        // Victim slots: those whose DIRECT parent is a doomed dir row (a
        // slot's parent is always one level up, so membership in the doomed
        // set is the exact subtree test). Removed in descending slot order:
        // swap-remove relocates only the last slot, so lower-index victims
        // keep their indices while higher ones are processed first.
        let doomed_set: HashSet<FileId> = doomed.iter().copied().collect();
        let mut victims: Vec<usize> = (0..self.parents.len())
            .filter(|&slot| doomed_set.contains(&self.parents[slot]))
            .collect();
        victims.sort_unstable_by(|a, b| b.cmp(a));
        for slot in victims {
            self.remove_slot_at(slot);
        }
        for id in doomed {
            if let Some(entry) = self.dirs.remove(&id) {
                self.interned_bytes = self.interned_bytes.saturating_sub(entry.name.len as u64);
                self.dir_by_parent_name
                    .remove(&(entry.parent, name_hash(self.str_of(entry.name))));
            }
        }
    }

    /// Swap-removes the slot keyed by `(parent_id, name)` (exact name); see
    /// [`HotIndex::remove_slot_at`] for the mechanics.
    fn remove_slot(&mut self, parent_id: FileId, name: &str) -> Option<FileId> {
        let &slot = self
            .slot_by_parent_name
            .get(&(parent_id, name_hash(name)))?;
        Some(self.remove_slot_at(slot))
    }

    /// Swap-removes the slot at `slot`, keeping every arena, counter, byte
    /// budget and secondary index consistent — the last slot moves into the
    /// hole and its index entries are repointed. Returns the removed row id.
    fn remove_slot_at(&mut self, slot: usize) -> FileId {
        let removed_id = self.ids[slot];
        let last = self.lower_refs.len() - 1;
        let key = (
            self.parents[slot],
            name_hash(self.str_of(self.orig_refs[slot])),
        );

        if self.is_dirs[slot] {
            self.dir_count = self.dir_count.saturating_sub(1);
        } else {
            self.file_count = self.file_count.saturating_sub(1);
        }
        self.interned_bytes = self
            .interned_bytes
            .saturating_sub(self.logical_bytes[slot] as u64);

        // Bookkeeping for the slot that moves into the hole (if any).
        let moved = if slot != last {
            Some((
                self.ids[last],
                (
                    self.parents[last],
                    name_hash(self.str_of(self.orig_refs[last])),
                ),
            ))
        } else {
            None
        };

        self.lower_refs.swap(slot, last);
        self.lower_refs.pop();
        self.orig_refs.swap(slot, last);
        self.orig_refs.pop();
        self.py_full_refs.swap(slot, last);
        self.py_full_refs.pop();
        self.py_init_refs.swap(slot, last);
        self.py_init_refs.pop();
        self.logical_bytes.swap(slot, last);
        self.logical_bytes.pop();
        self.ids.swap(slot, last);
        self.ids.pop();
        self.parents.swap(slot, last);
        self.parents.pop();
        self.sizes.swap(slot, last);
        self.sizes.pop();
        self.mtimes.swap(slot, last);
        self.mtimes.pop();
        self.ext_ids.swap(slot, last);
        self.ext_ids.pop();
        self.is_dirs.swap(slot, last);
        self.is_dirs.pop();

        self.slot_by_id.remove(&removed_id);
        self.slot_by_parent_name.remove(&key);
        if let Some((moved_id, moved_key)) = moved {
            self.slot_by_id.insert(moved_id, slot);
            self.slot_by_parent_name.insert(moved_key, slot);
        }
        removed_id
    }

    /// Non-directory entries.
    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    /// Directory entries (`files` rows with `is_dir = 1`).
    pub fn dir_count(&self) -> u64 {
        self.dir_count
    }

    /// Interned directory rows (the `dirs`-table tree size).
    pub fn dir_tree_len(&self) -> usize {
        self.dirs.len()
    }

    /// The interned dir row for a store `dirs` id: `(parent, name)`, or
    /// `None` when the id is not interned. Path reconstruction for search
    /// results (Task 4) is the real consumer; tests use it to pin the
    /// parent linkage.
    pub fn dir_row(&self, id: FileId) -> Option<(Option<FileId>, &str)> {
        self.dirs
            .get(&id)
            .map(|entry| (entry.parent, self.str_of(entry.name)))
    }

    /// Entry count (files + directory entries): the arena length.
    #[inline]
    pub fn slot_count(&self) -> usize {
        self.lower_refs.len()
    }

    /// The `files` row id of `slot`.
    #[inline]
    pub fn file_id(&self, slot: usize) -> FileId {
        self.ids[slot]
    }

    /// The slot whose `files` row id is `id` (O(1) reverse lookup), or
    /// `None` when the row is not indexed. The dedup groups view (Task 9)
    /// materializes stored groups' members through this.
    pub fn slot_of_id(&self, id: FileId) -> Option<usize> {
        self.slot_by_id.get(&id).copied()
    }

    /// Iterates every interned dir row as `(id, parent, name)` (`None`
    /// parent = root segment). The size aggregation (Task 11) rebuilds
    /// the tree's adjacency from this.
    pub(crate) fn all_dir_rows(&self) -> impl Iterator<Item = (FileId, Option<FileId>, &str)> + '_ {
        self.dirs
            .iter()
            .map(|(id, entry)| (*id, entry.parent, self.str_of(entry.name)))
    }

    /// The interned dir id of `(parent, name)` (exact name), or `None` —
    /// one resolution step of [`Self::resolve_dir`], exposed so the size
    /// aggregation can walk a root path segment by segment and detect a
    /// FILE at the chain's end.
    pub(crate) fn dir_child(&self, parent: Option<FileId>, name: &str) -> Option<FileId> {
        let id = *self.dir_by_parent_name.get(&(parent, name_hash(name)))?;
        if self.str_of(self.dirs.get(&id)?.name) == name {
            Some(id)
        } else {
            None
        }
    }

    /// The slot stored under `(parent_dir, name)` (exact name), or
    /// `None`. The size aggregation's file-root resolution uses this.
    pub(crate) fn slot_in_dir(&self, parent: FileId, name: &str) -> Option<usize> {
        let &slot = self.slot_by_parent_name.get(&(parent, name_hash(name)))?;
        if self.str_of(self.orig_refs[slot]) == name {
            Some(slot)
        } else {
            None
        }
    }

    /// The extension text interned for `ext_id` (from the `extensions`
    /// table); `""` for the no-extension sentinel 0 and unknown ids.
    pub fn ext_of(&self, ext_id: FileId) -> &str {
        self.exts.get(&ext_id).map_or("", String::as_str)
    }

    /// The interned ext id whose extension text equals `ext` (compare in
    /// the interned spelling: lowercased, no leading dot — see
    /// [`HotIndex::ext_of`]). `None` when no `extensions` row carries that
    /// text. Extension texts are UNIQUE, so at most one id matches; the
    /// `ext:` search filter uses this to turn its string compare into an
    /// integer one.
    pub fn ext_id_of(&self, ext: &str) -> Option<FileId> {
        self.exts
            .iter()
            .find(|(_, text)| text.as_str() == ext)
            .map(|(id, _)| *id)
    }

    /// Original-case dir names from the root segment down to `slot`'s
    /// containing directory, root first ([`HotIndex::path_of`] appends the
    /// entry's own name to this chain).
    pub fn dir_chain(&self, slot: usize) -> Vec<&str> {
        let mut chain: Vec<&str> = Vec::new();
        self.walk_chain(slot, |name| chain.push(name));
        chain.reverse();
        chain
    }

    /// Full path of `slot` rebuilt from the interned dir tree: root
    /// segment, ..., parent dirs, entry name — original spelling, joined
    /// with `std::path::MAIN_SEPARATOR`. This is the index-relative
    /// spelling the scanner stores (leading root segment, no drive
    /// prefix).
    pub fn path_of(&self, slot: usize) -> String {
        let mut segments = self.dir_chain(slot);
        segments.push(self.name_original(slot));
        let bytes =
            segments.iter().map(|seg| seg.len()).sum::<usize>() + segments.len().saturating_sub(1);
        let mut path = String::with_capacity(bytes);
        for (i, segment) in segments.iter().enumerate() {
            if i > 0 {
                path.push(MAIN_SEPARATOR);
            }
            path.push_str(segment);
        }
        path
    }

    /// Byte length of [`HotIndex::path_of`] for `slot` without building
    /// the string — the ranking pass needs it for every match.
    #[inline]
    pub fn path_len(&self, slot: usize) -> u64 {
        let mut len = self.orig_refs[slot].len as u64;
        self.walk_chain(slot, |name| len += name.len() as u64 + 1); // + separator
        len
    }

    /// [`HotIndex::path_len`], amortized across one ranking pass by a
    /// per-dir memo (`query.rs` builds ONE map per pass and threads it
    /// through every slot): the first slot under a given dir pays the
    /// ancestor walk, every later slot under the same dir is a single hash
    /// hit. At 1M matches the per-match chain walk is a measurable budget
    /// line; per-DIR memoization (not per-slot) keeps the map small —
    /// a scan-root tree holds orders of magnitude fewer dirs than slots.
    /// The memo is never reused across passes (a wholesale reload or
    /// watcher batch can swap dir rows between passes).
    #[inline]
    pub(crate) fn path_len_memo(&self, memo: &mut HashMap<FileId, u64>, slot: usize) -> u64 {
        self.orig_refs[slot].len as u64 + self.dir_chain_bytes(memo, self.parents[slot])
    }

    /// Total byte length of dir `id`'s reconstructed path — its own name
    /// plus every PRESENT ancestor plus one separator each, i.e. exactly
    /// what [`HotIndex::walk_chain`] would add for a slot whose containing
    /// dir is `id` — computed through `memo` so each dir's chain is walked
    /// at most once per pass. A missing ancestor row ends the chain where
    /// [`HotIndex::walk_chain`] stops; a step budget guards against a
    /// corrupt parent cycle, same as the shared chain walk.
    fn dir_chain_bytes(&self, memo: &mut HashMap<FileId, u64>, dir: FileId) -> u64 {
        let mut chain: Vec<FileId> = Vec::new();
        let mut current = dir;
        // The chain length the fill starts from: a memo hit (an ancestor
        // already resolved this pass), 0 for a missing row (the chain
        // contributes nothing past it), None for a root (fill from 0).
        let mut seed = None;
        let mut budget = self.dirs.len() + 1;
        loop {
            if let Some(len) = memo.get(&current) {
                seed = Some(*len);
                break;
            }
            let Some(entry) = self.dirs.get(&current) else {
                seed = Some(0);
                break;
            };
            chain.push(current);
            budget -= 1;
            match entry.parent {
                Some(parent) if budget > 0 => current = parent,
                // Root of the chain — or a budget-exhausted (cycled) one,
                // which terminates the same way walk_chain's budget does.
                _ => break,
            }
        }
        let mut acc = seed.unwrap_or(0);
        for id in chain.iter().rev() {
            let entry = &self.dirs[id];
            acc += entry.name.len as u64 + 1;
            memo.insert(*id, acc);
        }
        acc
    }

    /// Shared chain walk: `visit(dir_name)` for every dir from `slot`'s
    /// parent up to the root, stopping at a missing row, at a root, or
    /// after `dirs.len() + 1` steps — the tree cannot be deeper than its
    /// node count, so even a corrupt parent cycle (impossible via the
    /// store's API) terminates.
    fn walk_chain<'a, V: FnMut(&'a str)>(&'a self, slot: usize, mut visit: V) {
        let mut budget = self.dirs.len() + 1;
        let mut current = self.parents[slot];
        while budget > 0 {
            budget -= 1;
            let Some((parent, name)) = self.dir_row(current) else {
                break;
            };
            visit(name);
            match parent {
                Some(id) => current = id,
                None => break,
            }
        }
    }

    /// True once the initial full load completed.
    pub fn is_warm(&self) -> bool {
        self.warm
    }

    /// Total bytes held by the arenas: name lowercase + name original +
    /// both pinyin keys + dir names.
    pub fn interned_bytes(&self) -> u64 {
        self.interned_bytes
    }

    /// Lowercased name of `slot` (case-insensitive matching).
    #[inline]
    pub fn name_lower(&self, slot: usize) -> &str {
        self.str_of(self.lower_refs[slot])
    }

    /// Original-case name of `slot` (display, rebuilds).
    #[inline]
    pub fn name_original(&self, slot: usize) -> &str {
        self.str_of(self.orig_refs[slot])
    }

    /// Full-pinyin key of `slot` ("baogao.docx"), loaded from the store's
    /// `py_full` column. Substring matching target alongside
    /// [`HotIndex::name_lower`]. Aliases the lowercased name for
    /// pure-ASCII names (identical text — see module docs).
    #[inline]
    pub fn py_full(&self, slot: usize) -> &str {
        self.str_of(self.py_full_refs[slot])
    }

    /// Whether `slot`'s full-pinyin key aliases its lowercased name (the
    /// pure-ASCII case). The pinyin fallback in `query.rs` skips its two
    /// key scans for such slots — the name scan that ran first already
    /// proved the word is not in the identical text.
    #[inline]
    pub(crate) fn py_full_aliases_name(&self, slot: usize) -> bool {
        self.py_full_refs[slot] == self.lower_refs[slot]
    }

    /// First-letter pinyin key of `slot` ("bg.docx"), loaded from the
    /// store's `py_init` column. Substring matching target alongside
    /// [`HotIndex::py_full`]. Aliases the lowercased name for pure-ASCII
    /// names.
    #[inline]
    pub fn py_init(&self, slot: usize) -> &str {
        self.str_of(self.py_init_refs[slot])
    }

    /// Containing directory's `dirs` row id of `slot`.
    #[inline]
    pub fn parent_dir(&self, slot: usize) -> FileId {
        self.parents[slot]
    }

    /// Size in bytes of `slot` (0 for directory entries).
    #[inline]
    pub fn size(&self, slot: usize) -> u64 {
        self.sizes[slot]
    }

    /// mtime (Unix-epoch seconds) of `slot`.
    #[inline]
    pub fn mtime(&self, slot: usize) -> i64 {
        self.mtimes[slot]
    }

    /// Interned extension id of `slot` (0 = no extension).
    #[inline]
    pub fn ext_id(&self, slot: usize) -> i64 {
        self.ext_ids[slot]
    }

    /// Whether `slot` is a directory entry.
    #[inline]
    pub fn is_dir_entry(&self, slot: usize) -> bool {
        self.is_dirs[slot]
    }

    /// The `files` row id the index-relative `path` names (the `Hit::path`
    /// spelling; both separators accepted, directories included): walk every
    /// segment but the last through the interned dir tree, then look the
    /// final name up as a slot under that dir. Directory entries ARE slots
    /// (`files` rows with `is_dir = 1`), so a dir path yields the same row
    /// id a search hit for that dir would carry — the shape `coll_add`
    /// validates against. `None` when nothing matches: an unknown segment,
    /// a single-segment path naming no entry (the walk root itself is never
    /// a `files` row), or the empty virtual-root path `""`, which names no
    /// single entry. The inverse of [`HotIndex::path_of`] — the app's
    /// `file_id_of` command (Task 18) turns flame-chart nodes, which carry
    /// paths rather than ids, into collection members with it.
    pub fn id_of_path(&self, path: &str) -> Option<FileId> {
        let segments: Vec<&str> = path
            .split([MAIN_SEPARATOR, '/'])
            .filter(|segment| !segment.is_empty())
            .collect();
        let (name, parents) = segments.split_last()?;
        let dir_id = self.resolve_dir(parents)?;
        let slot = self.slot_in_dir(dir_id, name)?;
        Some(self.file_id(slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn rec(name: &str, is_dir: bool) -> FileRecord {
        FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size: 10,
            mtime: 1_700_000_000,
            ctime: 1_700_000_001,
            atime: 1_700_000_002,
            ext_id: 0,
            is_dir,
            hash_first: None,
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        }
    }

    #[test]
    fn load_counts_arenas_and_warm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        // Dir tree: "docs" with a nested "inner"; three entries under docs.
        let mut alpha = rec("Alpha.TXT", false);
        store.upsert_file(&["docs"], &mut alpha).unwrap();
        let mut sub = rec("Sub", true);
        store.upsert_file(&["docs"], &mut sub).unwrap();
        let mut memo = rec("memo.md", false);
        store.upsert_file(&["docs", "inner"], &mut memo).unwrap();

        // The store-side ids the hot index must reuse.
        let dir_id = |name: &str| {
            store
                .all_dirs()
                .unwrap()
                .into_iter()
                .find(|(_, _, dir_name)| dir_name == name)
                .map(|(id, _, _)| id)
                .unwrap()
        };
        let docs_id = dir_id("docs");
        let inner_id = dir_id("inner");

        let hot = HotIndex::load(&store).unwrap();

        assert!(hot.is_warm(), "a completed load yields a warm index");
        assert_eq!(hot.file_count(), 2);
        assert_eq!(hot.dir_count(), 1);
        assert_eq!(hot.dir_tree_len(), 2, "docs and docs/inner are interned");
        assert_eq!(
            hot.interned_bytes(),
            // dir names (docs=4, inner=5) + per-entry lower+original name
            // bytes. Pinyin keys for pure-ASCII names ALIAS the lowercased
            // name's arena range (arena refactor) and add zero logical
            // bytes: 9+9 (Alpha.TXT) + 3+3 (Sub) + 7+7 (memo.md).
            4 + 5 + 9 + 9 + 3 + 3 + 7 + 7,
            "interned bytes = dir names + lower/original names; ASCII pinyin aliases add 0"
        );

        // The interned tree reuses the DB's dirs ids and parent links.
        assert_eq!(hot.dir_row(docs_id), Some((None, "docs")));
        assert_eq!(hot.dir_row(inner_id), Some((Some(docs_id), "inner")));
        assert_eq!(hot.dir_row(999_999), None);

        // Slots are dense and id-ordered (insertion order here).
        assert_eq!(hot.name_lower(0), "alpha.txt");
        assert_eq!(hot.name_original(0), "Alpha.TXT");
        assert!(!hot.is_dir_entry(0));
        assert_eq!(hot.size(0), 10);
        assert_eq!(hot.mtime(0), 1_700_000_000);
        assert_eq!(
            hot.ext_id(0),
            alpha.ext_id,
            "the store's ext_id survives the load"
        );
        assert_eq!(hot.name_original(1), "Sub");
        assert!(hot.is_dir_entry(1));
        assert_eq!(hot.name_lower(2), "memo.md");
        // Pinyin arenas ride along (ASCII names: both keys equal the
        // lowercased name).
        assert_eq!(hot.py_full(0), "alpha.txt");
        assert_eq!(hot.py_init(0), "alpha.txt");
        assert_eq!(hot.py_full(2), "memo.md");
        assert_eq!(hot.py_init(2), "memo.md");
        assert_eq!(hot.parent_dir(0), docs_id);
        assert_eq!(hot.parent_dir(2), inner_id);
        assert!(docs_id > 0, "parents are real dir row ids");

        // The pre-load index is cold and empty.
        let cold = HotIndex::cold();
        assert!(!cold.is_warm());
        assert_eq!((cold.file_count(), cold.dir_count()), (0, 0));
        assert_eq!(cold.interned_bytes(), 0);
        assert_eq!(cold.dir_tree_len(), 0);
    }

    #[test]
    fn load_carries_pinyin_arenas() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        // One Han name, one pure-ASCII name (whose keys degenerate to the
        // lowercased spelling); keys are whatever the store persisted.
        let mut report = rec("报告.docx", false);
        store.upsert_file(&["docs"], &mut report).unwrap();
        let mut readme = rec("Readme.TXT", false);
        store.upsert_file(&["docs"], &mut readme).unwrap();

        let hot = HotIndex::load(&store).unwrap();
        assert_eq!(hot.py_full(0), "baogao.docx");
        assert_eq!(hot.py_init(0), "bg.docx");
        assert_eq!(hot.py_full(1), "readme.txt");
        assert_eq!(hot.py_init(1), "readme.txt");
    }

    #[test]
    fn path_reconstruction_and_ext_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut alpha = rec("Alpha.TXT", false);
        store.upsert_file(&["docs"], &mut alpha).unwrap();
        let mut memo = rec("memo.md", false);
        store.upsert_file(&["docs", "inner"], &mut memo).unwrap();

        let hot = HotIndex::load(&store).unwrap();
        let sep = std::path::MAIN_SEPARATOR;

        // Slots are id-ordered, so the upsert order fixes them here.
        assert_eq!(hot.slot_count(), 2);
        assert_eq!(hot.file_id(0), alpha.id, "the files row id travels");
        assert_eq!(hot.file_id(1), memo.id);
        // Paths rebuild root -> ... -> parent -> name in original spelling.
        assert_eq!(hot.path_of(0), format!("docs{sep}Alpha.TXT"));
        assert_eq!(hot.path_of(1), format!("docs{sep}inner{sep}memo.md"));
        assert_eq!(hot.dir_chain(1), vec!["docs", "inner"]);
        assert_eq!(
            hot.path_len(1),
            format!("docs{sep}inner{sep}memo.md").len() as u64
        );
        // Extension text comes from the extensions table; 0 means "".
        assert_eq!(hot.ext_of(alpha.ext_id), "txt");
        assert_eq!(hot.ext_of(memo.ext_id), "md");
        assert_eq!(hot.ext_of(0), "");
        // The row-id reverse lookup (dedup groups materialization).
        assert_eq!(hot.slot_of_id(alpha.id), Some(0));
        assert_eq!(hot.slot_of_id(memo.id), Some(1));
        assert_eq!(hot.slot_of_id(424_242), None);
    }

    /// `id_of_path` is the inverse of `path_of`: every indexed entry's path
    /// round-trips to its own row id, files and directory entries alike,
    /// and unknown shapes match nothing.
    #[test]
    fn id_of_path_roundtrips_path_of() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut alpha = rec("Alpha.TXT", false);
        store.upsert_file(&["docs"], &mut alpha).unwrap();
        let mut sub = rec("Sub", true);
        store.upsert_file(&["docs"], &mut sub).unwrap();
        let mut memo = rec("memo.md", false);
        store.upsert_file(&["docs", "inner"], &mut memo).unwrap();

        let hot = HotIndex::load(&store).unwrap();
        let sep = std::path::MAIN_SEPARATOR;

        // Round-trip: the path each slot reconstructs names its own row.
        for slot in 0..hot.slot_count() {
            let path = hot.path_of(slot);
            assert_eq!(
                hot.id_of_path(&path),
                Some(hot.file_id(slot)),
                "path {path:?} must resolve to its own row id"
            );
        }
        // The dir-entry slot is reachable by path too (dir ids are files
        // row ids here — a search hit for "Sub" carries exactly this id).
        assert_eq!(hot.id_of_path(&format!("docs{sep}Sub")), Some(sub.id));
        // Forward separators are accepted as well (the engine normalizes
        // neither; both spellings occur in user-built queries).
        assert_eq!(hot.id_of_path("docs/Alpha.TXT"), Some(alpha.id));

        // Nothing matches: the virtual root, an unknown segment, a file
        // used as an INTERMEDIATE segment, a truncated chain, and a bare
        // walk-root name (never a files row).
        assert_eq!(hot.id_of_path(""), None);
        assert_eq!(hot.id_of_path("docs/nope.txt"), None);
        assert_eq!(hot.id_of_path("docs/Alpha.TXT/memo.md"), None);
        assert_eq!(hot.id_of_path(&format!("docs{sep}inner")), None);
        assert_eq!(hot.id_of_path("docs"), None);
    }

    /// Full incremental round-trip: upsert a brand-new row (new dir chain
    /// and ext interned along the way), re-upsert it (same id, update in
    /// place), then remove it — arenas, counters, indexes and paths stay
    /// consistent throughout.
    #[test]
    fn incremental_upsert_and_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut seed = rec("seed.txt", false);
        store.upsert_file(&["docs"], &mut seed).unwrap();
        let mut hot = HotIndex::load(&store).unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(hot.file_count(), 1);
        let bytes_before = hot.interned_bytes();

        // New file in a brand-new subdirectory chain docs/inner.
        let mut fresh = rec("Fresh.TXT", false);
        store.upsert_file(&["docs", "inner"], &mut fresh).unwrap();
        hot.upsert_slot(&store, &fresh).unwrap();

        assert_eq!(hot.file_count(), 2);
        assert_eq!(hot.slot_count(), 2);
        assert_eq!(hot.dir_tree_len(), 2, "docs and docs/inner interned");
        assert!(hot.interned_bytes() > bytes_before);
        let fresh_slot = hot
            .slot_by_id
            .get(&fresh.id)
            .copied()
            .expect("test may index");
        assert_eq!(
            hot.path_of(fresh_slot),
            format!("docs{sep}inner{sep}Fresh.TXT")
        );
        assert_eq!(hot.name_lower(fresh_slot), "fresh.txt");
        assert_eq!(hot.ext_of(fresh.ext_id), "txt", "ext interned on demand");

        // Re-upsert same (dir, name): same row id, same slot, new size —
        // no duplicate slot, no counter drift.
        let mut updated = rec("Fresh.TXT", false);
        updated.size = 99;
        store.upsert_file(&["docs", "inner"], &mut updated).unwrap();
        assert_eq!(updated.id, fresh.id);
        hot.upsert_slot(&store, &updated).unwrap();
        assert_eq!(hot.file_count(), 2);
        assert_eq!(hot.slot_count(), 2);
        assert_eq!(hot.size(hot.slot_by_id[&fresh.id]), 99);

        // Remove the file by its path spelling (case-insensitive): slot
        // gone, seed untouched, dir rows stay (still referenced).
        // Remove the file by its exact stored spelling; a drifted casing
        // would benignly match nothing (see remove_by_dir_name docs).
        assert_eq!(
            hot.remove_by_dir_name(&["docs", "inner"], "Fresh.TXT"),
            Some(fresh.id)
        );
        assert_eq!(hot.file_count(), 1);
        assert_eq!(hot.slot_count(), 1);
        assert_eq!(hot.name_lower(0), "seed.txt");
        assert_eq!(hot.dir_tree_len(), 2);
        // Removing something never indexed is a benign None.
        assert_eq!(hot.remove_by_dir_name(&["docs", "inner"], "nope.txt"), None);
        assert_eq!(hot.file_count(), 1);

        // Remove a whole directory: its dir rows are purged from the tree.
        let mut sub = rec("inner", true);
        store.upsert_file(&["docs"], &mut sub).unwrap();
        hot.upsert_slot(&store, &sub).unwrap();
        assert_eq!(hot.dir_count(), 1);
        assert_eq!(
            hot.remove_by_dir_name(&["docs"], "inner"),
            Some(sub.id),
            "the dir-entry slot goes by its parent chain"
        );
        assert_eq!(hot.dir_count(), 0);
        assert_eq!(hot.dir_tree_len(), 1, "only docs remains interned");
    }

    /// Removing a directory entry sweeps the slots of everything under it:
    /// a deleted directory's children are processed after the dir rows are
    /// gone, so the purge (not the children's own removals) must claim
    /// their slots. This is the storm-convergence invariant.
    #[test]
    fn purging_a_dir_sweeps_child_slots() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        // docs/{a.txt, sub(dir entry)/{b.txt, deep/{c.txt}}}
        let mut a = rec("a.txt", false);
        store.upsert_file(&["docs"], &mut a).unwrap();
        let mut sub = rec("sub", true);
        store.upsert_file(&["docs"], &mut sub).unwrap();
        let mut b = rec("b.txt", false);
        store.upsert_file(&["docs", "sub"], &mut b).unwrap();
        let mut c = rec("c.txt", false);
        store.upsert_file(&["docs", "sub", "deep"], &mut c).unwrap();

        let mut hot = HotIndex::load(&store).unwrap();
        assert_eq!(hot.file_count(), 3);
        assert_eq!(hot.dir_count(), 1);
        assert_eq!(hot.dir_tree_len(), 3);

        assert_eq!(hot.remove_by_dir_name(&["docs"], "sub"), Some(sub.id));
        assert_eq!(
            hot.file_count(),
            1,
            "b.txt and the nested c.txt go with the subtree"
        );
        assert_eq!(hot.dir_count(), 0);
        assert_eq!(hot.slot_count(), 1);
        assert_eq!(hot.name_lower(0), "a.txt", "the sibling outside survives");
        assert_eq!(hot.dir_tree_len(), 1, "only docs remains interned");
        // The swept slots left no dangling index entries behind.
        assert!(!hot.slot_by_id.contains_key(&b.id));
        assert!(!hot.slot_by_id.contains_key(&c.id));
    }

    /// Swap-remove keeps the secondary indexes pointing at the right slots
    /// when a middle slot is removed and the tail moves into its place.
    #[test]
    fn swap_remove_keeps_indexes_consistent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut ids = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            let mut r = rec(name, false);
            store.upsert_file(&["docs"], &mut r).unwrap();
            ids.push(r.id);
        }
        let mut hot = HotIndex::load(&store).unwrap();
        let bytes_before = hot.interned_bytes();

        // Remove the SECOND slot (a middle one): the tail (d.txt) moves.
        assert_eq!(hot.remove_by_dir_name(&["docs"], "b.txt"), Some(ids[1]));
        assert_eq!(hot.file_count(), 3);
        assert_eq!(hot.slot_count(), 3);
        assert!(hot.interned_bytes() < bytes_before);

        // Every remaining id resolves to a slot whose name matches, and
        // every remaining name resolves to a slot whose id matches.
        for (name, id) in [("a.txt", ids[0]), ("c.txt", ids[2]), ("d.txt", ids[3])] {
            let slot = hot.slot_by_id[&id];
            assert_eq!(hot.name_original(slot), name);
            assert_eq!(hot.file_id(slot), id);
            let key_slot = hot.slot_by_parent_name[&(hot.parent_dir(slot), name_hash(name))];
            assert_eq!(key_slot, slot);
        }
        assert!(!hot.slot_by_id.contains_key(&ids[1]));
    }
}
