//! SQLite-backed store for the file index.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::pinyin::pinyin_keys;
use crate::{FileId, FileRecord, Result, SynsoError};

/// Schema of `user_version` 1, verbatim from the design. Later schema steps
/// add their own constants and bump [`SCHEMA_VERSION`].
const SCHEMA_V1_SQL: &str = "\
CREATE TABLE dirs(id INTEGER PRIMARY KEY, parent INTEGER, name TEXT, UNIQUE(parent,name));
CREATE TABLE files(id INTEGER PRIMARY KEY, dir INTEGER NOT NULL, name TEXT NOT NULL,
  size INTEGER NOT NULL, mtime INTEGER, ctime INTEGER, atime INTEGER, ext_id INTEGER NOT NULL,
  is_dir INTEGER NOT NULL, hash_first INTEGER, hash_full INTEGER, UNIQUE(dir,name));
CREATE TABLE extensions(id INTEGER PRIMARY KEY, ext TEXT UNIQUE);
CREATE TABLE collections(id INTEGER PRIMARY KEY, name TEXT UNIQUE);
CREATE TABLE collection_members(coll_id INTEGER, file_id INTEGER, snap_name TEXT, snap_size INTEGER,
  alive INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(coll_id,file_id));
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
";

/// Schema of `user_version` 2, verbatim from the design: pinyin search keys
/// on `files`, computed by [`crate::pinyin::pinyin_keys`] at upsert time.
/// Existing rows are backfilled once during migration.
const SCHEMA_V2_SQL: &str = "\
ALTER TABLE files ADD COLUMN py_full TEXT NOT NULL DEFAULT '';
ALTER TABLE files ADD COLUMN py_init TEXT NOT NULL DEFAULT '';
";

/// Current schema version, stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 2;

/// Persistent index store over a single SQLite database file.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and brings its
    /// schema up to [`SCHEMA_VERSION`].
    pub fn open(path: &Path) -> Result<Store> {
        let mut conn = Connection::open(path)?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    /// Inserts `rec` under the directory built from `dir_path` segments
    /// (e.g. `["Users", "a", "docs"]` for `Users/a/docs`), or updates the
    /// existing `(dir, name)` row. On success `rec.id`, `rec.parent` and
    /// `rec.ext_id` hold the values actually stored, and `rec.py_full` /
    /// `rec.py_init` hold the pinyin keys computed here from the name —
    /// the single choke point, so callers never pass pinyin in.
    pub fn upsert_file(&mut self, dir_path: &[&str], rec: &mut FileRecord) -> Result<()> {
        let dir_id = resolve_dir(&self.conn, dir_path)?;
        let ext_id = intern_extension(&self.conn, &rec.name)?;
        let (py_full, py_init) = pinyin_keys(&rec.name);
        // u64 columns (size, hashes) are stored as SQLite's signed 64-bit
        // integers via a bit-preserving cast.
        let id: FileId = self.conn.query_row(
            "INSERT INTO files(dir, name, size, mtime, ctime, atime, ext_id, is_dir, hash_first, hash_full, py_full, py_init)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(dir, name) DO UPDATE SET
               size = excluded.size,
               mtime = excluded.mtime,
               ctime = excluded.ctime,
               atime = excluded.atime,
               ext_id = excluded.ext_id,
               is_dir = excluded.is_dir,
               hash_first = excluded.hash_first,
               hash_full = excluded.hash_full,
               py_full = excluded.py_full,
               py_init = excluded.py_init
             RETURNING id",
            params![
                dir_id,
                rec.name,
                rec.size as i64,
                rec.mtime,
                rec.ctime,
                rec.atime,
                ext_id,
                rec.is_dir,
                rec.hash_first.map(|h| h as i64),
                rec.hash_full.map(|h| h as i64),
                py_full,
                py_init,
            ],
            |row| row.get(0),
        )?;
        rec.id = id;
        rec.parent = dir_id;
        rec.ext_id = ext_id;
        rec.py_full = py_full;
        rec.py_init = py_init;
        Ok(())
    }

    /// Deletes the `files` row `id`; `Err(NotFound)` if it does not exist.
    pub fn delete_file(&mut self, id: FileId) -> Result<()> {
        let deleted = self
            .conn
            .execute("DELETE FROM files WHERE id = ?1", params![id])?;
        if deleted == 0 {
            return Err(SynsoError::NotFound);
        }
        Ok(())
    }

    /// Deletes the directory resolved from `dir_path`, every descendant
    /// directory row, and every `files` row under them. Returns the number
    /// of `files` rows deleted (directory entries are stored as `files`
    /// rows too, so they are included in the count); `Ok(0)` when
    /// `dir_path` resolves to no existing directory, which is what lets a
    /// rescan purge a root on a fresh database. Rescans call this per root
    /// before walking it so files deleted while the app was closed do not
    /// linger in the index.
    pub fn delete_subtree(&mut self, dir_path: &[&str]) -> Result<u64> {
        let Some(dir_id) = find_dir(&self.conn, dir_path)? else {
            return Ok(0);
        };
        // A recursive CTE collects the subtree ids inside SQLite, so each
        // delete is a single statement not bound by any host-parameter
        // limit; UNION (not UNION ALL) keeps a corrupted parent cycle from
        // looping forever.
        const SUBTREE: &str = "\
             WITH RECURSIVE subtree(id) AS (\
                 SELECT id FROM dirs WHERE id = ?1 \
                 UNION \
                 SELECT child.id FROM dirs AS child \
                 JOIN subtree ON child.parent = subtree.id)";
        let files = self.conn.execute(
            &format!("{SUBTREE} DELETE FROM files WHERE dir IN (SELECT id FROM subtree)"),
            params![dir_id],
        )? as u64;
        self.conn.execute(
            &format!("{SUBTREE} DELETE FROM dirs WHERE id IN (SELECT id FROM subtree)"),
            params![dir_id],
        )?;
        Ok(files)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// Every `dirs` row as `(id, parent, name)`, id-ordered. Read source
    /// for the hot index's interned directory tree.
    pub fn all_dirs(&self) -> Result<Vec<(FileId, Option<FileId>, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent, name FROM dirs ORDER BY id")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every `files` row as a [`FileRecord`] with `id`, `parent`,
    /// `ext_id` and the pinyin keys filled from the database, id-ordered.
    /// Read source for the hot index's whole-table load.
    pub fn all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, dir, name, size, mtime, ctime, atime, ext_id, is_dir, hash_first, hash_full, py_full, py_init
             FROM files ORDER BY id",
        )?;
        // u64 columns round-trip the same bit-preserving casts upsert used.
        let rows = stmt.query_map([], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                parent: row.get(1)?,
                name: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                mtime: row.get(4)?,
                ctime: row.get(5)?,
                atime: row.get(6)?,
                ext_id: row.get(7)?,
                is_dir: row.get::<_, i64>(8)? != 0,
                hash_first: row.get::<_, Option<i64>>(9)?.map(|h| h as u64),
                hash_full: row.get::<_, Option<i64>>(10)?.map(|h| h as u64),
                py_full: row.get(11)?,
                py_init: row.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Every `extensions` row as `(id, ext)`, id-ordered. Read source for
    /// the hot index's ext-string lookup (`Hit.ext`).
    pub fn all_extensions(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, ext FROM extensions ORDER BY id")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn set_meta(&self, key: &str, val: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, val],
        )?;
        Ok(())
    }
}

/// Applies pending schema migrations and records the resulting version in
/// `PRAGMA user_version`. Reopening an already-migrated database is a no-op;
/// a database written by a newer schema version is left untouched. The steps
/// run in one transaction (SQLite DDL is transactional), so a crash
/// mid-migration cannot leave a half-migrated schema behind.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    if version < 1 {
        tx.execute_batch(SCHEMA_V1_SQL)?;
    }
    if version < 2 {
        tx.execute_batch(SCHEMA_V2_SQL)?;
        backfill_pinyin_keys(&tx)?;
    }
    // Later schema steps append `if version < 3 { ... }` blocks here.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// One-time v2 step: recompute the pinyin keys for every row written before
/// the columns existed. A whole-table pass at open time is the sanctioned
/// one-time cost.
fn backfill_pinyin_keys(conn: &Connection) -> Result<()> {
    // Collect first, update after: keeps the read cursor closed while
    // writing and mirrors the whole-table loads elsewhere.
    let rows: Vec<(FileId, String)> = {
        let mut select = conn.prepare("SELECT id, name FROM files")?;
        let mapped = select.query_map([], |row| {
            Ok((row.get::<_, FileId>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut update = conn.prepare("UPDATE files SET py_full = ?1, py_init = ?2 WHERE id = ?3")?;
    for (id, name) in rows {
        let (py_full, py_init) = pinyin_keys(&name);
        update.execute(params![py_full, py_init, id])?;
    }
    Ok(())
}

/// Resolves `segments` in `dirs`, creating missing levels (each segment's
/// parent is the previous one; the first segment's parent is NULL), and
/// returns the final dir id.
fn resolve_dir(conn: &Connection, segments: &[&str]) -> Result<FileId> {
    if segments.is_empty() {
        return Err(SynsoError::BadQuery(
            "dir_path must have at least one segment".into(),
        ));
    }
    let mut parent: Option<FileId> = None;
    let mut dir_id: FileId = 0;
    for segment in segments {
        if segment.is_empty() {
            return Err(SynsoError::BadQuery(
                "dir_path segments must be non-empty".into(),
            ));
        }
        let existing: Option<FileId> = conn
            .query_row(
                "SELECT id FROM dirs WHERE parent IS ?1 AND name = ?2",
                params![parent, segment],
                |row| row.get(0),
            )
            .optional()?;
        dir_id = match existing {
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO dirs(parent, name) VALUES (?1, ?2)",
                    params![parent, segment],
                )?;
                conn.last_insert_rowid()
            }
        };
        parent = Some(dir_id);
    }
    Ok(dir_id)
}

/// Like [`resolve_dir`] but never creates rows: resolves `segments` in
/// `dirs` and returns the final dir id, or `None` as soon as a segment is
/// missing. Empty segments are a caller error, mirroring `resolve_dir`.
fn find_dir(conn: &Connection, segments: &[&str]) -> Result<Option<FileId>> {
    if segments.is_empty() {
        return Err(SynsoError::BadQuery(
            "dir_path must have at least one segment".into(),
        ));
    }
    let mut parent: Option<FileId> = None;
    let mut dir_id: Option<FileId> = None;
    for segment in segments {
        if segment.is_empty() {
            return Err(SynsoError::BadQuery(
                "dir_path segments must be non-empty".into(),
            ));
        }
        dir_id = conn
            .query_row(
                "SELECT id FROM dirs WHERE parent IS ?1 AND name = ?2",
                params![parent, *segment],
                |row| row.get(0),
            )
            .optional()?;
        match dir_id {
            Some(id) => parent = Some(id),
            None => return Ok(None),
        }
    }
    Ok(dir_id)
}

/// Interns `name`'s extension (lowercase, no leading dot) in `extensions`
/// and returns its row id. A name without an extension maps to the sentinel
/// `0` (no row is interned for it).
fn intern_extension(conn: &Connection, name: &str) -> Result<i64> {
    // Path::extension gives std dotfile semantics (".gitignore" has no ext);
    // lossy conversion keeps interning on the String ToSql impl.
    let Some(ext) = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
    else {
        return Ok(0);
    };
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM extensions WHERE ext = ?1",
            params![ext],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match existing {
        Some(id) => id,
        None => {
            conn.execute("INSERT INTO extensions(ext) VALUES (?1)", params![ext])?;
            conn.last_insert_rowid()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::SCHEMA_V1_SQL;
    use crate::model::FileRecord;
    use crate::store::Store;
    use crate::SynsoError;
    use rusqlite::{params, Connection};

    /// Minimal record builder so each test only states what it cares about.
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
            hash_first: Some(0x00ff_00ff),
            hash_full: None,
            py_full: String::new(),
            py_init: String::new(),
        }
    }

    #[test]
    fn open_creates_schema_and_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");

        // First open: schema created, user_version bumped to 2.
        {
            let store = Store::open(&db_path).unwrap();
            let version: i64 = store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, 2);
        }

        // Second open on the same file: no error, still at 2 (idempotent).
        let store = Store::open(&db_path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);

        // Every v1 table exists and starts empty.
        for table in [
            "dirs",
            "files",
            "extensions",
            "collections",
            "collection_members",
            "meta",
        ] {
            let count: i64 = store
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "table {table} should exist and be empty");
        }
    }

    #[test]
    fn upsert_and_reload_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let mut store = Store::open(&db_path).unwrap();

        // a/b/report.docx
        let mut first = rec("report.docx", 1024);
        store.upsert_file(&["a", "b"], &mut first).unwrap();
        assert!(first.id > 0, "upsert must assign the row id into rec.id");

        // Same (dir, name) with a new size: the existing row is updated in place.
        let mut second = rec("report.docx", 2048);
        store.upsert_file(&["a", "b"], &mut second).unwrap();
        assert_eq!(second.id, first.id, "re-upsert must keep the same row");

        let conn = Connection::open(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "row count must still be 1 after re-upsert");

        let (dir, size, hash_first, hash_full): (i64, i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT dir, size, hash_first, hash_full FROM files WHERE id = ?1",
                params![first.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(size, 2048, "size must be updated");
        assert_eq!(hash_first, Some(0x00ff_00ff_u64 as i64));
        assert_eq!(hash_full, None);

        // dir_path segments resolved as a -> b with correct parent links.
        let (dir_a, parent_a): (i64, Option<i64>) = conn
            .query_row("SELECT id, parent FROM dirs WHERE name = 'a'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        let (dir_b, parent_b): (i64, Option<i64>) = conn
            .query_row("SELECT id, parent FROM dirs WHERE name = 'b'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(parent_a, None, "first segment's parent is NULL");
        assert_eq!(parent_b, Some(dir_a));
        assert_eq!(dir, dir_b, "file row sits under the final dir id");
    }

    #[test]
    fn meta_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let store = Store::open(&db_path).unwrap();

        assert_eq!(store.get_meta("missing").unwrap(), None);
        store.set_meta("schema", "v1").unwrap();
        assert_eq!(store.get_meta("schema").unwrap().as_deref(), Some("v1"));
        // set_meta upserts: the same key is overwritten.
        store.set_meta("schema", "v2").unwrap();
        assert_eq!(store.get_meta("schema").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn delete_file_removes_row_and_errors_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let mut store = Store::open(&db_path).unwrap();

        let mut record = rec("gone.txt", 10);
        store.upsert_file(&["x"], &mut record).unwrap();
        store.delete_file(record.id).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);

        assert!(matches!(
            store.delete_file(record.id),
            Err(SynsoError::NotFound)
        ));
    }

    #[test]
    fn delete_subtree_removes_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        // Tree: a/{fa}, a/b/{fb, sub(dir entry)}, a/b/c/{fc}; sibling z/{fz}.
        // "sub" is a files row with is_dir = 1 (the scanner records walked
        // directories that way) but has no dirs row of its own.
        for (dir_path, name, is_dir) in [
            (vec!["a"], "fa.txt", false),
            (vec!["a", "b"], "fb.txt", false),
            (vec!["a", "b", "c"], "fc.txt", false),
            (vec!["a", "b"], "sub", true),
            (vec!["z"], "fz.txt", false),
        ] {
            let mut record = rec(name, 1);
            record.is_dir = is_dir;
            store.upsert_file(&dir_path, &mut record).unwrap();
        }

        let deleted = store.delete_subtree(&["a"]).unwrap();
        assert_eq!(
            deleted, 4,
            "fa.txt, fb.txt, fc.txt and the dir-entry row sub are removed"
        );

        let remaining_files: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining_files, 1, "only fz.txt survives");
        let remaining_dirs: Vec<String> = {
            let mut stmt = store
                .conn
                .prepare("SELECT name FROM dirs ORDER BY name")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            remaining_dirs,
            vec!["z".to_string()],
            "a, a/b, a/b/c dir rows are gone; sub never had one"
        );

        // A path that resolves to nothing deletes nothing and is not an
        // error (fresh databases have no root-segment row yet).
        assert_eq!(store.delete_subtree(&["missing"]).unwrap(), 0);
        // Empty segment lists are a caller error, same as upsert_file.
        assert!(matches!(
            store.delete_subtree(&[]),
            Err(SynsoError::BadQuery(_))
        ));
    }

    #[test]
    fn upsert_persists_pinyin_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();

        let mut report = rec("报告.docx", 10);
        store.upsert_file(&["docs"], &mut report).unwrap();
        // ... and handed them back on the record, like id/parent/ext_id.
        assert_eq!(report.py_full, "baogao.docx");
        assert_eq!(report.py_init, "bg.docx");

        // The upsert choke point computed and persisted both keys from the
        // name — callers never pass pinyin in.
        let (py_full, py_init): (String, String) = store
            .conn
            .query_row(
                "SELECT py_full, py_init FROM files WHERE id = ?1",
                params![report.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(py_full, "baogao.docx");
        assert_eq!(py_init, "bg.docx");
    }

    #[test]
    fn migration_v2_backfills_pinyin_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");

        // A database frozen at schema v1, with rows written before the
        // pinyin columns existed.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(SCHEMA_V1_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO dirs(id, parent, name) VALUES (1, NULL, 'docs');
                 INSERT INTO files(dir, name, size, ext_id, is_dir) VALUES
                   (1, '报告.docx', 10, 0, 0),
                   (1, '文件ABC', 10, 0, 0);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        // Opening migrates to v2 and backfills the new columns in place.
        let store = Store::open(&db_path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        for (name, py_full, py_init) in [
            ("报告.docx", "baogao.docx", "bg.docx"),
            ("文件ABC", "wenjianabc", "wjabc"),
        ] {
            let (full, init): (String, String) = store
                .conn
                .query_row(
                    "SELECT py_full, py_init FROM files WHERE name = ?1",
                    params![name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(full, py_full, "py_full backfilled for {name}");
            assert_eq!(init, py_init, "py_init backfilled for {name}");
        }

        // Reopening the migrated database is a no-op and keeps the values.
        drop(store);
        let store = Store::open(&db_path).unwrap();
        let full: String = store
            .conn
            .query_row(
                "SELECT py_full FROM files WHERE name = '报告.docx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(full, "baogao.docx");
    }

    #[test]
    fn extension_normalization_lowercase_no_dot_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let mut store = Store::open(&db_path).unwrap();

        let mut upper = rec("Archive.ZIP", 1);
        store.upsert_file(&["d"], &mut upper).unwrap();
        let mut plain = rec("README", 1);
        store.upsert_file(&["d"], &mut plain).unwrap();

        assert!(upper.ext_id > 0, "dotted extension interns to a real row");
        assert_eq!(plain.ext_id, 0, "no extension maps to ext_id 0");

        // The whole table reads back id-ordered for the hot-index load.
        assert_eq!(
            store.all_extensions().unwrap(),
            vec![(upper.ext_id, "zip".to_string())]
        );

        let conn = Connection::open(&db_path).unwrap();
        let ext: String = conn
            .query_row(
                "SELECT ext FROM extensions WHERE id = ?1",
                params![upper.ext_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ext, "zip", "extension stored lowercase without leading dot");
        let exts: i64 = conn
            .query_row("SELECT COUNT(*) FROM extensions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(exts, 1, "no sentinel/empty row is interned for missing ext");
    }
}
