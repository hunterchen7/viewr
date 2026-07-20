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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS images (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                rating INTEGER,
                sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                last_seen INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self { conn })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS images (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                rating INTEGER,
                sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                last_seen INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get_image(&self, path: &str) -> Option<ImageRow> {
        self.conn
            .query_row(
                "SELECT rating, sidecar_mtime_ns FROM images WHERE path = ?1",
                [path],
                |row| {
                    Ok(ImageRow {
                        rating: row.get::<_, Option<u8>>(0)?,
                        sidecar_mtime_ns: row.get(1)?,
                    })
                },
            )
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
        self.conn.execute(
            "INSERT INTO images (path, size, mtime_ns, rating, sidecar_mtime_ns, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_mtime_ns = excluded.sidecar_mtime_ns,
               last_seen = excluded.last_seen",
            rusqlite::params![path, size, mtime_ns, rating, sidecar_mtime_ns],
        )?;
        Ok(())
    }
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
        db.upsert_rating("/p/a.arw", 10, 1, Some(2), 100).unwrap();
        assert_eq!(db.get_image("/p/a.arw").unwrap().rating, Some(2));
        assert!(db.get_image("/p/other.arw").is_none());
    }
}
