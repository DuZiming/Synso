//! Cold-tier dimension access (v1.2): the read path for per-file data
//! that is NOT part of the search scan — ctime/atime and the dedup
//! hashes today, summaries and any future per-file dimension tomorrow.
//! Values flow through the engine's [`crate::tiers::TierCache`] (hot →
//! staging → this cold read) and are cached per the memory policy.
//!
//! The cold read runs on a DEDICATED read-only SQLite connection in WAL
//! mode, deliberately outside the main write connection's mutex: a full
//! rescan holds that mutex for its whole walk, and queuing every
//! dimension touch behind it would recreate the freeze class v1.1
//! fought. WAL adds persistent `-wal`/`-shm` siblings next to the index
//! database — expected.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::model::FileId;
use crate::Result;

/// The pilot dimension's payload: everything a "file detail" surface
/// shows. Future dimensions register their own row types.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileDetail {
    pub id: FileId,
    pub ctime: Option<i64>,
    pub atime: Option<i64>,
    /// First-block hash (None until a dedup run covered this file).
    pub hash_first: Option<u64>,
    /// Full-content hash (None likewise).
    pub hash_full: Option<u64>,
}

impl FileDetail {
    /// Serde-friendly byte cost for the tier cache's budget accounting:
    /// the Option<u64> trio is 3×9 bytes + id 8 + slack — call it 64.
    pub fn cached_bytes(&self) -> u64 {
        64
    }
}

/// A read-only view over the index database for dimension reads.
pub(crate) struct ColdReader {
    conn: Connection,
}

impl ColdReader {
    /// Opens the database read-only with WAL. `journal_mode=WAL` must be
    /// set ONCE by the writer (Store::open does it); a reader then sees
    /// committed data without locking the writer out.
    pub(crate) fn open(db_path: &Path) -> Result<ColdReader> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        Ok(ColdReader { conn })
    }

    /// Fetches the detail rows for `ids` (order not guaranteed; missing
    /// ids are simply absent from the result).
    pub(crate) fn file_details(&self, ids: &[FileId]) -> Result<Vec<FileDetail>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Chunked IN lists stay under SQLite's parameter cap.
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, ctime, atime, hash_first, hash_full FROM files \
                 WHERE id IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok(FileDetail {
                    id: row.get(0)?,
                    ctime: row.get(1)?,
                    atime: row.get(2)?,
                    hash_first: row.get(3)?,
                    hash_full: row.get(4)?,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// The persisted heat snapshot, hottest first, capped at `limit`:
    /// `(file_id, score, at)` with `score` the raw ×256 fixed-point
    /// value (`raw / 256 >= hot_threshold` is the gate on both ends) and
    /// `at` the unix second the snapshot was taken. Read source for the
    /// startup preheat: the engine replays these scores into the tier
    /// cache so yesterday's hot files do not re-earn their heat from
    /// zero. No production caller yet — the engine's startup preheat
    /// wires it up in the next heat-persistence step.
    #[allow(dead_code)]
    pub(crate) fn read_heat(&self, limit: usize) -> Result<Vec<(FileId, u32, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_id, score, at FROM heat ORDER BY score DESC LIMIT ?1")?;
        // score round-trips the same bit-preserving cast replace_heat
        // used to store it (u32 → i64 → u32; scores are never negative).
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((
                row.get::<_, FileId>(0)?,
                row.get::<_, i64>(1)? as u32,
                row.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
