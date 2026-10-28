//! synso-core: the Synso file-indexing engine as one embeddable facade —
//! SQLite store, scanner, in-memory hot index, query execution, watcher,
//! dedup, collections, trash and directory-size aggregation, glued behind
//! a single entry point.
//!
//! The crate stays UI-free and dependency-light (never depend on tauri) so
//! the app layer and future embedders (P2P layer, MCP server) program
//! against ONE clean surface. Implementation modules are `pub(crate)`;
//! everything an embedder needs is re-exported here:
//!
//! - Engine plumbing: [`Engine`], [`EngineOptions`], [`EngineEvent`],
//!   [`EngineStats`]
//! - Settings: [`Settings`] (persisted in the store's meta table via
//!   [`Engine::settings`] / [`Engine::set_settings`]), [`list_volumes`]
//! - Query data: [`parse_query`], [`ParsedQuery`], [`SearchResult`], [`Hit`]
//! - Feature data: [`SizeNode`], [`DedupPhase`], [`DedupState`],
//!   [`DupeGroup`], [`Collection`], [`MemberHit`], [`FileId`],
//!   [`VolumeInfo`], [`pinyin_keys`]
//! - Errors: [`SynsoError`], [`Result`]
//!
//! With the optional `serde` feature enabled, the facade DATA types
//! (`Hit`, `SearchResult`, `ParsedQuery`, `SizeNode`, `DedupPhase`,
//! `DedupState`, `DupeGroup`, `Collection`, `MemberHit`, `EngineStats`,
//! `EngineEvent`, `VolumeInfo`, `Settings`) derive `Serialize` /
//! `Deserialize` — the app layer and JSON bridges turn that on; core
//! itself stays dependency-lean without it.
//!
//! # Example
//!
//! Open an engine over a scratch directory, scan it, search it:
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use synso_core::{Engine, EngineEvent, EngineOptions};
//!
//! let tmp = tempfile::tempdir()?;
//! let engine = Engine::open(&tmp.path().join("index.db"), EngineOptions::default())?;
//!
//! // Subscribe BEFORE scanning so no event is missed.
//! let events = engine.subscribe();
//! engine.start_full_scan();
//! loop {
//!     match events
//!         .recv_timeout(Duration::from_secs(60))
//!         .expect("event channel alive")
//!     {
//!         EngineEvent::ScanFinished { files, .. } => {
//!             println!("indexed {files} files");
//!             break;
//!         }
//!         EngineEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
//!         _ => {}
//!     }
//! }
//!
//! // Case-insensitive substring search over names and pinyin keys.
//! let result = engine.search("report", 0, 50);
//! for hit in &result.hits {
//!     println!("{} ({} bytes)", hit.path, hit.size);
//! }
//!
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub(crate) mod collections;
pub(crate) mod dedup;
pub(crate) mod engine;
pub(crate) mod hotindex;
pub(crate) mod model;
pub(crate) mod pinyin;
pub(crate) mod query;
pub(crate) mod scanner;
pub(crate) mod settings;
pub(crate) mod sizeagg;
pub(crate) mod store;
pub(crate) mod trash;
pub(crate) mod volumes;
pub(crate) mod watcher;

// The public facade: the ONLY surface embedders program against.
pub use collections::{Collection, MemberHit};
pub use dedup::{DedupState, DupeGroup};
pub use engine::{DedupPhase, Engine, EngineEvent, EngineOptions, EngineStats};
pub use model::{FileId, ParsedQuery};
pub use pinyin::pinyin_keys;
pub use query::{parse_query, Hit, SearchResult};
pub use settings::{load_persisted, Settings};
pub use sizeagg::SizeNode;
/// The fixed local volumes, under the name the first-run wizard consumes
/// (`Task 13` facade ruling): a thin re-export of
/// [`crate::volumes::fixed_volumes`].
pub use volumes::fixed_volumes as list_volumes;
pub use volumes::VolumeInfo;

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
    /// Malformed input from the caller (e.g. a path-segment resolution
    /// error in the store).
    #[error("bad query: {0}")]
    BadQuery(String),
    /// A glob/exclusion pattern failed to compile.
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
}

/// Result alias used across synso-core.
pub type Result<T> = std::result::Result<T, SynsoError>;
