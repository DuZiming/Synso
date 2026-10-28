//! Virtual collections: a user-curated organization layer stacked on the
//! file index (design: "文件永远留在源路径，一个文件可属于任意多个集合").
//!
//! Members are `files` row ids — file AND folder rows both qualify (the
//! scanner records walked directories as `files` rows with `is_dir = 1`).
//! Membership rows carry a snapshot (`snap_name`, `snap_size`) frozen at
//! [`add`] time; it is never refreshed while the member is alive, because
//! it is the rebind key:
//!
//! - **Tombstone:** when a member's `file_id` no longer exists in `files`
//!   (the file was moved/deleted between scans), [`reconcile`] sets
//!   `alive = 0` and keeps the snapshot. Because a rescan purges each
//!   root's subtree and SQLite recycles rowids, a stale id can also be
//!   re-occupied by a DIFFERENT row — so liveness additionally requires
//!   the row at that id to still carry the snapshot's name (a size drift
//!   on the same name is the same file grown in place and stays alive).
//! - **Rebind:** when a `files` row with the same name AND size reappears
//!   (the classic "moved elsewhere" shape), [`reconcile`] repoints the
//!   member at the new row id and sets `alive = 1`. Multiple candidates
//!   resolve to the most recent mtime (tie: highest file id); no candidate
//!   leaves the member dead. Since rescans purge and re-insert rows (ids
//!   churn), every reconcile re-derives membership identity from the
//!   snapshots this way — the snapshot, not the id, is the member's
//!   durable identity.
//!
//! [`reconcile`] is a standalone store pass the engine runs at the end of
//! every successful scan cycle (after the hot-index reload); the Task 8
//! watcher will call it too, so it must stay callable on its own.

use std::collections::HashSet;
use std::path::{Path, MAIN_SEPARATOR};

use rusqlite::{params, Connection, OptionalExtension};

use crate::model::FileId;
use crate::query::Hit;
use crate::store::Store;
use crate::{Result, SynsoError};

/// One virtual collection row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    pub name: String,
}

/// One collection member row, materialized for display. `alive == false`
/// members carry a [`Hit`] synthesized from the snapshot: `name`/`size` are
/// the snapshot values, `path` is empty (unknown — the file is gone),
/// `mtime` is 0, `is_dir` is false, `ext` is derived from `snap_name`, and
/// `id` is the STALE file id (kept so the UI can address the member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberHit {
    pub hit: Hit,
    pub alive: bool,
}

/// Creates a collection. A duplicate name is `Err(SynsoError::Conflict)`.
pub fn create(store: &mut Store, name: &str) -> Result<Collection> {
    let conn = store.conn_mut();
    // Pre-check so a duplicate surfaces as Conflict, not a raw UNIQUE
    // constraint Db error (the collections table has no INSERT OR IGNORE
    // shape that could report the clash cleanly).
    let taken: Option<i64> = conn
        .query_row(
            "SELECT id FROM collections WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    if taken.is_some() {
        return Err(SynsoError::Conflict(format!(
            "collection name {name:?} already exists"
        )));
    }
    conn.execute("INSERT INTO collections(name) VALUES (?1)", params![name])?;
    Ok(Collection {
        id: conn.last_insert_rowid(),
        name: name.to_string(),
    })
}

/// Renames a collection. `Err(NotFound)` when `id` is no collection;
/// `Err(Conflict)` when `name` is already taken by another collection.
pub fn rename(store: &mut Store, id: i64, name: &str) -> Result<()> {
    let conn = store.conn_mut();
    let clash: Option<i64> = conn
        .query_row(
            "SELECT id FROM collections WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    if clash.is_some_and(|other| other != id) {
        return Err(SynsoError::Conflict(format!(
            "collection name {name:?} already exists"
        )));
    }
    let changed = conn.execute(
        "UPDATE collections SET name = ?1 WHERE id = ?2",
        params![name, id],
    )?;
    if changed == 0 {
        return Err(SynsoError::NotFound);
    }
    Ok(())
}

/// Deletes a collection AND its member rows (explicit `DELETE`s — the
/// schema has no FK cascade). `Err(NotFound)` when `id` is no collection.
pub fn delete(store: &mut Store, id: i64) -> Result<()> {
    let conn = store.conn_mut();
    conn.execute(
        "DELETE FROM collection_members WHERE coll_id = ?1",
        params![id],
    )?;
    let changed = conn.execute("DELETE FROM collections WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(SynsoError::NotFound);
    }
    Ok(())
}

/// Every collection, id-ordered (creation order).
pub fn list(store: &Store) -> Result<Vec<Collection>> {
    let mut stmt = store
        .conn()
        .prepare("SELECT id, name FROM collections ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(Collection {
            id: row.get(0)?,
            name: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Adds `members` (file OR folder row ids) to collection `id`, snapshotting
/// each file's CURRENT name and size. `Err(NotFound)` when the collection
/// or any file row does not exist. Re-adding a member that is already
/// alive is a no-op (the snapshot is never refreshed while alive);
/// re-adding a dead member resurrects it with a fresh snapshot.
pub fn add(store: &mut Store, id: i64, members: &[FileId]) -> Result<()> {
    let conn = store.conn_mut();
    let exists: Option<i64> = conn
        .query_row(
            "SELECT id FROM collections WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(SynsoError::NotFound);
    }
    // One transaction so a stale id mid-list cannot leave a partial add.
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO collection_members(coll_id, file_id, snap_name, snap_size, alive)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(coll_id, file_id) DO UPDATE SET
               snap_name = excluded.snap_name,
               snap_size = excluded.snap_size,
               alive = 1
             WHERE collection_members.alive = 0",
        )?;
        for member in members {
            let snapshot: Option<(String, i64)> = tx
                .query_row(
                    "SELECT name, size FROM files WHERE id = ?1",
                    params![member],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((snap_name, snap_size)) = snapshot else {
                // Member row ids come from search hits, i.e. live files
                // rows; a row that vanished since is the caller's stale id.
                return Err(SynsoError::NotFound);
            };
            // The DO UPDATE ... WHERE alive = 0 arm keeps an already-alive
            // member's snapshot frozen (it is the rebind key) and only
            // lets an EXPLICIT re-add resurrect a dead member with a
            // fresh snapshot.
            insert.execute(params![id, member, snap_name, snap_size])?;
        }
    }
    Ok(tx.commit()?)
}

/// Removes one member row (alive or tombstoned). `Err(NotFound)` when
/// neither the collection nor that membership exists.
pub fn remove(store: &mut Store, id: i64, member: FileId) -> Result<()> {
    let changed = store.conn_mut().execute(
        "DELETE FROM collection_members WHERE coll_id = ?1 AND file_id = ?2",
        params![id, member],
    )?;
    if changed == 0 {
        return Err(SynsoError::NotFound);
    }
    Ok(())
}

/// The collection's members ordered by `(snap_name, snap_size, file_id)`
/// (deterministic across rescans). A nonexistent collection lists empty.
pub fn members(store: &Store, id: i64) -> Result<Vec<MemberHit>> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT m.file_id, m.snap_name, m.snap_size, m.alive,
                f.name, f.size, f.mtime, f.ext_id, f.is_dir, f.dir
         FROM collection_members AS m
         LEFT JOIN files AS f ON f.id = m.file_id
         WHERE m.coll_id = ?1
         ORDER BY m.snap_name, m.snap_size, m.file_id",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok((
            row.get::<_, FileId>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)? != 0,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?.is_some_and(|v| v != 0),
            row.get::<_, Option<FileId>>(9)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (file_id, snap_name, snap_size, alive, name, size, mtime, ext_id, is_dir, dir) = row?;
        // Alive members with a live row show the row as it is NOW; the
        // snapshot only takes over once the row is gone (dead members, and
        // the tiny window where an alive flag has not been reconciled yet).
        let hit = match (alive, name, size, dir) {
            (true, Some(name), Some(size), Some(dir)) => Hit {
                id: file_id,
                path: file_path(conn, dir, &name)?,
                name,
                size: size as u64,
                mtime: mtime.unwrap_or(0),
                ext: ext_text(conn, ext_id)?,
                is_dir,
            },
            _ => Hit {
                id: file_id,
                path: String::new(),
                mtime: 0,
                ext: snapshot_ext(&snap_name),
                is_dir: false,
                name: snap_name,
                size: snap_size as u64,
            },
        };
        hits.push(MemberHit { hit, alive });
    }
    Ok(hits)
}

/// The `files` row ids that are ALIVE members of EVERY named collection —
/// the execution set of the `coll:` search filter
/// ([`crate::engine::Engine::search_parsed`]). `Ok(None)` when any name
/// matches no collection: an unknown name filters the query to no results
/// rather than erroring. `names` must be non-empty (an empty list means
/// "no collection filter"; callers skip this call entirely).
pub fn alive_member_intersection(
    store: &Store,
    names: &[String],
) -> Result<Option<HashSet<FileId>>> {
    let conn = store.conn();
    let mut intersection: Option<HashSet<FileId>> = None;
    for name in names {
        let coll_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM collections WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(coll_id) = coll_id else {
            return Ok(None);
        };
        let mut members = conn.prepare(
            "SELECT file_id FROM collection_members
             WHERE coll_id = ?1 AND alive = 1",
        )?;
        let alive: HashSet<FileId> = members
            .query_map(params![coll_id], |row| row.get(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        intersection = Some(match intersection {
            None => alive,
            Some(prev) => prev.intersection(&alive).copied().collect(),
        });
    }
    Ok(intersection)
}

/// Tombstone + rebind pass (see module docs). Runs in one transaction.
pub fn reconcile(store: &mut Store) -> Result<()> {
    let conn = store.conn_mut();
    let tx = conn.transaction()?;

    // 1. Tombstone: every alive member whose file row is gone keeps its
    //    snapshot and flips to dead, in one set-based statement. The name
    //    check is load-bearing: rescan purges the root subtree and SQLite
    //    recycles rowids, so a vanished member's id may be re-occupied by
    //    an unrelated row — only a row still carrying the snapshot's name
    //    counts as "the member's file" (size may drift: same file, grown).
    tx.execute(
        "UPDATE collection_members AS m SET alive = 0
         WHERE m.alive = 1 AND NOT EXISTS (
             SELECT 1 FROM files AS f
             WHERE f.id = m.file_id AND f.name = m.snap_name)",
        [],
    )?;

    // 2. Rebind: dead members whose (name, size) snapshot reappeared point
    //    at the new row. Collected first so the writes below never mutate
    //    a cursor we are iterating; PK order keeps the pass deterministic.
    let dead: Vec<(i64, FileId, String, i64)> = {
        let mut select = tx.prepare(
            "SELECT coll_id, file_id, snap_name, snap_size FROM collection_members
             WHERE alive = 0 ORDER BY coll_id, file_id",
        )?;
        let rows = select.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for (coll, old_id, snap_name, snap_size) in dead {
        // Earlier iterations of this pass may have deleted this row (as a
        // duplicate) or re-purposed its id slot; skip both, never re-die.
        match member_state(&tx, coll, old_id)? {
            MemberState::Dead => {}
            _ => continue,
        }
        let Some(winner) = rebind_target(&tx, &snap_name, snap_size)? else {
            continue; // no candidate: stay dead, snapshot kept
        };
        if winner == old_id {
            // SQLite recycled the dead member's own rowid into the
            // reappeared file; the row already points at it.
            tx.execute(
                "UPDATE collection_members SET alive = 1 WHERE coll_id = ?1 AND file_id = ?2",
                params![coll, old_id],
            )?;
            continue;
        }
        match member_state(&tx, coll, winner)? {
            // Free slot: plain rebind.
            MemberState::Absent => {
                tx.execute(
                    "UPDATE collection_members SET file_id = ?1, alive = 1
                     WHERE coll_id = ?2 AND file_id = ?3",
                    params![winner, coll, old_id],
                )?;
            }
            // The collection already tracks the winner alive: this dead
            // member is a duplicate of it — drop it instead of colliding
            // on the (coll_id, file_id) primary key (and never overwrite
            // the live member's frozen snapshot).
            MemberState::Alive => {
                tx.execute(
                    "DELETE FROM collection_members WHERE coll_id = ?1 AND file_id = ?2",
                    params![coll, old_id],
                )?;
            }
            // A dead row squats on the winner's id (its stale file_id
            // number got recycled). Its own snapshot no longer matches the
            // file under that id, while this member's CERTAIN match is
            // worth more than its speculative future one: clear the slot,
            // then rebind.
            MemberState::Dead => {
                tx.execute(
                    "DELETE FROM collection_members WHERE coll_id = ?1 AND file_id = ?2",
                    params![coll, winner],
                )?;
                tx.execute(
                    "UPDATE collection_members SET file_id = ?1, alive = 1
                     WHERE coll_id = ?2 AND file_id = ?3",
                    params![winner, coll, old_id],
                )?;
            }
        }
    }
    Ok(tx.commit()?)
}

/// What sits at a `(coll_id, file_id)` membership slot right now.
enum MemberState {
    Absent,
    Alive,
    Dead,
}

fn member_state(conn: &Connection, coll: i64, file_id: FileId) -> Result<MemberState> {
    let alive: Option<i64> = conn
        .query_row(
            "SELECT alive FROM collection_members WHERE coll_id = ?1 AND file_id = ?2",
            params![coll, file_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match alive {
        None => MemberState::Absent,
        Some(v) if v != 0 => MemberState::Alive,
        Some(_) => MemberState::Dead,
    })
}

/// The rebind target for a snapshot: the `files` row with the same name
/// AND size — most recent mtime wins, full ties break to the highest file
/// id so the pick is deterministic. `None` when no candidate exists.
fn rebind_target(conn: &Connection, snap_name: &str, snap_size: i64) -> Result<Option<FileId>> {
    let mut stmt =
        conn.prepare("SELECT id, COALESCE(mtime, 0) FROM files WHERE name = ?1 AND size = ?2")?;
    let rows = stmt.query_map(params![snap_name, snap_size], |row| {
        Ok((row.get::<_, FileId>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut best: Option<(i64, FileId)> = None;
    for row in rows {
        let (id, mtime) = row?;
        if best.is_none_or(|(best_mtime, best_id)| (mtime, id) > (best_mtime, best_id)) {
            best = Some((mtime, id));
        }
    }
    Ok(best.map(|(_, id)| id))
}

/// Full index-relative path of the `files` row `(dir, name)`, rebuilt by
/// walking the `dirs` tree up from `dir` — the same shape
/// [`crate::hotindex::HotIndex::path_of`] produces for search hits.
fn file_path(conn: &Connection, dir: FileId, name: &str) -> Result<String> {
    // Walk root-first: collect parent names up (bounded by the dir-table
    // size — the tree cannot be deeper than its node count, which also
    // terminates a corrupt parent cycle), then reverse.
    let dir_count: i64 = conn.query_row("SELECT COUNT(*) FROM dirs", [], |row| row.get(0))?;
    let mut budget = dir_count + 1;
    let mut segments = vec![name.to_string()];
    let mut current = Some(dir);
    while let Some(id) = current {
        budget -= 1;
        if budget <= 0 {
            break;
        }
        let row: Option<(Option<FileId>, String)> = conn
            .query_row(
                "SELECT parent, name FROM dirs WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((parent, segment)) = row else {
            break; // missing dir row (corrupt): keep the chain we have
        };
        segments.push(segment);
        current = parent;
    }
    segments.reverse();
    let bytes = segments.iter().map(String::len).sum::<usize>() + segments.len().saturating_sub(1);
    let mut path = String::with_capacity(bytes);
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            path.push(MAIN_SEPARATOR);
        }
        path.push_str(segment);
    }
    Ok(path)
}

/// Extension text for an interned ext id (0 and unknown ids: empty).
fn ext_text(conn: &Connection, ext_id: Option<i64>) -> Result<String> {
    let Some(ext_id) = ext_id else {
        return Ok(String::new());
    };
    let ext: Option<String> = conn
        .query_row(
            "SELECT ext FROM extensions WHERE id = ?1",
            params![ext_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(ext.unwrap_or_default())
}

/// Extension derived from a snapshot name — std dotfile semantics and
/// lowercasing, mirroring the store's `intern_extension`.
fn snapshot_ext(snap_name: &str) -> String {
    Path::new(snap_name)
        .extension()
        .map_or(String::new(), |ext| {
            ext.to_string_lossy().to_ascii_lowercase()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineEvent, EngineOptions};
    use crate::model::FileRecord;
    use crate::store::Store;
    use crate::SynsoError;
    use crossbeam_channel::Receiver;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    // ------------------------- store-level helpers -------------------------

    /// Minimal record builder; tests override fields they care about.
    fn rec(name: &str, size: u64) -> FileRecord {
        FileRecord {
            id: 0,
            parent: 0,
            name: name.to_string(),
            size,
            mtime: 1_700_000_000,
            ctime: 1_700_000_001,
            atime: 1_700_000_002,
            ext_id: 0,
            is_dir: false,
            hash_first: None,
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        }
    }

    fn upsert_at(store: &mut Store, dir: &str, name: &str, size: u64, mtime: i64) -> FileId {
        let mut record = rec(name, size);
        record.mtime = mtime;
        store.upsert_file(&[dir], &mut record).unwrap();
        record.id
    }

    fn upsert(store: &mut Store, dir: &str, name: &str, size: u64) -> FileId {
        upsert_at(store, dir, name, size, 1_700_000_000)
    }

    fn member_ids(store: &Store, coll: i64) -> Vec<FileId> {
        members(store, coll)
            .unwrap()
            .into_iter()
            .map(|m| m.hit.id)
            .collect()
    }

    // ------------------------- engine-level helpers ------------------------

    fn open_engine(db: &Path, root: &Path) -> (Engine, Receiver<EngineEvent>) {
        let engine = Engine::open(
            db,
            EngineOptions {
                volumes: vec![root.to_string_lossy().into_owned()],
                exclusions: Vec::new(),
            },
        )
        .unwrap();
        let rx = engine.subscribe();
        (engine, rx)
    }

    /// Runs one full scan and waits for its `ScanFinished`.
    fn scan_once(engine: &Engine, rx: &Receiver<EngineEvent>) -> (u64, u64) {
        engine.start_full_scan();
        loop {
            match rx
                .recv_timeout(Duration::from_secs(60))
                .expect("an event arrives before the timeout")
            {
                EngineEvent::ScanProgress { .. } => continue,
                EngineEvent::ScanFinished { files, dirs } => return (files, dirs),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    /// The unique search hit whose name equals `name`.
    fn unique_hit(engine: &Engine, name: &str) -> crate::query::Hit {
        let result = engine.search(name, 0, 50);
        assert_eq!(result.total, 1, "exactly one hit named {name}");
        result.hits[0].clone()
    }

    /// Among the hits named `name`, the one living under the direct child
    /// directory `dir` of the scan root (segment-aware, cross-platform).
    fn hit_under(engine: &Engine, name: &str, dir: &str) -> crate::query::Hit {
        let result = engine.search(name, 0, 50);
        result
            .hits
            .iter()
            .find(|h| h.name == name && h.path.split(MAIN_SEPARATOR).any(|segment| segment == dir))
            .cloned()
            .unwrap_or_else(|| panic!("no {name} hit under {dir}: {result:?}"))
    }

    /// Sets a file's mtime explicitly (deterministic rebind ordering).
    fn set_mtime(path: &Path, when: SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    /// Fixture root with one subdirectory per `dirs` entry.
    fn fixture_with_dirs(base: &Path, dirs: &[&str]) -> PathBuf {
        let root = base.join("fixture");
        for dir in dirs {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        root
    }

    // ------------------------ engine-level tests ---------------------------

    #[test]
    fn collection_multi_membership() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = fixture_with_dirs(tmp.path(), &[""]);
        fs::write(root.join("readme.md"), b"hello").unwrap();

        let (engine, rx) = open_engine(&db_path, &root);
        let (files, _dirs) = scan_once(&engine, &rx);
        assert_eq!(files, 1);

        // One file, two collections.
        let hit = unique_hit(&engine, "readme.md");
        let work = engine.coll_create("工作").unwrap();
        let backup = engine.coll_create("backup").unwrap();
        assert_ne!(work.id, backup.id);
        engine.coll_add(work.id, &[hit.id]).unwrap();
        engine.coll_add(backup.id, &[hit.id]).unwrap();

        for coll in [&work, &backup] {
            let members = engine.coll_members(coll.id).unwrap();
            assert_eq!(members.len(), 1, "{} holds exactly the one file", coll.name);
            let member = &members[0];
            assert!(member.alive);
            assert_eq!(member.hit.id, hit.id);
            assert_eq!(member.hit.name, "readme.md");
            assert_eq!(member.hit.size, 5);
            assert!(member.hit.path.contains("readme.md"));
        }

        let names: Vec<String> = engine
            .coll_list()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(names.contains(&"工作".to_string()));
        assert!(names.contains(&"backup".to_string()));

        // Removing from one collection leaves the other untouched.
        engine.coll_remove(work.id, hit.id).unwrap();
        assert!(engine.coll_members(work.id).unwrap().is_empty());
        assert_eq!(engine.coll_members(backup.id).unwrap().len(), 1);
    }

    #[test]
    fn collection_folder_member() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = fixture_with_dirs(tmp.path(), &["docs"]);
        fs::write(root.join("docs").join("notes.txt"), b"0123456789").unwrap();

        let (engine, rx) = open_engine(&db_path, &root);
        let (files, dirs) = scan_once(&engine, &rx);
        assert_eq!((files, dirs), (1, 1), "notes.txt plus the docs entry");

        // Folders are files-table rows too and can join a collection.
        let folder = unique_hit(&engine, "docs");
        assert!(folder.is_dir);
        let coll = engine.coll_create("folders").unwrap();
        engine.coll_add(coll.id, &[folder.id]).unwrap();

        let members = engine.coll_members(coll.id).unwrap();
        assert_eq!(members.len(), 1);
        let member = &members[0];
        assert!(member.alive);
        assert!(member.hit.is_dir, "the member IS the folder row");
        assert_eq!(member.hit.name, "docs");
        assert!(member.hit.path.contains("docs"));
    }

    #[test]
    fn tombstone_then_rebind() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = fixture_with_dirs(tmp.path(), &["a", "b"]);
        fs::write(root.join("a").join("data.bin"), b"0123456789").unwrap();

        let (engine, rx) = open_engine(&db_path, &root);
        scan_once(&engine, &rx);

        let original = unique_hit(&engine, "data.bin");
        let coll = engine.coll_create("keep").unwrap();
        engine.coll_add(coll.id, &[original.id]).unwrap();
        assert!(engine.coll_members(coll.id).unwrap()[0].alive);

        // Disk delete + rescan: the member is tombstoned, not dropped.
        fs::remove_file(root.join("a").join("data.bin")).unwrap();
        scan_once(&engine, &rx);
        let members = engine.coll_members(coll.id).unwrap();
        assert_eq!(members.len(), 1, "tombstoned members stay listed");
        let dead = &members[0];
        assert!(!dead.alive);
        assert_eq!(dead.hit.id, original.id, "the stale file id is kept");
        assert_eq!(dead.hit.name, "data.bin");
        assert_eq!(dead.hit.size, 10);
        assert_eq!(dead.hit.path, "", "the snapshot knows no path");
        assert_eq!(dead.hit.mtime, 0);
        assert!(!dead.hit.is_dir);
        assert_eq!(dead.hit.ext, "bin");

        // Same name + same size elsewhere + rescan: rebind to the new id.
        fs::write(root.join("b").join("data.bin"), b"0123456789").unwrap();
        scan_once(&engine, &rx);
        let reborn = unique_hit(&engine, "data.bin");
        assert_ne!(reborn.id, original.id);
        let members = engine.coll_members(coll.id).unwrap();
        assert_eq!(members.len(), 1);
        let alive = &members[0];
        assert!(alive.alive, "rebound to the reappeared file");
        assert_eq!(alive.hit.id, reborn.id, "points at the NEW file id");
        assert!(
            alive.hit.path.contains("b"),
            "path moved: {}",
            alive.hit.path
        );
        assert_eq!(alive.hit.size, 10);
    }

    #[test]
    fn coll_delete_cascades_members() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = fixture_with_dirs(tmp.path(), &["docs"]);
        fs::write(root.join("readme.md"), b"hello").unwrap();
        fs::write(root.join("docs").join("notes.txt"), b"0123456789").unwrap();

        let (engine, rx) = open_engine(&db_path, &root);
        scan_once(&engine, &rx);
        let readme = unique_hit(&engine, "readme.md");
        let notes = unique_hit(&engine, "notes.txt");

        let coll = engine.coll_create("doomed").unwrap();
        engine.coll_add(coll.id, &[readme.id, notes.id]).unwrap();
        assert_eq!(engine.coll_members(coll.id).unwrap().len(), 2);

        engine.coll_delete(coll.id).unwrap();
        assert!(engine.coll_list().unwrap().is_empty());
        assert!(engine.coll_members(coll.id).unwrap().is_empty());

        // Both tables are truly empty (explicit DELETE, not an FK pragma).
        let store = Store::open(&db_path).unwrap();
        for table in ["collections", "collection_members"] {
            let count: i64 = store
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} must be empty after coll_delete");
        }
        assert!(matches!(
            engine.coll_delete(coll.id),
            Err(SynsoError::NotFound)
        ));
    }

    #[test]
    fn rebind_ambiguous_picks_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = fixture_with_dirs(tmp.path(), &["a", "b", "c"]);
        for dir in ["a", "b", "c"] {
            fs::write(root.join(dir).join("dup.bin"), b"01234567").unwrap();
        }

        let (engine, rx) = open_engine(&db_path, &root);
        scan_once(&engine, &rx);
        let tracked = hit_under(&engine, "dup.bin", "a");
        let coll = engine.coll_create("amb").unwrap();
        engine.coll_add(coll.id, &[tracked.id]).unwrap();

        // The tracked copy vanishes; the two twins get far-apart mtimes so
        // the multi-candidate pick is deterministic (c is more recent).
        fs::remove_file(root.join("a").join("dup.bin")).unwrap();
        let now = SystemTime::now();
        set_mtime(
            &root.join("b").join("dup.bin"),
            now - Duration::from_secs(90 * 86_400),
        );
        set_mtime(
            &root.join("c").join("dup.bin"),
            now - Duration::from_secs(10 * 86_400),
        );
        scan_once(&engine, &rx);

        let members = engine.coll_members(coll.id).unwrap();
        assert_eq!(members.len(), 1);
        let member = &members[0];
        assert!(member.alive, "ambiguous rebind still lands somewhere");
        assert!(
            member.hit.path.split(MAIN_SEPARATOR).any(|s| s == "c"),
            "the most recent mtime wins: {}",
            member.hit.path
        );
    }

    // ------------------------- store-level tests ---------------------------

    #[test]
    fn coll_crud_duplicate_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let a = create(&mut store, "a").unwrap();
        let b = create(&mut store, "b").unwrap();
        assert_ne!(a.id, b.id);
        assert!(matches!(
            create(&mut store, "a"),
            Err(SynsoError::Conflict(_))
        ));
        assert_eq!(list(&store).unwrap(), vec![a.clone(), b.clone()]);

        rename(&mut store, a.id, "c").unwrap();
        assert_eq!(list(&store).unwrap()[0].name, "c");
        assert!(
            matches!(rename(&mut store, b.id, "c"), Err(SynsoError::Conflict(_))),
            "renaming onto another collection's name conflicts"
        );
        assert!(matches!(
            rename(&mut store, 999, "x"),
            Err(SynsoError::NotFound)
        ));

        delete(&mut store, a.id).unwrap();
        assert!(matches!(
            delete(&mut store, a.id),
            Err(SynsoError::NotFound)
        ));
        assert_eq!(list(&store).unwrap(), vec![b]);
    }

    #[test]
    fn add_remove_members_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        let x = upsert(&mut store, "d", "x.txt", 5);
        let y = upsert(&mut store, "d", "y.txt", 6);
        let coll = create(&mut store, "c").unwrap();

        // Empty member lists are fine (the collection itself is validated).
        add(&mut store, coll.id, &[]).unwrap();
        add(&mut store, coll.id, &[x, y]).unwrap();
        add(&mut store, coll.id, &[x]).unwrap(); // re-add is idempotent
        let all = members(&store, coll.id).unwrap();
        assert_eq!(all.len(), 2);
        for (member, name, size) in [(&all[0], "x.txt", 5), (&all[1], "y.txt", 6)] {
            assert!(member.alive);
            assert_eq!(member.hit.name, name);
            assert_eq!(member.hit.size, size);
            assert_eq!(member.hit.ext, "txt");
            assert!(member.hit.path.starts_with('d'), "{}", member.hit.path);
        }

        assert!(matches!(
            add(&mut store, coll.id, &[777]),
            Err(SynsoError::NotFound)
        ));
        assert!(matches!(
            add(&mut store, 999, &[x]),
            Err(SynsoError::NotFound)
        ));

        remove(&mut store, coll.id, y).unwrap();
        assert_eq!(member_ids(&store, coll.id), vec![x]);
        assert!(matches!(
            remove(&mut store, coll.id, y),
            Err(SynsoError::NotFound)
        ));
        assert!(matches!(
            remove(&mut store, 999, x),
            Err(SynsoError::NotFound)
        ));
        // Unknown collections list empty rather than erroring.
        assert!(members(&store, 999).unwrap().is_empty());
    }

    #[test]
    fn snapshot_frozen_while_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        let id = upsert(&mut store, "d", "x.txt", 5);
        let coll = create(&mut store, "c").unwrap();
        add(&mut store, coll.id, &[id]).unwrap();

        // The file grows in place; an alive member shows the LIVE row.
        let mut grown = rec("x.txt", 500);
        store.upsert_file(&["d"], &mut grown).unwrap();
        assert_eq!(grown.id, id, "same (dir,name) row updated");
        assert_eq!(
            members(&store, coll.id).unwrap()[0].hit.size,
            500,
            "alive members reflect the live row"
        );

        // Re-adding does not refresh the snapshot, so after the row dies
        // the snapshot still holds the add-time size...
        add(&mut store, coll.id, &[id]).unwrap();
        store.delete_file(id).unwrap();
        reconcile(&mut store).unwrap();
        let dead = &members(&store, coll.id).unwrap()[0];
        assert!(!dead.alive);
        assert_eq!(dead.hit.size, 5, "the snapshot kept the add-time size");
        assert_eq!(dead.hit.name, "x.txt");

        // ...and a second reconcile with no candidate keeps it dead.
        reconcile(&mut store).unwrap();
        assert!(!members(&store, coll.id).unwrap()[0].alive);

        // Only a name+size(5) twin rebinds — the grown 500 row would not.
        let twin = upsert(&mut store, "e", "x.txt", 5);
        reconcile(&mut store).unwrap();
        let alive = &members(&store, coll.id).unwrap()[0];
        assert!(alive.alive);
        assert_eq!(alive.hit.id, twin);
    }

    #[test]
    fn rebind_tie_picks_highest_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        let p = upsert_at(&mut store, "p", "t.bin", 7, 1_700_000_000);
        let q = upsert_at(&mut store, "q", "t.bin", 7, 1_700_000_000);
        let r = upsert_at(&mut store, "r", "t.bin", 7, 1_700_000_000);
        assert!(p < q && q < r, "fresh-table rowids ascend in insert order");

        let coll = create(&mut store, "c").unwrap();
        add(&mut store, coll.id, &[p]).unwrap();
        store.delete_file(p).unwrap();
        reconcile(&mut store).unwrap();

        let alive = &members(&store, coll.id).unwrap()[0];
        assert!(alive.alive);
        assert_eq!(alive.hit.id, r, "mtime tie resolves to the highest file id");
    }

    #[test]
    fn rebind_dedupes_when_target_already_member() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        let p = upsert(&mut store, "p", "m.txt", 3);
        let q = upsert(&mut store, "q", "m.txt", 3);
        let coll = create(&mut store, "c").unwrap();
        add(&mut store, coll.id, &[p, q]).unwrap();

        // p dies; its ONLY rebind candidate is q, which the collection
        // already tracks alive — the dead duplicate collapses instead of
        // colliding on the (coll_id, file_id) primary key.
        store.delete_file(p).unwrap();
        reconcile(&mut store).unwrap();

        let alive = members(&store, coll.id).unwrap();
        assert_eq!(alive.len(), 1, "the dead duplicate was deduped away");
        assert!(alive[0].alive);
        assert_eq!(alive[0].hit.id, q);
    }

    #[test]
    fn rebind_unblocks_dead_slot_squatter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        let gone = upsert(&mut store, "p", "old.bin", 9);
        let _spare = upsert(&mut store, "q", "zzz.bin", 1);
        let coll = create(&mut store, "c").unwrap();
        add(&mut store, coll.id, &[gone]).unwrap();
        store.delete_file(gone).unwrap();
        let twin = upsert(&mut store, "r", "old.bin", 9);

        // A dead member row squatting on the rebinding target's id (its
        // stale file_id number got recycled by the twin's insert).
        store
            .conn()
            .execute(
                "INSERT INTO collection_members(coll_id, file_id, snap_name, snap_size, alive)
             VALUES (?1, ?2, 'zzz.bin', 12345, 0)",
                params![coll.id, twin],
            )
            .unwrap();

        reconcile(&mut store).unwrap();

        // The live-snapshot rebind wins the slot; the squatter (whose own
        // snapshot matches nothing) is removed, and exactly one row remains.
        let alive = members(&store, coll.id).unwrap();
        assert_eq!(alive.len(), 1);
        assert!(alive[0].alive);
        assert_eq!(alive[0].hit.id, twin);
        assert_eq!(alive[0].hit.name, "old.bin", "the tracked snapshot won");
    }
}
