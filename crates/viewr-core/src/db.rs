//! SQLite mirror: ratings + per-file metadata so folder reopens paint
//! instantly with zero raw-file reads. One connection, used only from
//! the library's persist thread.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("no data dir")]
    NoDataDir,
}

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, Default)]
pub struct ImageRow {
    pub rating: Option<u8>,
    pub sidecar_mtime_ns: i64,
    /// The database rating is newer than the sidecar and must win on load.
    pub sidecar_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSidecar {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub rating: u8,
}

pub fn default_db_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("viewr");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("viewr.db"))
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn get_image(&self, path: &str) -> Option<ImageRow> {
        self.conn
            .prepare_cached(
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty FROM images WHERE path = ?1",
            )
            .ok()?
            .query_row([path], |row| {
                Ok(ImageRow {
                    rating: row.get::<_, Option<u8>>(0)?,
                    sidecar_mtime_ns: row.get(1)?,
                    sidecar_dirty: row.get(2)?,
                })
            })
            .ok()
    }

    pub fn upsert_rating(
        &self,
        path: &str,
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
                path,
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
    pub fn record_rating_pending_sidecar(
        &self,
        path: &str,
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
            .execute(rusqlite::params![path, size, mtime_ns, rating])?;
        Ok(())
    }

    /// Return dirty ratings so a new persistence worker can resume sidecar
    /// writes that a prior process did not finish.
    pub fn pending_sidecars(&self) -> Result<Vec<PendingSidecar>, DbError> {
        let mut statement = self.conn.prepare_cached(
            "SELECT path, size, mtime_ns, rating
               FROM images
              WHERE sidecar_dirty = 1 AND rating IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PendingSidecar {
                path: PathBuf::from(row.get::<_, String>(0)?),
                size: row.get(1)?,
                mtime_ns: row.get(2)?,
                rating: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        db.upsert_rating("/p/a.arw", 10, 1, Some(4), 99).unwrap();
        let row = db.get_image("/p/a.arw").unwrap();
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(!row.sidecar_dirty);

        db.record_rating_pending_sidecar("/p/a.arw", 10, 1, 5)
            .unwrap();
        let row = db.get_image("/p/a.arw").unwrap();
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

        db.upsert_rating("/p/a.arw", 10, 1, Some(2), 100).unwrap();
        let row = db.get_image("/p/a.arw").unwrap();
        assert_eq!(row.rating, Some(2));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);
        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(db.get_image("/p/other.arw").is_none());
    }

    #[test]
    fn rows_survive_database_reopen_and_remain_updatable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("viewr.db");

        {
            let db = Db::open(&path).unwrap();
            db.upsert_rating("/p/persistent.arw", 42, 7, Some(5), 123)
                .unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            let row = db.get_image("/p/persistent.arw").unwrap();
            assert_eq!(row.rating, Some(5));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            db.upsert_rating("/p/persistent.arw", 84, 8, None, 456)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image("/p/persistent.arw").unwrap();
        assert_eq!(row.rating, None);
        assert_eq!(row.sidecar_mtime_ns, 456);
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
            let row = db.get_image("/p/legacy.arw").unwrap();
            assert_eq!(row.rating, Some(3));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            db.record_rating_pending_sidecar("/p/legacy.arw", 42, 7, 4)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image("/p/legacy.arw").unwrap();
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 123);
        assert!(row.sidecar_dirty);
    }
}
