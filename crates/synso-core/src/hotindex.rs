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
//! on name hashes instead of duplicating the name string (the dir tree
//! on a 64-bit name hash, the file slot table on a 32-bit fingerprint of
//! a parent+name combined hash — see [`NameSlotTable`]) — every read
//! still verifies the arena bytes. At 1.6M files this cut true process
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
use crate::{Result, SynsoError};

/// A string's location in the shared byte arena. `repr(C, packed)` keeps
/// it at exactly 6 bytes: four refs live in per-slot columns and millions
/// of slots compound the saving over the old `{u32, u32}` layout. Packed
/// fields are read BY VALUE only (no `&field` — a misaligned reference
/// would be UB); the derives all work by value on the `Copy` fields and
/// every reader follows the same discipline.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct StrRef {
    start: u32,
    len: u16,
}

/// The packed layout is load-bearing for the memory budget: a layout
/// change must be deliberate, not a quiet relapse to 8 bytes.
const _: () = assert!(std::mem::size_of::<StrRef>() == 6);

/// 64-bit hash of a name, used as the string-free key component of the
/// DIR-tree secondary index (`dir_by_parent_name`; the file slot table
/// fingerprints [`parent_name_hash`] instead — see [`NameSlotTable`]).
/// Collisions between two DIFFERENT names under the SAME parent would
/// need a 64-bit birthday hit within a handful of siblings (≈2⁻⁶⁴ per
/// pair); every read still VERIFIES the arena bytes, so a collision
/// degrades that one lookup to "not found" until the next wholesale
/// reload — it can never return a wrong entry.
fn name_hash(name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// 64-bit COMBINED hash of `(parent, name)` — the key source of
/// [`NameSlotTable`]'s fingerprint. The parent is hashed FIRST and the
/// name second, so files that share a name (desktop.ini / README.md —
/// easily tens of thousands of copies) but live under different parents
/// hash apart; keying the table on `name_hash` alone would pour every
/// same-name copy into one home bucket and one probe chain (builder-check
/// 裁定 2's anti-clustering requirement). Same `DefaultHasher`
/// construction as [`name_hash`], which itself stays the dir-tree
/// index's key component.
fn parent_name_hash(parent: FileId, name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parent.hash(&mut hasher);
    name.hash(&mut hasher);
    hasher.finish()
}

/// [`NameSlotTable`]'s fingerprint for a `(parent, name)` key: the HIGH
/// 32 bits of [`parent_name_hash`]. The home bucket derives from the
/// stored fingerprint itself (`fp & mask` — the fingerprint's low bits
/// are hash bits 32..32+k), so an entry carries everything relocation
/// needs: "store the hash, derive the index".
#[inline]
fn parent_name_fp(parent: FileId, name: &str) -> u32 {
    (parent_name_hash(parent, name) >> 32) as u32
}

/// Empty-bucket sentinel for [`NameSlotTable`]: `slot == u32::MAX` marks
/// a free bucket. A real slot can never collide with it — slots are
/// dense column positions, and the load guard keeps the entry count
/// (hence slot indices, which never exceed it) far below the ~128GB of
/// column memory a u32::MAX slot would imply.
const EMPTY_SLOT: u32 = u32::MAX;

/// Load ceiling for [`NameSlotTable`]: an insert that would push `len`
/// past 72% of capacity grows first. After a ×2 growth the table sits at
/// ≤36%, and linear-probe chains stay short at both points.
const SLOT_TABLE_MAX_LOAD_PERCENT: usize = 72;

/// Smallest capacity [`NameSlotTable`] ever grows to: keeps watcher-fed
/// cold indexes from rehashing through 1→2→4 buckets, and keeps the
/// mask arithmetic (`cap - 1`) away from zero.
const SLOT_TABLE_MIN_CAPACITY: usize = 8;

/// Linear-probing open-addressed map from a `(parent, name)` fingerprint
/// to a slot — the memory-critical replacement for the std HashMap this
/// index used to carry as `slot_by_parent_name` (v1.2: the HashMap's
/// estimate was ≈60MB at 2.1M entries, 24B per live entry before load
/// slack; 8-byte entries over a power-of-two capacity land at roughly
/// half that — exact numbers in `docs/known-issues-v1.md`'s v1.2 遗留 section).
///
/// Entries are `(fingerprint, slot)` pairs — fingerprint = the high 32
/// bits of [`parent_name_hash`], home bucket = the fingerprint's low
/// bits (`fp & (capacity - 1)`) — so an entry is fully self-contained:
/// backward-shift deletion and growth rehashing replay entries from
/// their own bytes alone.
///
/// Identity contract (builder-check 裁定 1): the table NEVER decides
/// identity on its own. Read paths ([`Self::get_matching`]) walk the
/// fingerprint chain and hand each candidate slot to the HOST, which
/// verifies against LIVE columns (`parents[slot] == parent && arena
/// name bytes match`). Mutation paths ([`Self::remove_by_slot`] /
/// [`Self::rebind`]) run AFTER the identity columns may already have
/// been overwritten (update_slot writes the columns first; swap-remove
/// runs after the swap-pop), so they locate an entry by (fingerprint,
/// stored slot number) ONLY — a slot number is stable across the whole
/// operation, and one slot exists in at most one entry.
///
/// No-panic / termination: every probe loop advances one bucket per
/// iteration and stops at the first bucket holding [`EMPTY_SLOT`].
/// [`Self::insert`] grows BEFORE an entry could cross the load ceiling,
/// so `len ≤ floor(0.72 × capacity) < capacity` always holds — at least
/// one empty bucket exists in the ring at every moment, and even a
/// pathological table whose live entries ALL share one fingerprint
/// (they can only coexist, never fill the ring) terminates within one
/// lap. A zero-capacity table (the `Default` cold state, before the
/// first insert grows it) is guarded by early returns before any mask
/// arithmetic runs.
#[derive(Debug, Default)]
struct NameSlotTable {
    /// `(fingerprint, slot)` per bucket; `slot == EMPTY_SLOT` ⇒ free.
    entries: Vec<(u32, u32)>,
    /// Live entries (buckets whose slot != [`EMPTY_SLOT`]).
    len: usize,
}

impl NameSlotTable {
    /// A table pre-sized for `expected` entries: capacity starts at the
    /// power of two covering `expected × 1.4` (load lands at ≤ 1/1.4 ≈
    /// 0.714) and doubles once more wherever that rounding lands under
    /// the 72% ceiling for the whole batch (e.g. expected 6 → 8 buckets,
    /// ceiling 5) — so the load path's batch of inserts never triggers a
    /// rehash, whatever the row count. `expected == 0` (and `Default`)
    /// stay allocation-free; the first insert grows to
    /// [`SLOT_TABLE_MIN_CAPACITY`].
    fn new(expected: usize) -> NameSlotTable {
        let mut table = NameSlotTable::default();
        if expected > 0 {
            table.grow_to(expected * 7 / 5); // ×1.4, rounded up to a pow2
            while (expected + 1) * 100
                > table.entries.len() * SLOT_TABLE_MAX_LOAD_PERCENT
            {
                table.grow_to(table.entries.len() * 2);
            }
        }
        table
    }

    /// Live bytes under the breakdown's accounting: the entries vec's length
    /// × 8 (the power-of-two capacity IS the live allocation here —
    /// no separate load-factor slack like a std HashMap's).
    fn mem_bytes(&self) -> u64 {
        self.entries.len() as u64 * std::mem::size_of::<(u32, u32)>() as u64
    }

    /// The never-full invariant's only enforcement point: grow (double,
    /// rehash) when the NEXT entry would cross
    /// [`SLOT_TABLE_MAX_LOAD_PERCENT`], so every probe ring always holds
    /// an empty bucket to stop at.
    fn reserve_for_insert(&mut self) {
        let cap = self.entries.len();
        if (self.len + 1) * 100 > cap * SLOT_TABLE_MAX_LOAD_PERCENT {
            self.grow_to(if cap == 0 {
                SLOT_TABLE_MIN_CAPACITY
            } else {
                cap * 2
            });
        }
    }

    /// Rehashes into a fresh power-of-two capacity ≥ `min_capacity`
    /// (never shrinks; at least [`SLOT_TABLE_MIN_CAPACITY`]). Entries
    /// are self-contained (fingerprint → home bucket), so rehashing is
    /// a plain re-placement sweep over the old vec — no host state, no
    /// order dependence.
    fn grow_to(&mut self, min_capacity: usize) {
        let new_cap = min_capacity
            .max(SLOT_TABLE_MIN_CAPACITY)
            .next_power_of_two();
        if new_cap <= self.entries.len() {
            return;
        }
        let old = std::mem::replace(&mut self.entries, vec![(0, EMPTY_SLOT); new_cap]);
        for &(fp, slot) in &old {
            if slot != EMPTY_SLOT {
                self.place(fp, slot);
            }
        }
    }

    /// Places `(fp, slot)` at the first empty bucket from fp's home —
    /// the raw loop insert and rehash share. The caller MUST guarantee
    /// an empty bucket exists ([`Self::reserve_for_insert`] before an
    /// insert, fresh oversized capacity during a grow): that guarantee
    /// is the loop's termination condition. A same-fp entry with a
    /// DIFFERENT slot is a different key (a fingerprint collision
    /// between distinct names) and simply coexists on the chain — read
    /// paths tell them apart through the host's live-column check.
    fn place(&mut self, fp: u32, slot: u32) {
        let mask = self.entries.len() - 1;
        let mut i = fp as usize & mask;
        while self.entries[i].1 != EMPTY_SLOT {
            i = (i + 1) & mask;
        }
        self.entries[i] = (fp, slot);
    }

    /// Inserts `(fp, slot)` under the load guard (the only path that
    /// grows `len`). Real call sites never insert an exact `(fp, slot)`
    /// pair twice — the store's `UNIQUE(dir, name)` keeps keys unique
    /// and re-keying removes first — so duplicates cannot arise to
    /// double-count `len`.
    fn insert(&mut self, fp: u32, slot: u32) {
        self.reserve_for_insert();
        self.place(fp, slot);
        self.len += 1;
    }

    /// Walks fp's probe chain and returns the first stored slot for
    /// which `is_match` holds — the READ path of the identity contract:
    /// the host closure runs the live-columns final check
    /// (`parents[slot] == parent && arena name bytes match`), so
    /// fingerprint collisions between distinct names are sorted apart
    /// here and a wrong entry can never be returned. `None` at the
    /// chain's end (first empty bucket). Terminates per the struct
    /// docs: an empty bucket always exists in the ring.
    fn get_matching(&self, fp: u32, is_match: impl Fn(u32) -> bool) -> Option<u32> {
        if self.entries.is_empty() {
            return None;
        }
        let mask = self.entries.len() - 1;
        let mut i = fp as usize & mask;
        loop {
            let (entry_fp, entry_slot) = self.entries[i];
            if entry_slot == EMPTY_SLOT {
                return None; // fp's chain ended without a verified match
            }
            if entry_fp == fp && is_match(entry_slot) {
                return Some(entry_slot);
            }
            i = (i + 1) & mask;
        }
    }

    /// Removes the entry `(fp, slot)` — identity is EXACTLY that pair,
    /// no host columns read (the MUTATION path of the identity contract:
    /// callers run this after the identity columns may already hold
    /// other data). The freed bucket is repaired by backward-shift
    /// deletion: each later entry on the chain whose probe path crossed
    /// the hole moves back into it, so no survivor ends up stranded
    /// behind an empty bucket. Returns whether an entry was found.
    fn remove_by_slot(&mut self, fp: u32, slot: u32) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let mask = self.entries.len() - 1;
        let mut i = fp as usize & mask;
        // Find the victim: the (fp, slot) pair on fp's chain.
        loop {
            let (entry_fp, entry_slot) = self.entries[i];
            if entry_slot == EMPTY_SLOT {
                return false; // fp's chain ended before the pair
            }
            if entry_fp == fp && entry_slot == slot {
                break;
            }
            i = (i + 1) & mask;
        }
        self.entries[i] = (0, EMPTY_SLOT);
        self.len -= 1;
        // Backward shift: scan forward from the hole, pulling back every
        // entry whose own probe path crossed it — the hole lies on the
        // path iff the distance hole→entry is ≤ the entry's displacement
        // from its home (equal means the hole IS its home). Entries that
        // probe past the hole without crossing it stay put; moving those
        // would break THEIR chains. The scan stops at the next empty
        // bucket (the never-full invariant guarantees one).
        let cap = self.entries.len();
        let mut hole = i;
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let (entry_fp, entry_slot) = self.entries[j];
            if entry_slot == EMPTY_SLOT {
                break;
            }
            let home = entry_fp as usize & mask;
            if (j + cap - hole) & mask <= (j + cap - home) & mask {
                self.entries[hole] = (entry_fp, entry_slot);
                self.entries[j] = (0, EMPTY_SLOT);
                hole = j;
            }
        }
        true
    }

    /// Rewrites the stored slot of the entry `(fp, old_slot)` to
    /// `new_slot` IN PLACE — swap-remove's tail relocation: the entry
    /// stays at its bucket, so no probe chain is disturbed at all, not
    /// even a backward shift. Identity is the (fingerprint, slot) pair,
    /// never host columns. A rebind that finds nothing (possible only
    /// under corruption) degrades to an insert — mirroring
    /// `HashMap::insert`'s tolerance so the mapping self-heals at the
    /// next wholesale reload instead of leaking a permanent miss.
    fn rebind(&mut self, fp: u32, old_slot: u32, new_slot: u32) {
        if self.entries.is_empty() {
            self.insert(fp, new_slot);
            return;
        }
        let mask = self.entries.len() - 1;
        let mut i = fp as usize & mask;
        loop {
            let (entry_fp, entry_slot) = self.entries[i];
            if entry_slot == EMPTY_SLOT {
                self.insert(fp, new_slot); // the tolerated fallback
                return;
            }
            if entry_fp == fp && entry_slot == old_slot {
                self.entries[i].1 = new_slot;
                return;
            }
            i = (i + 1) & mask;
        }
    }

    /// Longest displacement of any live entry from its home bucket —
    /// i.e. the worst-case successful-search probe length. The
    /// anti-clustering test reads it as its clustering guard.
    #[cfg(test)]
    fn max_probe_displacement(&self) -> usize {
        if self.entries.is_empty() {
            return 0;
        }
        let cap = self.entries.len();
        let mask = cap - 1;
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.1 != EMPTY_SLOT)
            .map(|(i, (fp, _))| (i + cap - (*fp as usize & mask)) & mask)
            .max()
            .unwrap_or(0)
    }
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

/// [`HotIndex::mem_breakdown`]'s payload: the estimated live bytes each
/// component of the hot index accounts for (see that method for what the
/// estimates deliberately exclude).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemBreakdown {
    /// File slots (rows in the parallel columns).
    pub slots: u64,
    /// Rows in the interned dir tree.
    pub dir_rows: u64,
    /// THE byte arena (all interned strings).
    pub arena_bytes: u64,
    /// The four arena-reference columns.
    pub slot_refs_bytes: u64,
    /// The scalar per-slot columns.
    pub slot_cols_bytes: u64,
    pub slot_by_id_bytes: u64,
    pub slot_by_parent_name_bytes: u64,
    /// The dir tree map itself.
    pub dir_tree_bytes: u64,
    /// The dir (parent, name-hash) secondary index.
    pub dir_index_bytes: u64,
    /// Extension text map, including the strings' heap.
    pub exts_bytes: u64,
    /// Sum of all of the above.
    pub total_estimated: u64,
}

/// capacity × (key + value) bytes — the live-entry estimate the breakdown
/// uses for hash maps (allocator headers and control bytes excluded by
/// design).
/// Narrowing guards for the compact columns (Task 5): our own upserts
/// only ever produce dense rowids far below u32::MAX, so a value that
/// does not fit means a foreign/edited database — refuse it.
fn narrow_id(id: FileId) -> Result<u32> {
    u32::try_from(id).map_err(|_| {
        SynsoError::BadQuery(format!(
            "row id {id} exceeds u32 — the compact hot index cannot hold it"
        ))
    })
}

fn narrow_ext(ext: i64) -> Result<i32> {
    i32::try_from(ext).map_err(|_| {
        SynsoError::BadQuery(format!(
            "extension id {ext} exceeds i32 — the compact hot index cannot hold it"
        ))
    })
}

fn est_map<K, V>(m: &HashMap<K, V>) -> u64 {
    m.capacity() as u64 * (std::mem::size_of::<K>() + std::mem::size_of::<V>()) as u64
}

/// Dead-byte floor before an arena compaction is worth its pause: below
/// this the absolute waste is noise for any realistic index.
const COMPACT_MIN_WASTE: u64 = 64 * 1024 * 1024;

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
    /// rows that already hold a slot (same id = update in place). DENSE
    /// direct-indexed array (Task 5 item 2): `slot_by_id[id] = slot + 1`,
    /// 0 = hole — row ids from our own upserts are near-contiguous, so a
    /// Vec answers in one cache line where the old HashMap spent 59MB on
    /// 2.1M entries. Guarded by [`narrow_id`] (u32 ids).
    slot_by_id: Vec<u32>,
    /// Secondary index: (containing dir id, name) → slot, EXACT-case,
    /// arena-verified on read — the open-addressed [`NameSlotTable`]
    /// (v1.3: replaced the std HashMap here, whose hash-map overhead on
    /// a two-integer key estimated ≈60MB at 2.1M entries; see that
    /// struct's docs for the identity contract its operations run
    /// under).
    slot_by_parent_name: NameSlotTable,
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
    /// `files` row id per slot (u32: SQLite rowids from our own upserts
    /// are dense and far below u32::MAX — the load guard rejects any
    /// database that violates that; see [`HotIndex::load`]). Search hits
    /// widen back to [`FileId`] at the accessor.
    ids: Vec<u32>,
    /// Containing directory's `dirs` row id per slot (u32, same guard).
    parents: Vec<u32>,
    /// File size in bytes per slot (0 for directory entries).
    sizes: Vec<u64>,
    /// mtime in Unix-epoch seconds per slot.
    mtimes: Vec<i64>,
    /// Interned extension id per slot (0 = no extension; i32 — the
    /// extensions table is tiny).
    ext_ids: Vec<i32>,
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
    /// Entries (files rows, dir entries, dirs-table nodes) whose name
    /// exceeded the 64 KiB a [`StrRef`] can name and were skipped instead
    /// of interned. Pure defense — filesystem names stay orders of
    /// magnitude below it, so a nonzero count means a foreign or corrupt
    /// database; logged once per load/upsert, never per row.
    skipped_oversized: u64,
    /// True once an initial full load completed.
    warm: bool,
}

impl HotIndex {
    /// Appends `s` to the byte arena and returns its ref. `None` — with
    /// the arena untouched — when `s` exceeds the 64 KiB a [`StrRef`] can
    /// name: a pure-defense guard (filesystem names stay far below it),
    /// tallied in [`HotIndex::skipped_oversized`] so callers skip the row
    /// and the skip is reported once per batch, never per row.
    fn put(&mut self, s: &str) -> Option<StrRef> {
        let len = s.len();
        if len > u16::MAX as usize {
            self.skipped_oversized += 1;
            return None;
        }
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        Some(StrRef {
            start,
            len: len as u16,
        })
    }

    /// The arena slice a ref names (valid UTF-8 by construction — only
    /// `&str` input ever enters the arena; the `unwrap_or` keeps the
    /// library's no-panic rule even against hypothetical corruption).
    #[inline]
    fn str_of(&self, r: StrRef) -> &str {
        let end = r.start as usize + r.len as usize;
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
                hot.push(rec).expect("synthetic ids fit u32");
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
            hot.push(rec).expect("synthetic ids fit u32");
        }
        hot
    }

    /// Interns one synthetic `dirs` row (the shared shape of
    /// [`HotIndex::load`]'s dir loop).
    fn intern_dir(&mut self, id: FileId, parent: Option<FileId>, name: String) {
        let name_ref = self
            .put(&name)
            .expect("synthetic names fit StrRef");
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
            let Some(name_ref) = index.put(&name) else {
                continue; // 64 KiB guard: the dir node is skipped, counted by put
            };
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
        let files = store.all_files()?;
        // Pre-size the slot table from the exact row count (×1.4 → a
        // power of two): the load batches its inserts, and a right-sized
        // table absorbs them all without a single rehash.
        index.slot_by_parent_name = NameSlotTable::new(files.len());
        for rec in files {
            index.push(rec)?;
        }
        // One summary line per load for the 64 KiB skips (push / the dir
        // loop skip silently; the tally is the count).
        if index.skipped_oversized > 0 {
            log::warn!(
                "synso: load skipped {} row(s)/dir(s) with names over 64 KiB (StrRef guard)",
                index.skipped_oversized
            );
        }
        Ok(index)
    }

    /// Appends one `files` row's columns to the arenas and secondary
    /// indexes (whole-table load and incremental watcher pushes).
    /// Pinyin keys equal to the lowercased name (every pure-ASCII name)
    /// alias the name's ref — no second copy in the arena.
    ///
    /// A row whose name (or either pinyin key) exceeds the 64 KiB ref
    /// guard is SKIPPED, not an error: tallied by `put` and reported once
    /// per batch, this returns `Ok(())` without touching any column, so
    /// the row is absent from the index until a reload that can hold it.
    ///
    /// Errors only on the compact-column guards: a row id, dir id or
    /// extension id beyond its narrowed type means the database predates
    /// or bypassed our own upserts — refusing beats wrapping.
    fn push(&mut self, rec: FileRecord) -> Result<()> {
        let lower = rec.name.to_lowercase();
        // 64 KiB guard: any None (counted by put) skips the whole row
        // BEFORE any mutation — the same guards-before-mutation rule as
        // the narrow_id checks below; a skip adds no logical bytes.
        let Some(lower_ref) = self.put(&lower) else {
            return Ok(());
        };
        let Some(orig_ref) = self.put(&rec.name) else {
            return Ok(());
        };
        let py_full_ref = if rec.py_full == lower {
            lower_ref
        } else {
            let Some(r) = self.put(&rec.py_full) else {
                return Ok(());
            };
            r
        };
        let py_init_ref = if rec.py_init == lower {
            lower_ref
        } else {
            let Some(r) = self.put(&rec.py_init) else {
                return Ok(());
            };
            r
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
        // Narrowing guards BEFORE any mutation (review fix #5): a failed
        // guard mid-push left the columns length-desynced and poisoned
        // every later file_id(slot) in the watcher path.
        let id32 = narrow_id(rec.id)?;
        let parent32 = narrow_id(rec.parent)?;
        let ext32 = narrow_ext(rec.ext_id)?;
        self.interned_bytes += logical;
        if rec.is_dir {
            self.dir_count += 1;
        } else {
            self.file_count += 1;
        }
        let slot = self.lower_refs.len();
        if (id32 as usize) >= self.slot_by_id.len() {
            self.slot_by_id.resize(id32 as usize + 1, 0);
        }
        self.slot_by_id[id32 as usize] = (slot as u32) + 1;
        self.slot_by_parent_name
            .insert(parent_name_fp(rec.parent, &rec.name), slot as u32);
        self.lower_refs.push(lower_ref);
        self.orig_refs.push(orig_ref);
        self.py_full_refs.push(py_full_ref);
        self.py_init_refs.push(py_init_ref);
        self.logical_bytes.push(logical as u32);
        self.ids.push(id32);
        self.parents.push(parent32);
        self.sizes.push(rec.size);
        self.mtimes.push(rec.mtime);
        self.ext_ids.push(ext32);
        self.is_dirs.push(rec.is_dir);
        Ok(())
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
        let skips_before = self.skipped_oversized;
        // Ancestor chain first, so the new/updated slot's paths resolve.
        for (id, parent, name) in store.dir_ancestors(rec.parent)? {
            if !self.dirs.contains_key(&id) {
                let Some(name_ref) = self.put(&name) else {
                    continue; // 64 KiB guard: the dir node is skipped, counted by put
                };
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
        if let Some(slot) = self.slot_of_id(rec.id) {
            self.update_slot(slot, rec)?;
        } else {
            self.push(rec.clone())?;
        }
        // One summary line per call for the 64 KiB skips (push and the
        // dir loop skip silently; the tally is the count).
        if self.skipped_oversized > skips_before {
            log::warn!(
                "synso: upsert skipped {} row(s)/dir(s) with names over 64 KiB (StrRef guard)",
                self.skipped_oversized - skips_before
            );
        }
        Ok(())
    }

    /// Updates an existing slot in place (same `files` row id), keeping
    /// counters, byte budget and secondary indexes coherent. New strings
    /// are APPENDED to the arena (the old bytes become unreachable until
    /// the next wholesale reload compacts them — bounded by rename churn).
    ///
    /// An incoming name (or either pinyin key) beyond the 64 KiB ref guard
    /// skips the update entirely — counted by `put`, the slot keeps its
    /// old spelling, and the next wholesale reload re-decides the row.
    fn update_slot(&mut self, slot: usize, rec: &FileRecord) -> Result<()> {
        let lower = rec.name.to_lowercase();
        let Some(lower_ref) = self.put(&lower) else {
            return Ok(());
        };
        let Some(orig_ref) = self.put(&rec.name) else {
            return Ok(());
        };
        let py_full_ref = if rec.py_full == lower {
            lower_ref
        } else {
            let Some(r) = self.put(&rec.py_full) else {
                return Ok(());
            };
            r
        };
        let py_init_ref = if rec.py_init == lower {
            lower_ref
        } else {
            let Some(r) = self.put(&rec.py_init) else {
                return Ok(());
            };
            r
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
        // The slot-table entry is located by (fingerprint, slot) AFTER
        // the columns below are overwritten (builder-check 裁定 1:
        // mutation paths never read host columns), so the OLD key's
        // hash is captured while the columns still hold the old
        // spelling.
        let old_h = parent_name_hash(
            self.parents[slot] as FileId,
            self.str_of(self.orig_refs[slot]),
        );
        // Guards before mutation (review fix #5 — see push).
        let ext32 = narrow_ext(rec.ext_id)?;
        let parent32 = narrow_id(rec.parent)?;
        self.lower_refs[slot] = lower_ref;
        self.orig_refs[slot] = orig_ref;
        self.py_full_refs[slot] = py_full_ref;
        self.py_init_refs[slot] = py_init_ref;
        self.logical_bytes[slot] = logical as u32;
        self.sizes[slot] = rec.size;
        self.mtimes[slot] = rec.mtime;
        self.ext_ids[slot] = ext32;
        self.parents[slot] = parent32;
        let new_h = parent_name_hash(rec.parent, &rec.name);
        if old_h != new_h {
            // Identity = (fingerprint, slot), never the overwritten
            // columns; an unchanged key (equal hashes) keeps its entry
            // untouched — it already maps this slot.
            self.slot_by_parent_name
                .remove_by_slot((old_h >> 32) as u32, slot as u32);
            self.slot_by_parent_name
                .insert((new_h >> 32) as u32, slot as u32);
        }
        Ok(())
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
            .filter(|&slot| doomed_set.contains(&(self.parents[slot] as FileId)))
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
    /// [`HotIndex::remove_slot_at`] for the mechanics. The lookup rides
    /// the READ path of the identity contract: [`HotIndex::slot_in_dir`]
    /// verifies parent and arena name against live columns (the old
    /// HashMap get verified nothing past its 64-bit hash key).
    fn remove_slot(&mut self, parent_id: FileId, name: &str) -> Option<FileId> {
        let slot = self.slot_in_dir(parent_id, name)?;
        Some(self.remove_slot_at(slot))
    }

    /// Swap-removes the slot at `slot`, keeping every arena, counter, byte
    /// budget and secondary index consistent — the last slot moves into the
    /// hole and its index entries are repointed. Returns the removed row id.
    fn remove_slot_at(&mut self, slot: usize) -> FileId {
        let removed_id = self.ids[slot] as FileId;
        let last = self.lower_refs.len() - 1;
        // Fingerprints captured while the identity columns are still
        // intact: both slot-table mutations below run AFTER the
        // swap-pop, when `parents`/`orig_refs` at these positions
        // already hold other data (builder-check 裁定 1 — the table
        // locates entries by (fingerprint, slot) alone).
        let fp_victim =
            parent_name_fp(self.parents[slot] as FileId, self.str_of(self.orig_refs[slot]));

        if self.is_dirs[slot] {
            self.dir_count = self.dir_count.saturating_sub(1);
        } else {
            self.file_count = self.file_count.saturating_sub(1);
        }
        self.interned_bytes = self
            .interned_bytes
            .saturating_sub(self.logical_bytes[slot] as u64);

        // Bookkeeping for the slot that moves into the hole (if any):
        // its row id (slot_by_id repoint) and its key's fingerprint
        // (slot-table rebind) — both read while the columns are intact.
        let moved = if slot != last {
            Some((
                self.ids[last] as FileId,
                parent_name_fp(
                    self.parents[last] as FileId,
                    self.str_of(self.orig_refs[last]),
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

        if let Ok(id32) = u32::try_from(removed_id) {
            if (id32 as usize) < self.slot_by_id.len() {
                self.slot_by_id[id32 as usize] = 0;
            }
        }
        self.slot_by_parent_name
            .remove_by_slot(fp_victim, slot as u32);
        if let Some((moved_id, fp_moved)) = moved {
            if let Ok(id32) = u32::try_from(moved_id) {
                if (id32 as usize) < self.slot_by_id.len() {
                    self.slot_by_id[id32 as usize] = (slot as u32) + 1;
                }
            }
            // In-place slot-number rewrite (builder-check 裁定 1): the
            // moved entry stays at its bucket — no probe chain is
            // disturbed, not even a backward shift.
            self.slot_by_parent_name
                .rebind(fp_moved, last as u32, slot as u32);
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
        self.ids[slot] as FileId
    }

    /// The slot whose `files` row id is `id` (O(1) reverse lookup), or
    /// `None` when the row is not indexed. The dedup groups view (Task 9)
    /// materializes stored groups' members through this.
    pub fn slot_of_id(&self, id: FileId) -> Option<usize> {
        let id32 = u32::try_from(id).ok()? as usize;
        // slot+1 encoding: 0 = hole.
        match *self.slot_by_id.get(id32)? {
            0 => None,
            encoded => Some(encoded as usize - 1),
        }
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
    /// READ path of the identity contract: the fingerprint chain narrows
    /// the candidates, then LIVE columns give the final identity check
    /// (parent id AND arena name bytes — the old HashMap lookup trusted
    /// a bare 64-bit hash get, so this is strictly stricter at the same
    /// result).
    pub(crate) fn slot_in_dir(&self, parent: FileId, name: &str) -> Option<usize> {
        let fp = parent_name_fp(parent, name);
        self.slot_by_parent_name
            .get_matching(fp, |slot| {
                self.parents[slot as usize] as FileId == parent
                    && self.str_of(self.orig_refs[slot as usize]) == name
            })
            .map(|slot| slot as usize)
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
        self.orig_refs[slot].len as u64 + self.dir_chain_bytes(memo, self.parents[slot] as FileId)
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
        let mut current = self.parents[slot] as FileId;
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

    /// The arena buffer's CURRENT size — after churn this exceeds
    /// [`HotIndex::interned_bytes`] by the dead bytes removals stranded.
    pub(crate) fn arena_len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Rebuilds the byte arena with ONLY the bytes live refs point at,
    /// remapping every ref (aliasing preserved via dedupe). The arena is
    /// append-only under incremental updates — upserts append, removals
    /// strand holes — and on an all-day instance 112MB of content was
    /// observed ballooned to 1.78GB. Returns the bytes reclaimed.
    pub(crate) fn compact_arena(&mut self) -> u64 {
        let before = self.bytes.len() as u64;
        let mut fresh: Vec<u8> = Vec::with_capacity(self.interned_bytes as usize);
        // old ref -> new ref, so aliased ranges (pinyin aliasing the
        // lowercased name) copy once.
        let mut seen: HashMap<StrRef, StrRef> = HashMap::new();
        let bytes = &self.bytes;

        let mut remap_vec = |refs: &mut Vec<StrRef>| {
            for r in refs.iter_mut() {
                let nr = *seen.entry(*r).or_insert_with(|| {
                    let start = fresh.len() as u32;
                    fresh.extend_from_slice(
                        &bytes[r.start as usize..(r.start + r.len as u32) as usize],
                    );
                    StrRef { start, len: r.len }
                });
                *r = nr;
            }
        };

        remap_vec(&mut self.lower_refs);
        remap_vec(&mut self.orig_refs);
        remap_vec(&mut self.py_full_refs);
        remap_vec(&mut self.py_init_refs);
        for entry in self.dirs.values_mut() {
            let nr = *seen.entry(entry.name).or_insert_with(|| {
                let start = fresh.len() as u32;
                let r = entry.name;
                fresh.extend_from_slice(&bytes[r.start as usize..(r.start + r.len as u32) as usize]);
                StrRef { start, len: r.len }
            });
            entry.name = nr;
        }

        self.bytes = fresh;
        before - self.bytes.len() as u64
    }

    /// Compacts the arena when churn has stranded enough dead bytes:
    /// more than [`COMPACT_MIN_WASTE`] absolute AND more than half the
    /// live content. Called after every watcher batch — the comparison
    /// is two integers, the rebuild is rare and bounded.
    pub(crate) fn maybe_compact_arena(&mut self) {
        let len = self.bytes.len() as u64;
        if len > self.interned_bytes + COMPACT_MIN_WASTE && len > self.interned_bytes * 3 / 2 {
            let reclaimed = self.compact_arena();
            log::info!(
                "synso: arena compacted — {reclaimed} bytes reclaimed, now {} for {} live",
                self.bytes.len(),
                self.interned_bytes
            );
        }
    }

    /// Per-structure memory breakdown (v1.1 Task 6 diagnostics): the
    /// estimated LIVE bytes each component of the hot index accounts
    /// for. Vecs are counted as `len × element size` (capacity overshoot
    /// excluded); hash maps as `capacity × (key + value)` — allocator
    /// headers, hashbrown control bytes and fragmentation are all
    /// deliberately NOT in these numbers, so `total_estimated` vs. the
    /// process working set separates "live structures" from "overhead /
    /// high water" at a glance in the memlog output.
    pub fn mem_breakdown(&self) -> MemBreakdown {
        let slots = self.ids.len() as u64;
        let arena_bytes = self.arena_len();
        // Four arena-reference columns (lower/orig/pinyin full/init).
        let slot_refs_bytes = slots * 4 * std::mem::size_of::<StrRef>() as u64;
        // Scalar columns: logical_bytes, ids, parents, sizes, mtimes,
        // ext_ids, is_dirs.
        let slot_cols_bytes = slots
            * (std::mem::size_of::<u32>() // logical_bytes
                + std::mem::size_of::<u32>() // ids (narrowed, Task 5)
                + std::mem::size_of::<u32>() // parents (narrowed)
                + std::mem::size_of::<u64>() // sizes
                + std::mem::size_of::<i64>() // mtimes
                + std::mem::size_of::<i32>() // ext_ids (narrowed)
                + std::mem::size_of::<bool>()) as u64; // is_dirs
        let slot_by_id_bytes = self.slot_by_id.len() as u64 * std::mem::size_of::<u32>() as u64;
        // The open-addressed table counts like the vecs it is: len × 8
        // (the power-of-two capacity is the live allocation).
        let slot_by_parent_name_bytes = self.slot_by_parent_name.mem_bytes();
        let dir_tree_bytes = est_map(&self.dirs);
        let dir_index_bytes = est_map(&self.dir_by_parent_name);
        let exts_bytes =
            est_map(&self.exts) + self.exts.values().map(|s| s.capacity() as u64).sum::<u64>();
        let total_estimated = arena_bytes
            + slot_refs_bytes
            + slot_cols_bytes
            + slot_by_id_bytes
            + slot_by_parent_name_bytes
            + dir_tree_bytes
            + dir_index_bytes
            + exts_bytes;
        MemBreakdown {
            slots,
            dir_rows: self.dirs.len() as u64,
            arena_bytes,
            slot_refs_bytes,
            slot_cols_bytes,
            slot_by_id_bytes,
            slot_by_parent_name_bytes,
            dir_tree_bytes,
            dir_index_bytes,
            exts_bytes,
            total_estimated,
        }
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
        self.parents[slot] as FileId
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
        self.ext_ids[slot] as i64
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
    /// id a search hit for that dir would carry. `None` when nothing
    /// matches: an unknown segment, a single-segment path naming no entry
    /// (the walk root itself is never a `files` row), or the empty
    /// virtual-root path `""`, which names no single entry. The inverse of
    /// [`HotIndex::path_of`].
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
        let fresh_slot = hot.slot_of_id(fresh.id).expect("test may index");
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
        assert_eq!(hot.size(hot.slot_of_id(fresh.id).unwrap()), 99);

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
        assert!(hot.slot_of_id(b.id).is_none());
        assert!(hot.slot_of_id(c.id).is_none());
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
            let slot = hot.slot_of_id(id).expect("survivor resolves");
            assert_eq!(hot.name_original(slot), name);
            assert_eq!(hot.file_id(slot), id);
            assert_eq!(
                hot.slot_in_dir(hot.parent_dir(slot), name),
                Some(slot),
                "the (parent, name) table points at the survivor's slot"
            );
        }
        assert!(hot.slot_of_id(ids[1]).is_none());
    }

    /// The breakdown's invariants (v1.1 diagnostics): every component of
    /// a populated index is non-zero, the arena matches the authoritative
    /// `interned_bytes`, and `total_estimated` is exactly the sum of the
    /// parts — so the memlog line's live-vs-working-set gap is real, not
    /// arithmetic drift.
    #[test]
    fn mem_breakdown_sums_its_parts() {
        let hot = HotIndex::synthetic(1000);
        let b = hot.mem_breakdown();

        // synthetic's slots include its dir entries — the invariant is
        // the slot count, not the file count.
        assert_eq!(b.slots, hot.slot_count() as u64);
        assert!(b.slots >= 1000);
        assert!(b.dir_rows > 0);
        assert_eq!(b.arena_bytes, hot.interned_bytes());
        assert!(b.slot_refs_bytes > 0);
        assert!(b.slot_cols_bytes > 0);
        assert!(b.slot_by_id_bytes > 0);
        assert!(b.slot_by_parent_name_bytes > 0);
        assert!(b.dir_tree_bytes > 0);
        assert!(b.dir_index_bytes > 0);
        assert!(b.exts_bytes > 0);
        assert_eq!(
            b.total_estimated,
            b.arena_bytes
                + b.slot_refs_bytes
                + b.slot_cols_bytes
                + b.slot_by_id_bytes
                + b.slot_by_parent_name_bytes
                + b.dir_tree_bytes
                + b.dir_index_bytes
                + b.exts_bytes
        );
    }

    /// The arena compaction contract (v1.2 memory fix): churn leaves the
    /// arena append-only — upserts append, removals leave holes — and on
    /// an all-day instance 112MB of content was observed ballooned to
    /// 1.78GB. `compact_arena` must rebuild the buffer with ONLY the
    /// bytes live refs point at (aliasing preserved), remap every ref,
    /// and leave every survivor fully functional.
    #[test]
    fn compact_arena_reclaims_holes_and_keeps_lookups_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        // Fill, then churn: upsert a wave of files, remove most of them,
        // upsert another wave. Every removed name's bytes stay in the
        // arena as holes.
        fn upsert(store: &mut Store, dir: &str, name: &str, id: u8) {
            let mut r = rec(name, false);
            r.name = format!("{name}{id}");
            store.upsert_file(&[dir], &mut r).unwrap();
        }
        for i in 0..50u8 {
            upsert(&mut store, "docs", "keep", i);
            upsert(&mut store, "docs", "drop", i);
        }
        let mut hot = HotIndex::load(&store).unwrap();
        for i in 0..50u8 {
            hot.remove_by_dir_name(&["docs"], &format!("drop{i}"))
                .expect("each drop row removes");
        }
        let live_interned = hot.interned_bytes();
        // Two holes per removed name (lower + original); the arena must
        // currently exceed the logical content.
        let holed = hot.arena_len();
        assert!(
            holed > live_interned,
            "churn must leave holes: arena {holed} > interned {live_interned}"
        );

        let reclaimed = hot.compact_arena();
        assert!(reclaimed > 0, "compaction reclaims the holes");
        let compact = hot.arena_len();
        assert!(
            compact <= live_interned + 1024,
            "compacted arena {compact} must match live content {live_interned} (±alias bookkeeping)"
        );

        // Every survivor still resolves end to end: name lookups, the
        // original-case spelling, pinyin keys, and slot-by-id.
        for i in 0..50u8 {
            let name = format!("keep{i}");
            let docs = hot.dir_child(None, "docs").expect("docs still interned");
            let slot = hot
                .slot_in_dir(docs, &name)
                .unwrap_or_else(|| panic!("{name} must resolve after compaction"));
            assert!(hot.name_original(slot).ends_with(&name));
            assert_eq!(hot.name_lower(slot), name);
        }
    }

    /// The compact u32 columns cannot hold a row id beyond u32::MAX —
    /// load must refuse such a database with an error, never panic or
    /// silently wrap (Task 5 Review Focus: the narrowing guard).
    #[test]
    fn id_beyond_u32_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        // One sane row so the schema is exercised, then one poisoned row
        // with a huge id straight through SQL (the store's own upsert
        // never produces such ids — this simulates a foreign/edited DB).
        let mut a = rec("a.txt", false);
        store.upsert_file(&["docs"], &mut a).unwrap();
        store
            .conn_mut()
            .execute(
                "INSERT INTO files (id, dir, name, size, mtime, ctime, atime, ext_id, is_dir, scan_gen) \
                 VALUES (4294967296, 1, 'poison.txt', 1, 1, 1, 1, 0, 0, 1)",
                [],
            )
            .unwrap();
        let err = HotIndex::load(&store);
        assert!(
            err.is_err(),
            "a row id beyond u32::MAX must fail the load with an error"
        );
        // Without the poison the same database loads fine.
        store
            .conn_mut()
            .execute("DELETE FROM files WHERE id = 4294967296", [])
            .unwrap();
        assert!(HotIndex::load(&store).is_ok());
    }

    /// The dense slot_by_id (Task 5 item 2): row ids are near-contiguous,
    /// so a direct-indexed Vec answers lookups — and holes (deleted ids)
    /// must read as absent, exactly like the HashMap did.
    #[test]
    fn slot_by_id_dense_with_holes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        // ids 1..=10 with 3,5,9 deleted by hand (gaps that never re-fill
        // until SQLite reuses them — the dense array must treat them as
        // EMPTY, and a later upsert AT a reused id must repoint it).
        let mut kept = Vec::new();
        for i in 1..=10i64 {
            let name = format!("f{i}.txt");
            let mut r = rec(&name, false);
            store.upsert_file(&["docs"], &mut r).unwrap();
            kept.push(r.id);
        }
        for id in [kept[2], kept[4], kept[8]] {
            store
                .conn_mut()
                .execute("DELETE FROM files WHERE id = ?1", [id])
                .unwrap();
        }
        let mut hot = HotIndex::load(&store).unwrap();
        for (i, id) in kept.iter().enumerate() {
            let expected = !(i == 2 || i == 4 || i == 8);
            assert_eq!(
                hot.slot_of_id(*id).is_some(),
                expected,
                "id {id} (index {i}) presence must be {expected}"
            );
            if let Some(slot) = hot.slot_of_id(*id) {
                assert_eq!(hot.file_id(slot), *id);
            }
        }
        // A fresh upsert lands at its id even inside a hole region.
        let mut again = rec("f3-reborn.txt", false);
        store.upsert_file(&["docs"], &mut again).unwrap();
        // (SQLite may or may not reuse id kept[2]; either way the lookup
        // must agree with the push.)
        hot.upsert_slot(&store, &again).unwrap();
        let slot = hot.slot_of_id(again.id).expect("fresh upsert resolvable");
        assert_eq!(hot.file_id(slot), again.id);
    }

    /// [`NameSlotTable`]'s four-op contract in miniature (v1.3 Task 1): a
    /// DELIBERATE collision cluster — four fingerprints sharing home
    /// bucket 0 on an 8-bucket table — must be fully findable, and
    /// deleting a MIDDLE cluster entry must backward-shift the chain's
    /// tail so the entries behind the hole stay reachable (the classic
    /// open-addressing deletion hazard).
    #[test]
    fn name_slot_table_roundtrip_and_shift() {
        let mut t = NameSlotTable::new(0);
        // fps 0/8/16/24 all mask to home bucket 0: one probe chain.
        let cluster = [(0u32, 10u32), (8, 11), (16, 12), (24, 13)];
        for (fp, slot) in cluster {
            t.insert(fp, slot);
        }
        assert_eq!(t.len, 4);
        for (fp, slot) in cluster {
            assert_eq!(
                t.get_matching(fp, |s| s == slot),
                Some(slot),
                "cluster entry (fp {fp}, slot {slot}) must be findable"
            );
        }
        // Delete a middle cluster entry: entries past the hole relocate
        // back, so no survivor ends up stranded behind an empty bucket.
        assert!(t.remove_by_slot(8, 11));
        assert_eq!(t.len, 3);
        assert_eq!(t.get_matching(8, |s| s == 11), None, "deleted key is gone");
        for (fp, slot) in [(0, 10), (16, 12), (24, 13)] {
            assert_eq!(
                t.get_matching(fp, |s| s == slot),
                Some(slot),
                "survivor (fp {fp}, slot {slot}) still reachable after the shift"
            );
        }
        // Removing a pair that never existed is a benign false.
        assert!(!t.remove_by_slot(8, 11));
        assert_eq!(t.len, 3);
    }

    /// [`NameSlotTable::rebind`] rewrites the stored slot number IN PLACE
    /// — swap-remove's tail relocation — without touching `len` or any
    /// neighboring entry; a rebind of a pair that never existed degrades
    /// to insert (the HashMap::insert tolerance) so the mapping
    /// self-heals.
    #[test]
    fn name_slot_table_rebind_in_place() {
        let mut t = NameSlotTable::new(0);
        t.insert(100, 7);
        t.insert(200, 8); // separate chain, must be untouched
        t.rebind(100, 7, 42);
        assert_eq!(t.get_matching(100, |s| s == 42), Some(42), "new slot resolves");
        assert_eq!(t.get_matching(100, |s| s == 7), None, "old slot is gone");
        assert_eq!(t.get_matching(200, |s| s == 8), Some(8), "neighbor untouched");
        assert_eq!(t.len, 2, "rebind moves an entry, it does not add one");
        t.rebind(300, 5, 9);
        assert_eq!(t.get_matching(300, |s| s == 9), Some(9));
        assert_eq!(t.len, 3);
    }

    /// Same-fingerprint collision (spec: "指纹碰撞走验证兜底"): two
    /// entries sharing ONE fingerprint but holding distinct slots
    /// coexist on one probe chain — the read walk hands each same-fp
    /// candidate to the host predicate, so the entry whose host check
    /// fails is SKIPPED (the verify-fallback branch) and the one it
    /// matches still resolves. Removing one entry steps past its
    /// same-fp neighbor (the victim is matched by the stored slot
    /// number, not the first fingerprint hit), and the backward shift
    /// keeps the survivor reachable.
    #[test]
    fn name_slot_table_same_fingerprint_collision() {
        let mut t = NameSlotTable::new(0);
        t.insert(777, 10);
        t.insert(777, 11); // same fp, distinct slots: collides by design
        assert_eq!(t.len, 2);
        assert_eq!(
            t.get_matching(777, |s| s == 11),
            Some(11),
            "host check skips the same-fp neighbor whose slot mismatches"
        );
        assert_eq!(t.get_matching(777, |s| s == 10), Some(10));
        assert!(t.remove_by_slot(777, 10));
        assert_eq!(t.len, 1);
        assert_eq!(
            t.get_matching(777, |s| s == 11),
            Some(11),
            "survivor stays reachable after the removal's backward shift"
        );
        assert_eq!(t.get_matching(777, |s| s == 10), None, "removed pair is gone");
    }

    /// Growth rehash (Review Focus 3): 100k synthetic fingerprints from an
    /// 8-bucket start cross every doubling threshold; after the rehashes
    /// every key must stay findable and no slot may be reachable twice.
    #[test]
    fn name_slot_table_grow_preserves_all() {
        let mut t = NameSlotTable::new(0);
        let n = 100_000u32;
        for i in 0..n {
            // Fibonacci-hash spread (an odd multiplier is a bijection on
            // u32, so fps are distinct — a table unit test needs no real
            // hasher).
            t.insert(i.wrapping_mul(2_654_435_761), i);
        }
        assert!(
            t.entries.len() >= 1 << 18,
            "capacity crossed the doublings: {}",
            t.entries.len()
        );
        let mut seen = std::collections::HashSet::new();
        for i in 0..n {
            let slot = t
                .get_matching(i.wrapping_mul(2_654_435_761), |s| s == i)
                .unwrap_or_else(|| panic!("key {i} lost across growth"));
            assert!(seen.insert(slot), "slot {slot} reachable twice — duplicate entry");
        }
        assert_eq!(t.len, n as usize);
    }

    /// Same-name anti-clustering guard (builder-check 裁定 2, Review
    /// Focus 2): ≥5000 entries sharing ONE name under distinct parents,
    /// built through the REAL `push`/`parent_name_hash` path (not the raw
    /// table), must spread — the fingerprint mixes the parent in, so no
    /// shared home bucket. Every key resolves to its own slot and the
    /// longest probe displacement stays tiny (a name_hash-keyed table
    /// would pour all 5000 into one ~5000-long chain).
    #[test]
    fn same_name_heavy_no_clustering() {
        const N: usize = 5000;
        let mut hot = HotIndex::cold();
        for i in 0..N {
            let mut r = rec("desktop.ini", false);
            r.id = i as FileId + 1;
            r.parent = i as FileId + 1; // every copy under its own parent
            hot.push(r).unwrap();
        }
        for i in 0..N {
            let parent = i as FileId + 1;
            let slot = hot
                .slot_in_dir(parent, "desktop.ini")
                .unwrap_or_else(|| panic!("copy under parent {parent} must hit"));
            assert_eq!(
                slot, i,
                "the copy under parent {parent} resolves to its own slot"
            );
        }
        assert!(
            hot.slot_by_parent_name.max_probe_displacement() < 64,
            "same-name entries must not cluster: max probe displacement {}",
            hot.slot_by_parent_name.max_probe_displacement()
        );
    }

    /// The identity-contract integration (builder-check 裁定 1, Review
    /// Focus 1): a real rename re-points the slot's entry via
    /// (fingerprint, slot) AFTER the columns changed, and watcher
    /// removals — middle slot (tail rebinds in place) and tail slot (no
    /// rebind) — must leave the old key unresolvable, the survivors
    /// resolvable, and exactly one table entry per live slot (no stale
    /// leftovers; the HashMap-era code got that for free).
    #[test]
    fn rename_and_mid_removal_no_stale_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut ids = Vec::new();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
            let mut r = rec(name, false);
            store.upsert_file(&["docs"], &mut r).unwrap();
            ids.push(r.id);
        }
        let mut hot = HotIndex::load(&store).unwrap();
        assert_eq!(hot.slot_by_parent_name.len, hot.slot_count());
        let docs = hot.dir_child(None, "docs").expect("docs interned");

        // Rename through the real watcher path: same row id re-upserted
        // under a new name (SQLite rowid reuse after a delete reaches
        // exactly this update_slot key-change branch). The entry is
        // re-pointed by (fingerprint, slot) while the columns already
        // hold the NEW spelling.
        let mut renamed = rec("cc.txt", false);
        renamed.id = ids[2];
        renamed.parent = docs;
        hot.upsert_slot(&store, &renamed).unwrap();
        assert_eq!(hot.slot_in_dir(docs, "c.txt"), None, "the old spelling is gone");
        let renamed_slot = hot.slot_in_dir(docs, "cc.txt").expect("new spelling resolves");
        assert_eq!(hot.file_id(renamed_slot), ids[2]);
        assert_eq!(hot.slot_by_parent_name.len, hot.slot_count());

        // Middle removal (b.txt, slot 1 of 5): the tail e.txt's entry is
        // rebind-moved to slot 1 in place.
        assert_eq!(hot.remove_by_dir_name(&["docs"], "b.txt"), Some(ids[1]));
        assert_eq!(hot.slot_in_dir(docs, "b.txt"), None);
        for (name, id) in [
            ("a.txt", ids[0]),
            ("cc.txt", ids[2]),
            ("d.txt", ids[3]),
            ("e.txt", ids[4]),
        ] {
            let slot = hot
                .slot_in_dir(docs, name)
                .unwrap_or_else(|| panic!("{name} must resolve"));
            assert_eq!(hot.file_id(slot), id);
        }
        assert_eq!(hot.slot_by_parent_name.len, hot.slot_count());

        // One more middle removal (e.txt now lives in a moved slot), then
        // a tail removal (cc.txt is the last slot — the no-rebind branch).
        assert_eq!(hot.remove_by_dir_name(&["docs"], "e.txt"), Some(ids[4]));
        assert_eq!(hot.remove_by_dir_name(&["docs"], "cc.txt"), Some(ids[2]));
        for (name, id) in [("a.txt", ids[0]), ("d.txt", ids[3])] {
            let slot = hot
                .slot_in_dir(docs, name)
                .unwrap_or_else(|| panic!("{name} must resolve"));
            assert_eq!(hot.file_id(slot), id);
        }
        assert_eq!(hot.slot_count(), 2);
        assert_eq!(
            hot.slot_by_parent_name.len,
            hot.slot_count(),
            "no stale entries: one table entry per live slot"
        );
    }

    // ------------- Task 2: StrRef 6B packing + fallible put ---------------

    /// The packed ref IS the compression: four refs per slot over millions
    /// of slots compound the 2-byte saving per ref. Guarded at compile time
    /// right above the struct too; this keeps the guarantee visible in the
    /// suite.
    #[test]
    fn strref_packed_size() {
        assert_eq!(std::mem::size_of::<StrRef>(), 6);
    }

    /// `put` rejects anything a u16 length cannot name — without writing
    /// arena bytes — and tallies the skip. The boundary value itself still
    /// lands (and round-trips through `str_of`).
    #[test]
    fn strref_rejects_64k_name() {
        let mut hot = HotIndex::cold();

        let arena_before = hot.arena_len();
        assert!(
            hot.put(&"x".repeat(u16::MAX as usize + 1)).is_none(),
            "a name past the u16 length is rejected"
        );
        assert_eq!(
            hot.arena_len(),
            arena_before,
            "a rejected put writes no arena bytes"
        );
        assert_eq!(hot.skipped_oversized, 1, "the rejection is counted");

        let r = hot
            .put(&"x".repeat(u16::MAX as usize))
            .expect("exactly u16::MAX bytes fit a StrRef");
        assert_eq!(hot.str_of(r).len(), u16::MAX as usize);
        assert_eq!(hot.skipped_oversized, 1, "the boundary value is not a skip");
    }

    /// A store row whose name exceeds the guard is skipped at load —
    /// absent from the index, tallied, costing zero interned bytes — while
    /// every other row loads untouched. The same guard covers dirs-table
    /// nodes: the oversized dir node is skipped, and the file under it
    /// still loads (its path reconstructs short — walk_chain stops at the
    /// missing row, by design).
    #[test]
    fn load_skips_oversized_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut alpha = rec("Alpha.TXT", false);
        store.upsert_file(&["docs"], &mut alpha).unwrap();
        let mut huge = rec(&"x".repeat(u16::MAX as usize + 1), false);
        store.upsert_file(&["docs"], &mut huge).unwrap();
        let mut memo = rec("memo.md", false);
        store.upsert_file(&["docs", "inner"], &mut memo).unwrap();
        let huge_dir = "d".repeat(u16::MAX as usize + 1);
        let mut note = rec("note.md", false);
        store.upsert_file(&["docs", huge_dir.as_str()], &mut note).unwrap();

        let hot = HotIndex::load(&store).unwrap();

        assert!(hot.is_warm());
        assert_eq!(
            hot.file_count(),
            3,
            "only the oversized file row is skipped"
        );
        assert_eq!(
            hot.dir_tree_len(),
            2,
            "docs and inner interned; the oversized dir node is not"
        );
        assert_eq!(
            hot.skipped_oversized,
            2,
            "one files row + one dirs row skipped"
        );
        // Survivors are exactly the normally named rows.
        let mut names: Vec<&str> = (0..hot.slot_count())
            .map(|slot| hot.name_original(slot))
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["Alpha.TXT", "memo.md", "note.md"]);
        // And a skip costs zero interned bytes (load_counts_arenas_and_warm's
        // formula, plus note.md's 7+7).
        assert_eq!(hot.interned_bytes(), 4 + 5 + 9 + 9 + 7 + 7 + 7 + 7);
    }
}
