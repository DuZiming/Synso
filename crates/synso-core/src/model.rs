//! Data model: one indexed filesystem entry, plus the parsed query shape
//! the search path filters on.

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

/// Parsed form of one search query string: the free-text words plus the
/// Everything-style dimension filters [`crate::query::parse_query`]
/// recognized along the way. Pure data — parsing does no I/O; execution
/// (filtering) happens in [`crate::engine::Engine::search_parsed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ParsedQuery {
    /// The free-text tokens that were NOT modifiers, joined by single
    /// spaces, original spelling. Multiple words must ALL match (AND),
    /// each an independent case-insensitive substring hit; empty text
    /// lists the most recent entries instead.
    pub text: String,
    /// `ext:pdf` — exact extension match. Lowercased, no leading dot (the
    /// spelling the store interns). Single value in v1; a repeated token
    /// wins with its last occurrence.
    pub ext: Option<String>,
    /// Inclusive lower size bound in bytes (`size:>1mb` stores 1 MiB + 1,
    /// since `>` is strict). Repeated size tokens combine into the
    /// tightest bounds.
    pub min_size: Option<u64>,
    /// Inclusive upper size bound in bytes (`size:<1mb` stores 1 MiB - 1).
    pub max_size: Option<u64>,
    /// `path:proj` — case-insensitive SUBSTRING match on the full
    /// reconstructed path (the field name keeps the brief's spelling;
    /// the semantics are substring, not prefix). Last occurrence wins.
    pub path_prefix: Option<String>,
}
