//! synso-core: embeddable file-index data model and SQLite store.
//!
//! This crate must stay UI-free and dependency-light (never depend on tauri)
//! so the scanner/engine layers built on it remain embeddable and testable.

pub mod collections;
pub mod dedup;
pub mod engine;
pub mod hotindex;
pub mod model;
pub mod pinyin;
pub mod query;
pub mod scanner;
pub mod sizeagg;
pub mod store;
pub mod trash;
pub mod volumes;
pub mod watcher;

pub use collections::{Collection, MemberHit};
pub use dedup::{DedupState, DupeGroup};
pub use engine::{DedupPhase, Engine, EngineEvent, EngineOptions, EngineStats};
pub use hotindex::HotIndex;
pub use model::{FileId, FileRecord, ParsedQuery};
pub use pinyin::pinyin_keys;
pub use query::{parse_query, Hit, SearchResult};
pub use scanner::{is_placeholder_attr, scan, ScanOptions, ScanStats};
pub use sizeagg::SizeNode;
pub use store::Store;
pub use trash::delete_paths_to_trash;
pub use volumes::{fixed_volumes, VolumeInfo};
pub use watcher::Watcher;

/// Error type for all synso-core operations.
#[derive(Debug, thiserror::Error)]
pub enum SynsoError {
    /// Filesystem / OS I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite failure (open, execute, query).
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// Target row does not exist.
    #[error("not found")]
    NotFound,
    /// Requested state conflicts with what already exists (e.g. a
    /// collection name is already taken).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Malformed input from the caller.
    #[error("bad query: {0}")]
    BadQuery(String),
}

/// Result alias used across synso-core.
pub type Result<T> = std::result::Result<T, SynsoError>;
