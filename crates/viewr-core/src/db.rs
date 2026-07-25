//! SQLite rating mirror and unfinished-sidecar recovery journal.
//!
//! Startup can restore rating precedence from this database without waiting
//! for the metadata wave, but EXIF and in-camera metadata still come from RAW
//! container reads. Each [`Db`] owns one connection; startup reads and the
//! persistence worker may use separate instances.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use rusqlite::types::{Type, Value, ValueRef};
use rusqlite::{Connection, Row};

#[derive(Debug, thiserror::Error)]
/// Failure while opening, migrating, or updating the metadata database.
pub enum DbError {
    /// SQLite returned an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// SQLite-backed rating and sidecar recovery journal.
///
/// A `Db` owns one connection and does not provide its own synchronization.
/// Opening enables WAL mode and applies additive schema migrations before
/// returning.
pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, Default)]
/// Rating state mirrored for one RAW path.
pub struct ImageRow {
    /// RAW size captured when this row was last updated.
    pub size: u64,
    /// RAW modification time captured when this row was last updated.
    pub mtime_ns: i64,
    /// Last recorded rating, normally in `0..=5`.
    pub rating: Option<u8>,
    /// Modification time of the sidecar represented by this row, or zero when
    /// no completed sidecar write has been recorded.
    pub sidecar_mtime_ns: i64,
    /// The database rating is newer than the sidecar and must win on load.
    pub sidecar_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A journaled rating whose XMP sidecar write must be resumed.
pub struct PendingSidecar {
    /// RAW path; its sidecar is the same path with an `xmp` extension.
    pub path: PathBuf,
    /// RAW size captured when the rating was set.
    pub size: u64,
    /// RAW modification time captured when the rating was set.
    pub mtime_ns: i64,
    /// Rating to persist, normally in `0..=5`.
    pub rating: u8,
}

/// Returns the platform-default database path, creating its parent directory.
///
/// Returns `None` when no platform configuration directory is available or
/// the parent cannot be created.
pub fn default_db_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("viewr");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("viewr.db"))
}

impl Db {
    /// Opens or creates a database, enables WAL mode, and initializes its
    /// schema.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] for open, pragma, or migration failures.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    /// Creates an in-memory database with the production schema for tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Looks up a row by its native filesystem path.
    ///
    /// Missing rows and SQLite prepare/query failures both return `None`. This
    /// makes the database a best-effort metadata accelerator rather than a
    /// requirement for opening a photo folder.
    pub fn get_image(&self, path: &Path) -> Option<ImageRow> {
        self.conn
            .prepare_cached(
                "SELECT size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1",
            )
            .ok()?
            .query_row([path_value(path)], |row| {
                Ok(ImageRow {
                    size: row.get(0)?,
                    mtime_ns: row.get(1)?,
                    rating: row.get::<_, Option<u8>>(2)?,
                    sidecar_mtime_ns: row.get(3)?,
                    sidecar_dirty: row.get(4)?,
                })
            })
            .ok()
    }

    /// Records rating state after a sidecar has been successfully synchronized.
    ///
    /// This clears any pending-sidecar marker for `path`. Rating ranges are not
    /// validated by the database layer.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] when the insert or update fails.
    pub fn upsert_rating(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: Option<u8>,
        sidecar_mtime_ns: i64,
    ) -> Result<(), DbError> {
        self.conn
            .prepare_cached(
                "INSERT INTO images
                   (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_mtime_ns = excluded.sidecar_mtime_ns,
               sidecar_dirty = 0,
               last_seen = excluded.last_seen",
            )?
            .execute(rusqlite::params![
                path_value(path),
                size,
                mtime_ns,
                rating,
                sidecar_mtime_ns
            ])?;
        Ok(())
    }

    /// Record a rating before the debounced sidecar write. Existing sidecar
    /// metadata stays intact, but the dirty flag makes this database value win
    /// if the process exits before the sidecar reaches disk.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] if preparing or executing the database
    /// update fails.
    pub fn record_rating_pending_sidecar(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    ) -> Result<(), DbError> {
        self.conn
            .prepare_cached(
                "INSERT INTO images
                   (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty, last_seen)
             VALUES (?1, ?2, ?3, ?4, 0, 1, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_dirty = 1,
               last_seen = excluded.last_seen",
            )?
            .execute(rusqlite::params![path_value(path), size, mtime_ns, rating])?;
        Ok(())
    }

    /// Mark one exact dirty rating as synchronized with its sidecar.
    ///
    /// Returns `false` if another writer has replaced the row since this
    /// operation was journaled. In that case the newer dirty rating remains
    /// authoritative.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] if preparing or executing the update fails.
    pub fn complete_pending_sidecar(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
        sidecar_mtime_ns: i64,
    ) -> Result<bool, DbError> {
        let changed = self
            .conn
            .prepare_cached(
                "UPDATE images
                    SET sidecar_mtime_ns = ?5,
                        sidecar_dirty = 0,
                        last_seen = unixepoch()
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND rating = ?4
                    AND sidecar_dirty = 1",
            )?
            .execute(rusqlite::params![
                path_value(path),
                size,
                mtime_ns,
                rating,
                sidecar_mtime_ns
            ])?;
        Ok(changed == 1)
    }

    /// Remove one exact dirty row after its RAW identity has gone stale.
    ///
    /// A conditional delete prevents an older recovery attempt from removing
    /// a newer rating for the same path.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] if preparing or executing the delete fails.
    pub fn discard_pending_sidecar(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    ) -> Result<bool, DbError> {
        let changed = self
            .conn
            .prepare_cached(
                "DELETE FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND rating = ?4
                    AND sidecar_dirty = 1",
            )?
            .execute(rusqlite::params![path_value(path), size, mtime_ns, rating])?;
        Ok(changed == 1)
    }

    /// Return dirty ratings so a new persistence worker can resume sidecar
    /// writes that a prior process did not finish.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] if preparing or reading the query fails.
    pub fn pending_sidecars(&self) -> Result<Vec<PendingSidecar>, DbError> {
        let mut statement = self.conn.prepare_cached(
            "SELECT path, size, mtime_ns, rating
               FROM images
              WHERE sidecar_dirty = 1 AND rating IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PendingSidecar {
                path: row_path(row, 0)?,
                size: row.get(1)?,
                mtime_ns: row.get(2)?,
                rating: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(path) => Value::Text(path.to_owned()),
        None => Value::Blob(encode_native_path(path.as_os_str())),
    }
}

#[cfg(unix)]
fn encode_native_path(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;

    path.as_bytes().to_vec()
}

#[cfg(windows)]
fn encode_native_path(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt as _;

    path.encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(unix)]
fn decode_native_path(bytes: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt as _;

    Some(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn decode_native_path(bytes: &[u8]) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt as _;

    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return None;
    }
    Some(OsString::from_wide(
        &chunks
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>(),
    ))
}

fn row_path(row: &Row<'_>, column: usize) -> rusqlite::Result<PathBuf> {
    match row.get_ref(column)? {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
            }),
        ValueRef::Blob(bytes) => decode_native_path(bytes).map(PathBuf::from).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                Type::Blob,
                "invalid native path encoding".into(),
            )
        }),
        value => Err(rusqlite::Error::InvalidColumnType(
            column,
            "path".to_owned(),
            value.data_type(),
        )),
    }
}

fn initialize_schema(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS images (
            path TEXT PRIMARY KEY,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            rating INTEGER,
            sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
            sidecar_dirty INTEGER NOT NULL DEFAULT 0,
            last_seen INTEGER NOT NULL DEFAULT 0
        );",
    )?;

    if !has_column(conn, "images", "sidecar_dirty")?
        && let Err(error) = conn.execute(
            "ALTER TABLE images ADD COLUMN sidecar_dirty INTEGER NOT NULL DEFAULT 0",
            [],
        )
    {
        // Another process can complete the migration after our check but
        // before ALTER obtains SQLite's schema lock.
        if !has_column(conn, "images", "sidecar_dirty")? {
            return Err(error.into());
        }
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_read_rating() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/a.arw");
        db.upsert_rating(path, 10, 1, Some(4), 99).unwrap();
        let row = db.get_image(path).unwrap();
        assert_eq!(row.size, 10);
        assert_eq!(row.mtime_ns, 1);
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(!row.sidecar_dirty);

        db.record_rating_pending_sidecar(path, 10, 1, 5).unwrap();
        let row = db.get_image(path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(row.sidecar_dirty);
        assert_eq!(
            db.pending_sidecars().unwrap(),
            vec![PendingSidecar {
                path: PathBuf::from("/p/a.arw"),
                size: 10,
                mtime_ns: 1,
                rating: 5,
            }]
        );

        db.upsert_rating(path, 10, 1, Some(2), 100).unwrap();
        let row = db.get_image(path).unwrap();
        assert_eq!(row.rating, Some(2));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);
        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(db.get_image(Path::new("/p/other.arw")).is_none());
    }

    #[test]
    fn rows_survive_database_reopen_and_remain_updatable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("viewr.db");

        {
            let db = Db::open(&path).unwrap();
            db.upsert_rating(Path::new("/p/persistent.arw"), 42, 7, Some(5), 123)
                .unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            let row = db.get_image(Path::new("/p/persistent.arw")).unwrap();
            assert_eq!(row.rating, Some(5));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            db.upsert_rating(Path::new("/p/persistent.arw"), 84, 8, None, 456)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image(Path::new("/p/persistent.arw")).unwrap();
        assert_eq!(row.size, 84);
        assert_eq!(row.mtime_ns, 8);
        assert_eq!(row.rating, None);
        assert_eq!(row.sidecar_mtime_ns, 456);
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn stale_completion_and_discard_cannot_replace_a_newer_dirty_rating() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/concurrent.arw");
        db.record_rating_pending_sidecar(path, 10, 1, 2).unwrap();
        db.record_rating_pending_sidecar(path, 10, 1, 5).unwrap();

        assert!(!db.complete_pending_sidecar(path, 10, 1, 2, 99).unwrap());
        assert!(!db.discard_pending_sidecar(path, 10, 1, 2).unwrap());
        let row = db.get_image(path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(row.sidecar_dirty);

        assert!(db.complete_pending_sidecar(path, 10, 1, 5, 100).unwrap());
        let row = db.get_image(path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn opening_a_legacy_database_adds_the_dirty_column_without_changing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, last_seen)
                VALUES ('/p/legacy.arw', 42, 7, 3, 123, 0);",
            )
            .unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            let row = db.get_image(Path::new("/p/legacy.arw")).unwrap();
            assert_eq!(row.rating, Some(3));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            db.record_rating_pending_sidecar(Path::new("/p/legacy.arw"), 42, 7, 4)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image(Path::new("/p/legacy.arw")).unwrap();
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 123);
        assert!(row.sidecar_dirty);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip_without_lossy_collisions() {
        use std::os::unix::ffi::OsStringExt as _;

        let db = Db::open_in_memory().unwrap();
        let first = PathBuf::from(OsString::from_vec(b"/p/photo-\x80.arw".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/p/photo-\x81.arw".to_vec()));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        db.record_rating_pending_sidecar(&first, 10, 1, 3).unwrap();
        db.record_rating_pending_sidecar(&second, 20, 2, 5).unwrap();

        assert_eq!(db.get_image(&first).unwrap().rating, Some(3));
        assert_eq!(db.get_image(&second).unwrap().rating, Some(5));
        let mut pending = db.pending_sidecars().unwrap();
        pending.sort_by_key(|item| item.size);
        assert_eq!(
            pending,
            vec![
                PendingSidecar {
                    path: first,
                    size: 10,
                    mtime_ns: 1,
                    rating: 3,
                },
                PendingSidecar {
                    path: second,
                    size: 20,
                    mtime_ns: 2,
                    rating: 5,
                },
            ]
        );
    }
}
