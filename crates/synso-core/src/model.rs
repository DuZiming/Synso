//! Data model: one indexed filesystem entry.

/// Row id in the `files`/`dirs` tables. `0` means "not yet stored".
pub type FileId = i64;

/// One indexed filesystem entry (file or directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// Assigned by `Store::upsert_file` (the `files` row id).
    pub id: FileId,
    /// Containing directory's row id in `dirs`, assigned by `Store::upsert_file`.
    pub parent: FileId,
    /// Final path component only (no separators).
    pub name: String,
    pub size: u64,
    /// Unix-epoch seconds.
    pub mtime: i64,
    /// Unix-epoch seconds.
    pub ctime: i64,
    /// Unix-epoch seconds.
    pub atime: i64,
    /// Interned extension id; 0 = no extension. Assigned by `Store::upsert_file`.
    pub ext_id: i64,
    pub is_dir: bool,
    /// First-block hash (partial), if computed.
    pub hash_first: Option<u64>,
    /// Whole-file hash, if computed.
    pub hash_full: Option<u64>,
    /// Full-pinyin search key ("baogao.docx"): Han chars expand to their
    /// first reading, everything else passes through lowercased. Computed
    /// and assigned by [`crate::store::Store::upsert_file`]; callers
    /// constructing records leave it empty.
    pub py_full: String,
    /// First-letter pinyin key ("bg.docx"), same generation rule as
    /// [`FileRecord::py_full`]. Also store-assigned.
    pub py_init: String,
}
