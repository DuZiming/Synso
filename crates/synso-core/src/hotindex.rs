//! Hot index: the in-memory mirror of the store that search will run
//! against (Task 5). The engine bulk-loads it from SQLite at startup and
//! swaps in a fresh copy after every completed scan — wholesale rebuild is
//! the v1 correctness story; live incremental updates arrive with the
//! filesystem watcher (Task 8).
//!
//! Shape: an interned directory tree keyed by the database's `dirs` row
//! ids (hot-index dir ids and store dir ids are the same numbers), plus
//! one dense slot per `files` row whose columns live in parallel vectors.
//! Names are held twice — lowercased for case-insensitive matching and in
//! the original spelling for display — and [`HotIndex::interned_bytes`]
//! sums the bytes those arenas hold (plus dir names) so the memory budget
//! test (Task 20) has a concrete number to assert against. Pinyin columns
//! (Task 5) will be additional parallel vectors beside the name arenas;
//! they are deliberately not built yet.

use std::collections::HashMap;

use crate::model::{FileId, FileRecord};
use crate::store::Store;
use crate::Result;

/// One interned `dirs` row: parent dir id (`None` for root segments) and
/// the original-case directory name.
#[derive(Debug)]
struct DirEntry {
    parent: Option<FileId>,
    name: String,
}

/// In-memory search index over the store (see module docs).
#[derive(Debug, Default)]
pub struct HotIndex {
    /// Interned directory tree, keyed by the store's `dirs` row ids.
    dirs: HashMap<FileId, DirEntry>,
    /// Name arena, lowercased for case-insensitive matching.
    names_lower: Vec<String>,
    /// Name arena preserving the original spelling (display, rebuilds).
    names_orig: Vec<String>,
    /// Containing directory's `dirs` row id per slot.
    parents: Vec<FileId>,
    /// File size in bytes per slot (0 for directory entries).
    sizes: Vec<u64>,
    /// mtime in Unix-epoch seconds per slot.
    mtimes: Vec<i64>,
    /// Interned extension id per slot (0 = no extension).
    ext_ids: Vec<i64>,
    /// Directory-entry flag per slot.
    is_dirs: Vec<bool>,
    /// Non-directory slot count, cached for O(1) stats.
    file_count: u64,
    /// Directory-entry slot count (`files` rows with `is_dir = 1`, matching
    /// what `ScanStats::dirs` counts — not the `dirs`-table row count).
    dir_count: u64,
    /// Bytes held by the arenas: name lowercase + name original + dir names.
    interned_bytes: u64,
    /// True once an initial full load completed.
    warm: bool,
}

impl HotIndex {
    /// The empty, not-yet-loaded index an engine starts with.
    pub fn cold() -> HotIndex {
        HotIndex::default()
    }

    /// Loads the full `dirs` and `files` tables from `store` into a fresh,
    /// warm index. Whole-table read by design (see module docs).
    pub fn load(store: &Store) -> Result<HotIndex> {
        let mut index = HotIndex {
            warm: true,
            ..HotIndex::default()
        };
        for (id, parent, name) in store.all_dirs()? {
            index.interned_bytes += name.len() as u64;
            index.dirs.insert(id, DirEntry { parent, name });
        }
        for rec in store.all_files()? {
            index.push(rec);
        }
        Ok(index)
    }

    /// Appends one `files` row's columns to the arenas (load-time only
    /// today; incremental pushes arrive with the watcher in Task 8).
    fn push(&mut self, rec: FileRecord) {
        let lower = rec.name.to_lowercase();
        self.interned_bytes += (lower.len() + rec.name.len()) as u64;
        if rec.is_dir {
            self.dir_count += 1;
        } else {
            self.file_count += 1;
        }
        self.names_lower.push(lower);
        self.names_orig.push(rec.name);
        self.parents.push(rec.parent);
        self.sizes.push(rec.size);
        self.mtimes.push(rec.mtime);
        self.ext_ids.push(rec.ext_id);
        self.is_dirs.push(rec.is_dir);
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
    /// results (Task 5) is the real consumer; tests use it to pin the
    /// parent linkage.
    pub fn dir_row(&self, id: FileId) -> Option<(Option<FileId>, &str)> {
        self.dirs
            .get(&id)
            .map(|entry| (entry.parent, entry.name.as_str()))
    }

    /// True once the initial full load completed.
    pub fn is_warm(&self) -> bool {
        self.warm
    }

    /// Total bytes held by the arenas: name lowercase + name original +
    /// dir names.
    pub fn interned_bytes(&self) -> u64 {
        self.interned_bytes
    }

    /// Lowercased name of `slot` (case-insensitive matching).
    pub fn name_lower(&self, slot: usize) -> &str {
        &self.names_lower[slot]
    }

    /// Original-case name of `slot` (display, rebuilds).
    pub fn name_original(&self, slot: usize) -> &str {
        &self.names_orig[slot]
    }

    /// Containing directory's `dirs` row id of `slot`.
    pub fn parent_dir(&self, slot: usize) -> FileId {
        self.parents[slot]
    }

    /// Size in bytes of `slot` (0 for directory entries).
    pub fn size(&self, slot: usize) -> u64 {
        self.sizes[slot]
    }

    /// mtime (Unix-epoch seconds) of `slot`.
    pub fn mtime(&self, slot: usize) -> i64 {
        self.mtimes[slot]
    }

    /// Interned extension id of `slot` (0 = no extension).
    pub fn ext_id(&self, slot: usize) -> i64 {
        self.ext_ids[slot]
    }

    /// Whether `slot` is a directory entry.
    pub fn is_dir_entry(&self, slot: usize) -> bool {
        self.is_dirs[slot]
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
            // dir names + per-entry lowercase/original name bytes
            4 + 5 + 9 + 9 + 3 + 3 + 7 + 7,
            "interned bytes = dir names + both name arenas"
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
}
