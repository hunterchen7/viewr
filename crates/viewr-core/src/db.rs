//! SQLite rating mirror and unfinished-sidecar recovery journal.
//!
//! Startup can restore rating precedence from this database without waiting
//! for the metadata wave, but EXIF and in-camera metadata still come from RAW
//! container reads. Each [`Db`] owns one connection; startup reads and the
//! persistence worker may use separate instances.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use rusqlite::types::{Type, Value, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::folder::{normalize_physical_path, sidecar_owner_key};

const RATING_GENERATION_MIGRATION: &str = "rating-generation-and-owner-v7";
const RATING_READ_COMPATIBILITY_MIGRATION: &str = "rating-generation-and-owner-v6";
const DATABASE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DATABASE_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
/// Failure while opening, migrating, or updating the metadata database.
pub enum DbError {
    /// SQLite returned an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stable filesystem owner for the sidecar could not be established.
    #[error("sidecar owner: {0}")]
    SidecarOwner(#[from] std::io::Error),
    /// The latency-sensitive read path found a database that still needs a
    /// migration by the background persistence worker.
    #[error("database schema is not ready for latency-sensitive reads")]
    SchemaNotReady,
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
    /// Last recorded rating, normally in `0..=5`.
    pub rating: Option<u8>,
    /// Modification time of the sidecar represented by this row, or zero when
    /// no completed sidecar write has been recorded.
    pub sidecar_mtime_ns: i64,
    /// The database rating is newer than the sidecar and must win on load.
    pub sidecar_dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageRevisionSnapshot {
    Missing { revision: i64 },
    Present { revision: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RatingOwnerSnapshot {
    image: ImageRevisionSnapshot,
    owner_revision: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RatingGlobalSnapshot(i64);

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedPendingSidecar {
    pub owner: PathBuf,
    pub pending: PendingSidecar,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirtyOwnerRating {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub rating: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredRatingRow {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub rating: Option<u8>,
    pub sidecar_mtime_ns: i64,
    pub sidecar_dirty: bool,
    pub sidecar_owner: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub(crate) struct RatingSnapshot {
    pub by_path: HashMap<PathBuf, StoredRatingRow>,
    pub by_owner: HashMap<PathBuf, StoredRatingRow>,
}

pub(crate) enum PendingSidecarSync<E> {
    Written,
    Discarded,
    Superseded,
    WriteFailed(E),
}

pub(crate) enum PendingSidecarWrite<E> {
    Written(i64),
    Discard,
    Failed(E),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PendingSidecarSyncError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("sidecar ownership changed inside an immediate transaction")]
    OwnershipLost,
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
        Self::open_with_timeout(path, DATABASE_LOCK_TIMEOUT)
    }

    /// Opens a read-compatible schema without waiting or migrating.
    ///
    /// This is intended for best-effort reads on a latency-sensitive caller.
    /// Persistence workers should use [`Db::open`] so they can queue behind a
    /// cooperating writer instead of dropping durable recovery.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] for open or immediate lock-contention
    /// failures, and [`DbError::SchemaNotReady`] when a background open must
    /// migrate the database first.
    pub fn try_open_for_read(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::ZERO)?;
        if !migration_is_complete(&conn, RATING_GENERATION_MIGRATION)?
            && !migration_is_complete(&conn, RATING_READ_COMPATIBILITY_MIGRATION)?
        {
            return Err(DbError::SchemaNotReady);
        }
        Ok(Self { conn })
    }

    pub(crate) fn open_with_timeout(
        path: &Path,
        lock_timeout: std::time::Duration,
    ) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        // Schema initialization and sidecar publication use write
        // transactions. Let cooperating processes queue briefly instead of
        // spuriously degrading to database-free persistence on lock overlap.
        conn.busy_timeout(lock_timeout)?;
        enable_wal(&conn, lock_timeout)?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    pub(crate) fn is_lock_contention(error: &DbError) -> bool {
        matches!(error, DbError::Sqlite(error) if database_is_locked(error))
    }

    #[cfg(any(test, feature = "benchmarks"))]
    #[doc(hidden)]
    /// Creates an in-memory database with the production schema for tests and
    /// the benchmark harness.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        initialize_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Looks up a row by its UTF-8 filesystem path.
    ///
    /// Missing rows and SQLite prepare/query failures both return `None`. This
    /// makes the database a best-effort metadata accelerator rather than a
    /// requirement for opening a photo folder.
    pub fn get_image(&self, path: &str) -> Option<ImageRow> {
        let original = Path::new(path);
        let normalized = normalize_physical_path(original);
        self.get_image_path(&normalized).or_else(|| {
            (normalized != original)
                .then(|| self.get_image_path(original))
                .flatten()
        })
    }

    pub(crate) fn get_image_path(&self, path: &Path) -> Option<ImageRow> {
        self.conn
            .prepare_cached(
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1
                    AND sidecar_quarantined = 0",
            )
            .ok()?
            .query_row([path_value(path)], |row| {
                Ok(ImageRow {
                    rating: row.get::<_, Option<u8>>(0)?,
                    sidecar_mtime_ns: row.get(1)?,
                    sidecar_dirty: row.get(2)?,
                })
            })
            .ok()
    }

    #[cfg(test)]
    pub(crate) fn get_image_for_identity(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
    ) -> Option<ImageRow> {
        self.conn
            .prepare_cached(
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND sidecar_quarantined = 0",
            )
            .ok()?
            .query_row(rusqlite::params![path_value(path), size, mtime_ns], |row| {
                Ok(ImageRow {
                    rating: row.get::<_, Option<u8>>(0)?,
                    sidecar_mtime_ns: row.get(1)?,
                    sidecar_dirty: row.get(2)?,
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
        path: &str,
        size: u64,
        mtime_ns: i64,
        rating: Option<u8>,
        sidecar_mtime_ns: i64,
    ) -> Result<(), DbError> {
        let source = Path::new(path);
        let path = normalize_physical_path(source);
        let owner = sidecar_owner_key(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        migrate_legacy_source_alias(&transaction, source, &path)?;
        delete_sidecar_owner_rows(&transaction, &path, &owner)?;
        transaction.execute(
            "INSERT INTO images
               (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty,
                sidecar_quarantined, sidecar_owner, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_mtime_ns = excluded.sidecar_mtime_ns,
               sidecar_dirty = 0,
               sidecar_quarantined = 0,
               sidecar_owner = excluded.sidecar_owner,
               last_seen = excluded.last_seen",
            rusqlite::params![
                path_value(&path),
                size,
                mtime_ns,
                rating,
                sidecar_mtime_ns,
                path_value(&owner)
            ],
        )?;
        advance_sidecar_owner_revision(&transaction, &owner)?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn upsert_rating_path(
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
               sidecar_quarantined = 0,
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
        path: &str,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    ) -> Result<(), DbError> {
        self.record_rating_pending_sidecar_canonical(Path::new(path), size, mtime_ns, rating)
    }

    #[cfg(test)]
    pub(crate) fn record_rating_pending_sidecar_path(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    ) -> Result<(), DbError> {
        // Exact-path tests may intentionally use synthetic or non-UTF-8
        // paths. Give those rows a stable test-only owner so recovery queries
        // still exercise the production schema.
        let owner = sidecar_owner_key(path).unwrap_or_else(|_| path.with_extension("xmp"));
        upsert_pending_sidecar(&self.conn, path, size, mtime_ns, rating, Some(&owner))?;
        Ok(())
    }

    pub(crate) fn record_rating_pending_sidecar_canonical(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    ) -> Result<(), DbError> {
        let source = path;
        let path = normalize_physical_path(source);
        let owner = sidecar_owner_key(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        migrate_legacy_source_alias(&transaction, source, &path)?;
        journal_canonical_rating(&transaction, &path, &owner, size, mtime_ns, rating)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn rating_owner_snapshot(
        &self,
        path: &Path,
    ) -> Result<RatingOwnerSnapshot, DbError> {
        let path = normalize_physical_path(path);
        let owner = sidecar_owner_key(&path)?;
        Ok(RatingOwnerSnapshot {
            image: rating_revision_snapshot_on(&self.conn, &path)?,
            owner_revision: sidecar_owner_revision_on(&self.conn, &owner)?,
        })
    }

    pub(crate) fn rating_global_snapshot(&self) -> Result<RatingGlobalSnapshot, DbError> {
        rating_global_snapshot_on(&self.conn).map_err(Into::into)
    }

    #[cfg(test)]
    fn rating_revision_snapshot(&self, path: &Path) -> Result<ImageRevisionSnapshot, DbError> {
        rating_revision_snapshot_on(&self.conn, path).map_err(Into::into)
    }

    pub(crate) fn record_rating_pending_sidecar_if_unchanged(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
        predecessor: RatingOwnerSnapshot,
    ) -> Result<bool, DbError> {
        let path = normalize_physical_path(path);
        let owner = sidecar_owner_key(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = RatingOwnerSnapshot {
            image: rating_revision_snapshot_on(&transaction, &path)?,
            owner_revision: sidecar_owner_revision_on(&transaction, &owner)?,
        };
        if current != predecessor {
            transaction.rollback()?;
            return Ok(false);
        }
        journal_canonical_rating(&transaction, &path, &owner, size, mtime_ns, rating)?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn record_rating_pending_sidecar_if_global_unchanged(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
        predecessor: RatingGlobalSnapshot,
    ) -> Result<bool, DbError> {
        let path = normalize_physical_path(path);
        let owner = sidecar_owner_key(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if sidecar_owner_last_global_revision_on(&transaction, &owner)? > predecessor.0 {
            transaction.rollback()?;
            return Ok(false);
        }
        journal_canonical_rating(&transaction, &path, &owner, size, mtime_ns, rating)?;
        transaction.commit()?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn dirty_rating_for_owner(
        &self,
        owner: &Path,
    ) -> Result<Option<DirtyOwnerRating>, DbError> {
        self.conn
            .query_row(
                "SELECT path, size, mtime_ns, rating
                   FROM images
                  WHERE sidecar_owner = ?1
                    AND sidecar_dirty = 1
                    AND sidecar_quarantined = 0
                    AND rating IS NOT NULL",
                [path_value(owner)],
                |row| {
                    Ok(DirtyOwnerRating {
                        path: row_path(row, 0)?,
                        size: row.get(1)?,
                        mtime_ns: row.get(2)?,
                        rating: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn rating_snapshot<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        owners: &[Option<PathBuf>],
    ) -> Result<RatingSnapshot, DbError> {
        const QUERY_KEYS_PER_CHUNK: usize = 900;

        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let path_keys = deduplicated_paths(paths);
        let owner_keys = deduplicated_paths(owners.iter().filter_map(Option::as_deref));
        let mut rows = HashMap::with_capacity(path_keys.len().saturating_add(owner_keys.len()));
        for chunk in path_keys.chunks(QUERY_KEYS_PER_CHUNK) {
            query_rating_rows(&transaction, "path", chunk, &mut rows)?;
        }
        for chunk in owner_keys.chunks(QUERY_KEYS_PER_CHUNK) {
            query_rating_rows(&transaction, "sidecar_owner", chunk, &mut rows)?;
        }
        transaction.commit()?;

        let mut snapshot = RatingSnapshot {
            by_path: HashMap::with_capacity(rows.len()),
            by_owner: HashMap::with_capacity(owner_keys.len()),
        };
        for row in rows.into_values() {
            snapshot.by_path.insert(row.path.clone(), row.clone());
            if let Some(owner) = &row.sidecar_owner {
                snapshot.by_owner.insert(owner.clone(), row);
            }
        }
        Ok(snapshot)
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
    #[cfg(test)]
    pub(crate) fn complete_pending_sidecar(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
        sidecar_mtime_ns: i64,
    ) -> Result<bool, DbError> {
        let path = normalize_physical_path(path);
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
                    AND sidecar_dirty = 1
                    AND sidecar_quarantined = 0",
            )?
            .execute(rusqlite::params![
                path_value(&path),
                size,
                mtime_ns,
                rating,
                sidecar_mtime_ns
            ])?;
        Ok(changed == 1)
    }

    pub(crate) fn synchronize_pending_sidecar<E>(
        &self,
        path: &Path,
        size: u64,
        mtime_ns: i64,
        rating: u8,
        write: impl FnOnce() -> PendingSidecarWrite<E>,
    ) -> Result<PendingSidecarSync<E>, PendingSidecarSyncError> {
        // The immediate transaction serializes cooperating Viewr processes
        // before any sidecar bytes are replaced. Without this ownership
        // boundary, an older writer can publish stale XMP and only discover
        // that it lost the database compare-and-swap afterward.
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current_owner = transaction
            .query_row(
                "SELECT sidecar_owner
                   FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND rating = ?4
                    AND sidecar_dirty = 1
                    AND sidecar_quarantined = 0
                    AND sidecar_owner IS NOT NULL",
                rusqlite::params![path_value(path), size, mtime_ns, rating],
                |row| row_path(row, 0),
            )
            .optional()?;
        let Some(current_owner) = current_owner else {
            transaction.rollback()?;
            return Ok(PendingSidecarSync::Superseded);
        };

        let sidecar_mtime_ns = match write() {
            PendingSidecarWrite::Written(mtime) => mtime,
            PendingSidecarWrite::Discard => {
                let changed = transaction.execute(
                    "DELETE FROM images
                      WHERE path = ?1
                        AND size = ?2
                        AND mtime_ns = ?3
                        AND rating = ?4
                        AND sidecar_dirty = 1
                        AND sidecar_quarantined = 0",
                    rusqlite::params![path_value(path), size, mtime_ns, rating],
                )?;
                if changed != 1 {
                    return Err(PendingSidecarSyncError::OwnershipLost);
                }
                advance_sidecar_owner_revision(&transaction, &current_owner)?;
                transaction.commit()?;
                return Ok(PendingSidecarSync::Discarded);
            }
            PendingSidecarWrite::Failed(error) => {
                transaction.rollback()?;
                return Ok(PendingSidecarSync::WriteFailed(error));
            }
        };
        let changed = transaction.execute(
            "UPDATE images
                SET sidecar_mtime_ns = ?5,
                    sidecar_dirty = 0,
                    last_seen = unixepoch()
              WHERE path = ?1
                AND size = ?2
                AND mtime_ns = ?3
                AND rating = ?4
                AND sidecar_dirty = 1
                AND sidecar_quarantined = 0",
            rusqlite::params![path_value(path), size, mtime_ns, rating, sidecar_mtime_ns],
        )?;
        if changed != 1 {
            // No other SQLite writer can change the row while this immediate
            // transaction is active. Treat trigger/corruption interference as
            // a database failure and leave the journal dirty for a retry.
            return Err(PendingSidecarSyncError::OwnershipLost);
        }
        transaction.commit()?;
        Ok(PendingSidecarSync::Written)
    }

    #[cfg(test)]
    fn discard_pending_sidecar(
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
                    AND sidecar_dirty = 1
                    AND sidecar_quarantined = 0",
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
        self.pending_sidecars_with_owners()
            .map(|rows| rows.into_iter().map(|row| row.pending).collect::<Vec<_>>())
    }

    pub(crate) fn pending_sidecars_with_owners(&self) -> Result<Vec<OwnedPendingSidecar>, DbError> {
        pending_sidecars_on(&self.conn).map_err(Into::into)
    }
}

fn upsert_pending_sidecar(
    conn: &Connection,
    path: &Path,
    size: u64,
    mtime_ns: i64,
    rating: u8,
    sidecar_owner: Option<&Path>,
) -> Result<usize, rusqlite::Error> {
    let sidecar_owner = sidecar_owner.map(path_value);
    conn.execute(
        "INSERT INTO images
           (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty,
            sidecar_owner, last_seen)
         VALUES (?1, ?2, ?3, ?4, 0, 1, ?5, unixepoch())
         ON CONFLICT(path) DO UPDATE SET
           size = excluded.size,
           mtime_ns = excluded.mtime_ns,
           rating = excluded.rating,
           sidecar_dirty = 1,
           sidecar_quarantined = 0,
           sidecar_owner = COALESCE(excluded.sidecar_owner, images.sidecar_owner),
           last_seen = excluded.last_seen",
        rusqlite::params![path_value(path), size, mtime_ns, rating, sidecar_owner],
    )
}

fn journal_canonical_rating(
    conn: &Connection,
    path: &Path,
    sidecar_owner: &Path,
    size: u64,
    mtime_ns: i64,
    rating: u8,
) -> Result<(), rusqlite::Error> {
    delete_sidecar_owner_rows(conn, path, sidecar_owner)?;
    upsert_pending_sidecar(conn, path, size, mtime_ns, rating, Some(sidecar_owner))?;
    advance_sidecar_owner_revision(conn, sidecar_owner)
}

fn advance_sidecar_owner_revision(
    conn: &Connection,
    sidecar_owner: &Path,
) -> Result<(), rusqlite::Error> {
    let global_revision = conn.query_row(
        "UPDATE rating_global_revision
            SET revision = revision + 1
          WHERE singleton = 1
        RETURNING revision",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    conn.execute(
        "INSERT INTO sidecar_owner_revisions (owner, revision, global_revision)
         VALUES (?1, 1, ?2)
         ON CONFLICT(owner) DO UPDATE SET
            revision = sidecar_owner_revisions.revision + 1,
            global_revision = excluded.global_revision",
        rusqlite::params![path_value(sidecar_owner), global_revision],
    )?;
    Ok(())
}

fn rating_revision_snapshot_on(
    conn: &Connection,
    path: &Path,
) -> Result<ImageRevisionSnapshot, rusqlite::Error> {
    let (revision, present) = conn.query_row(
        "SELECT
             COALESCE(
                 (SELECT revision FROM image_revisions WHERE path = ?1),
                 0
             ),
             EXISTS(SELECT 1 FROM images WHERE path = ?1)",
        [path_value(path)],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
    )?;
    Ok(if present {
        ImageRevisionSnapshot::Present { revision }
    } else {
        ImageRevisionSnapshot::Missing { revision }
    })
}

fn sidecar_owner_revision_on(conn: &Connection, owner: &Path) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COALESCE(
             (SELECT revision
                FROM sidecar_owner_revisions
               WHERE owner = ?1),
             0
         )",
        [path_value(owner)],
        |row| row.get(0),
    )
}

fn sidecar_owner_last_global_revision_on(
    conn: &Connection,
    owner: &Path,
) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COALESCE(
             (SELECT global_revision
                FROM sidecar_owner_revisions
               WHERE owner = ?1),
             0
         )",
        [path_value(owner)],
        |row| row.get(0),
    )
}

fn rating_global_snapshot_on(conn: &Connection) -> Result<RatingGlobalSnapshot, rusqlite::Error> {
    conn.query_row(
        "SELECT revision
           FROM rating_global_revision
          WHERE singleton = 1",
        [],
        |row| row.get(0).map(RatingGlobalSnapshot),
    )
}

fn pending_sidecars_on(conn: &Connection) -> Result<Vec<OwnedPendingSidecar>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, size, mtime_ns, rating, sidecar_owner
           FROM images
          WHERE sidecar_dirty = 1
            AND sidecar_quarantined = 0
            AND sidecar_owner IS NOT NULL
            AND rating IS NOT NULL",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(OwnedPendingSidecar {
            owner: row_path(row, 4)?,
            pending: PendingSidecar {
                path: row_path(row, 0)?,
                size: row.get(1)?,
                mtime_ns: row.get(2)?,
                rating: row.get(3)?,
            },
        })
    })?;
    rows.collect()
}

fn deduplicated_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert((*path).to_path_buf()))
        .map(Path::to_path_buf)
        .collect()
}

fn query_rating_rows(
    conn: &Connection,
    indexed_column: &str,
    keys: &[PathBuf],
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    debug_assert!(matches!(indexed_column, "path" | "sidecar_owner"));
    if keys.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty, sidecar_owner
           FROM images
          WHERE sidecar_quarantined = 0
            AND {indexed_column} IN ({placeholders})"
    );
    let mut statement = conn.prepare(&sql)?;
    let mapped = statement.query_map(
        rusqlite::params_from_iter(keys.iter().map(|path| path_value(path))),
        |row| {
            Ok(StoredRatingRow {
                path: row_path(row, 0)?,
                size: row.get(1)?,
                mtime_ns: row.get(2)?,
                rating: row.get(3)?,
                sidecar_mtime_ns: row.get(4)?,
                sidecar_dirty: row.get(5)?,
                sidecar_owner: match row.get_ref(6)? {
                    ValueRef::Null => None,
                    _ => Some(row_path(row, 6)?),
                },
            })
        },
    )?;
    for row in mapped {
        let row = row?;
        rows.insert(row.path.clone(), row);
    }
    Ok(())
}

fn delete_sidecar_owner_rows(
    conn: &Connection,
    canonical: &Path,
    sidecar_owner: &Path,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "DELETE FROM images
          WHERE path <> ?1
            AND sidecar_owner = ?2",
        rusqlite::params![path_value(canonical), path_value(sidecar_owner)],
    )?;
    Ok(())
}

fn migrate_legacy_source_alias(
    conn: &Connection,
    source: &Path,
    canonical: &Path,
) -> Result<(), rusqlite::Error> {
    if source == canonical {
        return Ok(());
    }
    conn.execute(
        "UPDATE images
            SET path = ?2
          WHERE path = ?1
            AND sidecar_owner IS NULL
            AND sidecar_quarantined = 0
            AND NOT EXISTS(
                    SELECT 1
                      FROM images
                     WHERE path = ?2
                )",
        rusqlite::params![path_value(source), path_value(canonical)],
    )?;
    conn.execute(
        "DELETE FROM images
          WHERE path = ?1
            AND sidecar_owner IS NULL",
        [path_value(source)],
    )?;
    Ok(())
}

/// Looks up ratings through the same per-path database path used during
/// folder startup and returns the number of hits.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_rating_lookup(db: &Db, paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|path| db.get_image_path(path).is_some())
        .count()
}

/// Populates one clean synthetic row without filesystem owner discovery.
///
/// This is available only to the benchmark harness so lookup, reopen, and
/// indexed-journal scaling can use large databases without creating tens of
/// thousands of placeholder RAW files.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_insert_rating(
    db: &Db,
    path: &Path,
    size: u64,
    mtime_ns: i64,
    rating: u8,
) -> Result<(), DbError> {
    let owner = path.with_extension("xmp");
    let transaction = Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate)?;
    transaction.execute(
        "INSERT INTO images
           (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty,
            sidecar_quarantined, sidecar_owner, last_seen)
         VALUES (?1, ?2, ?3, ?4, 1, 0, 0, ?5, unixepoch())
         ON CONFLICT(path) DO UPDATE SET
           size = excluded.size,
           mtime_ns = excluded.mtime_ns,
           rating = excluded.rating,
           sidecar_mtime_ns = excluded.sidecar_mtime_ns,
           sidecar_dirty = 0,
           sidecar_quarantined = 0,
           sidecar_owner = excluded.sidecar_owner,
           last_seen = excluded.last_seen",
        rusqlite::params![path_value(path), size, mtime_ns, rating, path_value(&owner)],
    )?;
    advance_sidecar_owner_revision(&transaction, &owner)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_rating_cardinalities(db: &Db) -> Result<(usize, usize), DbError> {
    Ok((
        db.conn
            .query_row("SELECT COUNT(*) FROM images", [], |row| row.get(0))?,
        db.conn
            .query_row("SELECT COUNT(*) FROM sidecar_owner_revisions", [], |row| {
                row.get(0)
            })?,
    ))
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

fn migrate_legacy_dirty_ratings(conn: &Connection) -> Result<(usize, usize), rusqlite::Error> {
    #[derive(Debug)]
    struct LegacyDirty {
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
        dirty: bool,
        has_rating: bool,
    }

    let existing_owner_paths = {
        let mut statement = conn.prepare(
            "SELECT sidecar_owner, path
               FROM images
              WHERE sidecar_owner IS NOT NULL",
        )?;
        statement
            .query_map([], |row| Ok((row_path(row, 0)?, row_path(row, 1)?)))?
            .collect::<Result<HashMap<_, _>, _>>()?
    };
    let rows = {
        let mut statement = conn.prepare(
            "SELECT path, size, mtime_ns, sidecar_dirty, rating IS NOT NULL
               FROM images
              WHERE sidecar_owner IS NULL",
        )?;
        statement
            .query_map([], |row| {
                Ok(LegacyDirty {
                    path: row_path(row, 0)?,
                    size: row.get(1)?,
                    mtime_ns: row.get(2)?,
                    dirty: row.get(3)?,
                    has_rating: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut by_owner: HashMap<PathBuf, Vec<LegacyDirty>> = HashMap::new();
    let mut quarantine = Vec::new();
    for row in rows {
        let owner = match sidecar_owner_key(&row.path) {
            Ok(owner) => owner,
            Err(_) => {
                if row.dirty {
                    quarantine.push(row.path);
                }
                continue;
            }
        };
        by_owner.entry(owner).or_default().push(row);
    }

    let mut recovered = 0;
    for (owner, mut rows) in by_owner {
        // A clean alias proves that this owner had another publication
        // history, while multiple dirty aliases have no cross-path order.
        // Only a group containing one legacy row total is unambiguous.
        if rows.len() != 1 {
            quarantine.extend(rows.into_iter().filter(|row| row.dirty).map(|row| row.path));
            continue;
        }
        let row = rows.pop().expect("single legacy owner row");
        if !row.dirty {
            continue;
        }
        let identity_matches = std::fs::metadata(&row.path).ok().is_some_and(|metadata| {
            let mtime_ns = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or(0);
            metadata.len() == row.size && mtime_ns == row.mtime_ns
        });
        if !row.has_rating || !identity_matches {
            quarantine.push(row.path);
            continue;
        }
        let owner_conflicts = existing_owner_paths
            .get(&owner)
            .is_some_and(|path| path != &row.path);
        if owner_conflicts {
            quarantine.push(row.path);
            continue;
        }
        let changed = conn.execute(
            "UPDATE images
                SET sidecar_owner = ?2
              WHERE path = ?1
                AND sidecar_dirty = 1
                AND sidecar_owner IS NULL",
            rusqlite::params![path_value(&row.path), path_value(&owner)],
        )?;
        if changed == 1 {
            advance_sidecar_owner_revision(conn, &owner)?;
            recovered += 1;
        }
    }

    let mut quarantined = 0;
    for path in quarantine {
        conn.execute(
            "INSERT OR REPLACE INTO quarantined_legacy_ratings
                (path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                 last_seen, quarantined_at)
             SELECT path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                    last_seen, unixepoch()
               FROM images
              WHERE path = ?1
                AND sidecar_dirty = 1
                AND sidecar_owner IS NULL",
            [path_value(&path)],
        )?;
        quarantined += conn.execute(
            "DELETE FROM images
              WHERE path = ?1
                AND sidecar_dirty = 1
                AND sidecar_owner IS NULL",
            [path_value(&path)],
        )?;
    }
    Ok((recovered, quarantined))
}

fn initialize_schema(conn: &Connection) -> Result<(), DbError> {
    if migration_is_complete(conn, RATING_GENERATION_MIGRATION)? {
        return Ok(());
    }

    // The fast marker check above keeps ordinary opens read-only. An
    // immediate transaction serializes first-time migration, and the second
    // check prevents a waiter from repeating the row backfill after the
    // process that held the lock commits.
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if migration_is_complete(&transaction, RATING_GENERATION_MIGRATION)? {
        transaction.rollback()?;
        return Ok(());
    }

    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS images (
            path TEXT PRIMARY KEY,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            rating INTEGER,
            sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
            sidecar_dirty INTEGER NOT NULL DEFAULT 0,
            sidecar_quarantined INTEGER NOT NULL DEFAULT 0,
            sidecar_owner,
            revision INTEGER NOT NULL DEFAULT 0,
            last_seen INTEGER NOT NULL DEFAULT 0
        );",
    )?;

    if !has_column(&transaction, "images", "sidecar_dirty")? {
        transaction.execute(
            "ALTER TABLE images ADD COLUMN sidecar_dirty INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(&transaction, "images", "revision")? {
        transaction.execute(
            "ALTER TABLE images ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(&transaction, "images", "sidecar_quarantined")? {
        transaction.execute(
            "ALTER TABLE images
             ADD COLUMN sidecar_quarantined INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(&transaction, "images", "sidecar_owner")? {
        transaction.execute("ALTER TABLE images ADD COLUMN sidecar_owner", [])?;
    }
    // Conflicting or unverifiable pre-owner journals cannot be replayed
    // safely because those versions did not carry a total order across path
    // aliases. The migration below promotes only identity-valid,
    // single-owner rows and archives the rest for an explicit re-rating.
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS quarantined_legacy_ratings (
            path PRIMARY KEY,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            rating INTEGER,
            sidecar_mtime_ns INTEGER NOT NULL,
            revision INTEGER NOT NULL,
            last_seen INTEGER NOT NULL,
            quarantined_at INTEGER NOT NULL
        ) WITHOUT ROWID;",
    )?;

    // `images.revision` is the logical rating generation used by delayed
    // compare-and-swap retries. Sidecar completion bookkeeping deliberately
    // does not advance it: completing the predecessor is not a newer rating.
    // The companion ledger retains the generation after a row is deleted, so
    // a missing -> present -> missing ABA cycle cannot revive a stale retry.
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS image_revisions (
            path TEXT PRIMARY KEY,
            revision INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sidecar_owner_revisions (
            owner PRIMARY KEY,
            revision INTEGER NOT NULL,
            global_revision INTEGER NOT NULL DEFAULT 0
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS rating_global_revision (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            revision INTEGER NOT NULL
        ) WITHOUT ROWID;

        INSERT OR IGNORE INTO rating_global_revision (singleton, revision)
        VALUES (1, 0);

        CREATE INDEX IF NOT EXISTS images_pending_sidecars
            ON images(path)
         WHERE sidecar_dirty = 1
           AND sidecar_quarantined = 0
           AND rating IS NOT NULL;

        CREATE TRIGGER IF NOT EXISTS images_reject_unowned_dirty_insert
        BEFORE INSERT ON images
        WHEN NEW.sidecar_dirty = 1 AND NEW.sidecar_owner IS NULL
        BEGIN
            SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
        END;

        CREATE TRIGGER IF NOT EXISTS images_reject_unowned_dirty_update
        BEFORE UPDATE OF sidecar_dirty, sidecar_owner ON images
        WHEN NEW.sidecar_dirty = 1 AND NEW.sidecar_owner IS NULL
        BEGIN
            SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
        END;

        INSERT OR IGNORE INTO image_revisions (path, revision)
        SELECT path, revision FROM images;

        UPDATE image_revisions
           SET revision = MAX(
                   revision,
                   COALESCE(
                       (SELECT images.revision
                          FROM images
                         WHERE images.path = image_revisions.path),
                       revision
                   )
               );

        UPDATE images
           SET revision = (
                   SELECT image_revisions.revision
                     FROM image_revisions
                    WHERE image_revisions.path = images.path
               )
         WHERE revision < (
                   SELECT image_revisions.revision
                     FROM image_revisions
                    WHERE image_revisions.path = images.path
               );

        CREATE TRIGGER IF NOT EXISTS images_revision_after_insert
        AFTER INSERT ON images
        BEGIN
            INSERT INTO image_revisions (path, revision)
            VALUES (NEW.path, MAX(NEW.revision, 0) + 1)
            ON CONFLICT(path) DO UPDATE SET
                revision = MAX(image_revisions.revision, NEW.revision) + 1;
            UPDATE images
               SET revision = (
                       SELECT image_revisions.revision
                         FROM image_revisions
                        WHERE path = NEW.path
                   )
             WHERE path = NEW.path;
        END;

        CREATE TRIGGER IF NOT EXISTS images_generation_after_update_v2
        AFTER UPDATE OF size, mtime_ns, rating ON images
        BEGIN
            INSERT INTO image_revisions (path, revision)
            VALUES (NEW.path, MAX(OLD.revision, 0) + 1)
            ON CONFLICT(path) DO UPDATE SET
                revision = MAX(image_revisions.revision, OLD.revision) + 1;
            UPDATE images
               SET revision = (
                       SELECT image_revisions.revision
                         FROM image_revisions
                        WHERE path = NEW.path
                   )
             WHERE path = NEW.path;
        END;

        CREATE TRIGGER IF NOT EXISTS images_revision_after_delete
        AFTER DELETE ON images
        BEGIN
            INSERT INTO image_revisions (path, revision)
            VALUES (OLD.path, MAX(OLD.revision, 0) + 1)
            ON CONFLICT(path) DO UPDATE SET
                revision = MAX(image_revisions.revision, OLD.revision) + 1;
        END;

        DROP TRIGGER IF EXISTS images_revision_after_update;",
    )?;
    if !has_column(&transaction, "sidecar_owner_revisions", "global_revision")? {
        transaction.execute(
            "ALTER TABLE sidecar_owner_revisions
             ADD COLUMN global_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // An owner row created before v7 has no per-owner position in the global
    // order. Treat migration as its latest possible position. This is
    // conservative for retries captured before the migration and exact for
    // every snapshot captured afterward.
    transaction.execute(
        "UPDATE sidecar_owner_revisions
            SET global_revision = (
                    SELECT revision
                      FROM rating_global_revision
                     WHERE singleton = 1
                )
          WHERE global_revision = 0",
        [],
    )?;
    // v6 updates the owner ledger before the global ledger and does not know
    // about `global_revision`. These triggers make that still-live SQL shape
    // visible to v7's per-owner guard. A v7 write supplies a changed nonzero
    // value after advancing the global row, so it does not take either path.
    transaction.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS sidecar_owner_v6_insert_order
         AFTER INSERT ON sidecar_owner_revisions
         WHEN NEW.global_revision = 0
         BEGIN
             UPDATE sidecar_owner_revisions
                SET global_revision = (
                        SELECT revision + 1
                          FROM rating_global_revision
                         WHERE singleton = 1
                    )
              WHERE owner = NEW.owner;
         END;

         CREATE TRIGGER IF NOT EXISTS sidecar_owner_v6_update_order
         AFTER UPDATE OF revision ON sidecar_owner_revisions
         WHEN NEW.global_revision = OLD.global_revision
         BEGIN
             UPDATE sidecar_owner_revisions
                SET global_revision = (
                        SELECT revision + 1
                          FROM rating_global_revision
                         WHERE singleton = 1
                    )
              WHERE owner = NEW.owner;
         END;",
    )?;

    let (recovered, quarantined) = migrate_legacy_dirty_ratings(&transaction)?;
    transaction.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS images_sidecar_owners
            ON images(sidecar_owner)
         WHERE sidecar_owner IS NOT NULL;",
    )?;
    if recovered > 0 {
        eprintln!("recovered {recovered} unambiguous unfinished legacy rating(s)");
    }
    if quarantined > 0 {
        eprintln!(
            "quarantined {quarantined} conflicting or unverifiable unfinished legacy rating(s); \
             rate those photos again to publish a sidecar safely"
        );
    }

    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS viewr_schema_migrations (
            name TEXT PRIMARY KEY
        ) WITHOUT ROWID;",
    )?;
    transaction.execute(
        "INSERT INTO viewr_schema_migrations (name) VALUES (?1)",
        [RATING_GENERATION_MIGRATION],
    )?;
    transaction.commit()?;
    Ok(())
}

fn enable_wal(conn: &Connection, lock_timeout: std::time::Duration) -> Result<(), rusqlite::Error> {
    let deadline = std::time::Instant::now() + lock_timeout;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(error) if database_is_locked(&error) && std::time::Instant::now() < deadline => {
                // SQLITE_LOCKED does not invoke SQLite's busy handler. A
                // short bounded retry covers simultaneous first opens while
                // retaining the connection-level handler for SQLITE_BUSY.
                std::thread::sleep(DATABASE_LOCK_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn database_is_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn migration_is_complete(conn: &Connection, migration: &str) -> Result<bool, rusqlite::Error> {
    let table_exists = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM sqlite_schema
              WHERE type = 'table'
                AND name = 'viewr_schema_migrations'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !table_exists {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM viewr_schema_migrations
              WHERE name = ?1
         )",
        [migration],
        |row| row.get(0),
    )
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
    #[allow(clippy::type_complexity)]
    fn legacy_string_method_signatures_remain_compatible() {
        let _: fn(&Db, &str) -> Option<ImageRow> = Db::get_image;
        let _: fn(&Db, &str, u64, i64, Option<u8>, i64) -> Result<(), DbError> = Db::upsert_rating;
        let _: fn(&Db, &str, u64, i64, u8) -> Result<(), DbError> =
            Db::record_rating_pending_sidecar;
    }

    #[cfg(unix)]
    #[test]
    fn public_path_methods_round_trip_through_a_parent_alias() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let raw = physical.join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let aliased = alias.join("photo.ARW");
        let db = Db::open_in_memory().unwrap();

        db.record_rating_pending_sidecar(aliased.to_str().unwrap(), 3, 1, 4)
            .unwrap();

        assert_eq!(
            db.get_image(aliased.to_str().unwrap()).unwrap().rating,
            Some(4)
        );
        assert_eq!(db.get_image(raw.to_str().unwrap()).unwrap().rating, Some(4));
        assert!(db.complete_pending_sidecar(&aliased, 3, 1, 4, 99).unwrap());
        assert!(!db.get_image(raw.to_str().unwrap()).unwrap().sidecar_dirty);
    }

    #[cfg(unix)]
    #[test]
    fn public_getter_falls_back_to_a_legacy_symlink_spelling() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let raw = physical.join("photo.ARW");
        let legacy_raw = alias.join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let database_path = directory.path().join("legacy-alias.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE images (
                    path PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, last_seen)
                 VALUES (?1, 3, 1, 4, 99, 0)",
                [path_value(&legacy_raw)],
            )
            .unwrap();
        drop(connection);

        let db = Db::open(&database_path).unwrap();

        assert_eq!(
            db.get_image(legacy_raw.to_str().unwrap()).unwrap().rating,
            Some(4)
        );
        assert!(db.get_image(raw.to_str().unwrap()).is_none());
    }

    #[test]
    fn revision_cas_rejects_present_and_missing_aba() {
        let directory = tempfile::tempdir().unwrap();
        let path = normalize_physical_path(&directory.path().join("aba.arw"));
        std::fs::write(&path, b"raw").unwrap();
        let db = Db::open_in_memory().unwrap();
        let initially_missing = db.rating_owner_snapshot(&path).unwrap();
        assert_eq!(
            initially_missing.image,
            ImageRevisionSnapshot::Missing { revision: 0 }
        );

        assert!(
            db.record_rating_pending_sidecar_if_unchanged(&path, 10, 1, 2, initially_missing)
                .unwrap()
        );
        let rating_two = db.rating_owner_snapshot(&path).unwrap();
        assert_eq!(
            rating_two.image,
            ImageRevisionSnapshot::Present { revision: 1 }
        );

        db.record_rating_pending_sidecar_path(&path, 10, 1, 5)
            .unwrap();
        db.record_rating_pending_sidecar_path(&path, 10, 1, 2)
            .unwrap();
        assert!(
            !db.record_rating_pending_sidecar_if_unchanged(&path, 10, 1, 4, rating_two)
                .unwrap()
        );
        assert_eq!(db.get_image_path(&path).unwrap().rating, Some(2));

        assert!(db.discard_pending_sidecar(&path, 10, 1, 2).unwrap());
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Missing { revision: 4 }
        );
        assert!(
            !db.record_rating_pending_sidecar_if_unchanged(&path, 10, 1, 3, initially_missing)
                .unwrap()
        );

        let currently_missing = db.rating_owner_snapshot(&path).unwrap();
        assert!(
            db.record_rating_pending_sidecar_if_unchanged(&path, 10, 1, 3, currently_missing)
                .unwrap()
        );
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 5 }
        );
    }

    #[test]
    fn rating_generation_advances_only_for_rating_ownership_changes() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/revisions.arw");

        db.upsert_rating_path(path, 10, 1, Some(1), 10).unwrap();
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 1 }
        );
        db.upsert_rating_path(path, 10, 1, Some(2), 11).unwrap();
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 2 }
        );
        db.record_rating_pending_sidecar_path(path, 10, 1, 3)
            .unwrap();
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 3 }
        );
        assert!(db.complete_pending_sidecar(path, 10, 1, 3, 12).unwrap());
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 3 }
        );
        db.record_rating_pending_sidecar_path(path, 10, 1, 4)
            .unwrap();
        assert!(matches!(
            db.synchronize_pending_sidecar(path, 10, 1, 4, || {
                PendingSidecarWrite::<()>::Written(13)
            })
            .unwrap(),
            PendingSidecarSync::Written
        ));
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 4 }
        );
        db.record_rating_pending_sidecar_path(path, 10, 1, 5)
            .unwrap();
        assert!(db.discard_pending_sidecar(path, 10, 1, 5).unwrap());
        assert_eq!(
            db.rating_revision_snapshot(path).unwrap(),
            ImageRevisionSnapshot::Missing { revision: 6 }
        );
    }

    #[test]
    fn completing_the_predecessor_does_not_supersede_a_delayed_rating() {
        let directory = tempfile::tempdir().unwrap();
        let path = normalize_physical_path(&directory.path().join("completed-predecessor.arw"));
        std::fs::write(&path, b"raw").unwrap();
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&path, 10, 1, 2)
            .unwrap();
        let predecessor = db.rating_owner_snapshot(&path).unwrap();

        assert!(db.complete_pending_sidecar(&path, 10, 1, 2, 99).unwrap());
        assert_eq!(db.rating_owner_snapshot(&path).unwrap(), predecessor);
        assert!(
            db.record_rating_pending_sidecar_if_unchanged(&path, 10, 1, 5, predecessor)
                .unwrap()
        );

        let row = db.get_image_path(&path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(row.sidecar_dirty);
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 2 }
        );
    }

    #[test]
    fn upsert_and_read_rating() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/a.arw");
        db.upsert_rating_path(path, 10, 1, Some(4), 99).unwrap();
        let row = db.get_image_path(path).unwrap();
        assert!(db.get_image_for_identity(path, 10, 1).is_some());
        assert!(db.get_image_for_identity(path, 11, 1).is_none());
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(!row.sidecar_dirty);

        db.record_rating_pending_sidecar_path(path, 10, 1, 5)
            .unwrap();
        let row = db.get_image_path(path).unwrap();
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

        db.upsert_rating_path(path, 10, 1, Some(2), 100).unwrap();
        let row = db.get_image_path(path).unwrap();
        assert_eq!(row.rating, Some(2));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);
        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(db.get_image_path(Path::new("/p/other.arw")).is_none());
    }

    #[test]
    fn rating_snapshot_chunks_large_path_sets_without_losing_rows() {
        let db = Db::open_in_memory().unwrap();
        let paths = (0..1_901)
            .map(|index| PathBuf::from(format!("/p/{index}.arw")))
            .collect::<Vec<_>>();
        for (index, path) in paths.iter().enumerate() {
            db.upsert_rating_path(path, 10, index as i64, Some((index % 6) as u8), 1)
                .unwrap();
        }

        let snapshot = db
            .rating_snapshot(paths.iter().map(PathBuf::as_path), &vec![None; paths.len()])
            .unwrap();

        assert_eq!(snapshot.by_path.len(), paths.len());
        assert!(snapshot.by_owner.is_empty());
        assert_eq!(snapshot.by_path.get(&paths[1_900]).unwrap().rating, Some(4));
    }

    #[test]
    fn rows_survive_database_reopen_and_remain_updatable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("viewr.db");

        {
            let db = Db::open(&path).unwrap();
            db.upsert_rating_path(Path::new("/p/persistent.arw"), 42, 7, Some(5), 123)
                .unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            let row = db.get_image_path(Path::new("/p/persistent.arw")).unwrap();
            assert_eq!(row.rating, Some(5));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            db.upsert_rating_path(Path::new("/p/persistent.arw"), 84, 8, None, 456)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image_path(Path::new("/p/persistent.arw")).unwrap();
        assert!(
            db.get_image_for_identity(Path::new("/p/persistent.arw"), 84, 8)
                .is_some()
        );
        assert_eq!(row.rating, None);
        assert_eq!(row.sidecar_mtime_ns, 456);
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn sidecar_compare_and_swap_requires_every_identity_field() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/concurrent.arw");
        db.record_rating_pending_sidecar_path(path, 10, 1, 5)
            .unwrap();

        for (size, mtime_ns, rating) in [(11, 1, 5), (10, 2, 5), (10, 1, 4)] {
            assert!(
                !db.complete_pending_sidecar(path, size, mtime_ns, rating, 99)
                    .unwrap()
            );
            assert!(
                !db.discard_pending_sidecar(path, size, mtime_ns, rating)
                    .unwrap()
            );
            let row = db.get_image_path(path).unwrap();
            assert_eq!(row.rating, Some(5));
            assert!(row.sidecar_dirty);
        }

        assert!(db.complete_pending_sidecar(path, 10, 1, 5, 100).unwrap());
        let row = db.get_image_path(path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);

        assert!(!db.complete_pending_sidecar(path, 10, 1, 5, 101).unwrap());
        assert!(!db.discard_pending_sidecar(path, 10, 1, 5).unwrap());
        let missing = Path::new("/p/missing.arw");
        assert!(!db.complete_pending_sidecar(missing, 10, 1, 5, 101).unwrap());
        assert!(!db.discard_pending_sidecar(missing, 10, 1, 5).unwrap());
    }

    #[test]
    fn sidecar_sync_holds_sqlite_write_ownership_across_publication() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let path = PathBuf::from("/p/concurrent.arw");
        let owner = Db::open(&database_path).unwrap();
        let contender = Db::open(&database_path).unwrap();
        owner
            .record_rating_pending_sidecar_path(&path, 10, 1, 2)
            .unwrap();

        let (entered, entered_wait) = std::sync::mpsc::channel();
        let (release, release_wait) = std::sync::mpsc::channel();
        let owner_path = path.clone();
        let owner_thread = std::thread::spawn(move || {
            owner
                .synchronize_pending_sidecar(&owner_path, 10, 1, 2, || {
                    entered.send(()).unwrap();
                    release_wait.recv().unwrap();
                    PendingSidecarWrite::<()>::Written(99)
                })
                .unwrap()
        });
        entered_wait
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        contender
            .conn
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let error = contender
            .record_rating_pending_sidecar_path(&path, 10, 1, 5)
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(ref failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ));

        release.send(()).unwrap();
        assert!(matches!(
            owner_thread.join().unwrap(),
            PendingSidecarSync::Written
        ));

        contender
            .record_rating_pending_sidecar_path(&path, 10, 1, 5)
            .unwrap();
        assert!(matches!(
            contender
                .synchronize_pending_sidecar(&path, 10, 1, 5, || {
                    PendingSidecarWrite::<()>::Written(100)
                })
                .unwrap(),
            PendingSidecarSync::Written
        ));
        let row = contender.get_image_path(&path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, 100);
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn sidecar_discard_holds_ownership_until_the_delete_commits() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let path = PathBuf::from("/p/discard-race.arw");
        let owner = Db::open(&database_path).unwrap();
        let contender = Db::open(&database_path).unwrap();
        owner
            .record_rating_pending_sidecar_path(&path, 10, 1, 4)
            .unwrap();

        let (entered, entered_wait) = std::sync::mpsc::channel();
        let (release, release_wait) = std::sync::mpsc::channel();
        let owner_path = path.clone();
        let owner_thread = std::thread::spawn(move || {
            owner
                .synchronize_pending_sidecar(&owner_path, 10, 1, 4, || {
                    entered.send(()).unwrap();
                    release_wait.recv().unwrap();
                    PendingSidecarWrite::<()>::Discard
                })
                .unwrap()
        });
        entered_wait
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        contender
            .conn
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let error = contender
            .record_rating_pending_sidecar_path(&path, 10, 1, 4)
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::Sqlite(ref error) if database_is_locked(error)
        ));

        release.send(()).unwrap();
        assert!(matches!(
            owner_thread.join().unwrap(),
            PendingSidecarSync::Discarded
        ));

        contender
            .record_rating_pending_sidecar_path(&path, 10, 1, 4)
            .unwrap();
        let row = contender.get_image_path(&path).unwrap();
        assert_eq!(row.rating, Some(4));
        assert!(row.sidecar_dirty);
    }

    #[test]
    fn failed_sidecar_publication_rolls_back_ownership_for_a_retry() {
        let db = Db::open_in_memory().unwrap();
        let path = Path::new("/p/retry.arw");
        db.record_rating_pending_sidecar_path(path, 10, 1, 4)
            .unwrap();

        let result = db
            .synchronize_pending_sidecar(path, 10, 1, 4, || {
                PendingSidecarWrite::Failed("injected sidecar failure")
            })
            .unwrap();

        assert!(matches!(
            result,
            PendingSidecarSync::WriteFailed("injected sidecar failure")
        ));
        assert!(db.get_image_path(path).unwrap().sidecar_dirty);
        assert!(matches!(
            db.synchronize_pending_sidecar(path, 10, 1, 4, || {
                PendingSidecarWrite::<()>::Written(101)
            })
            .unwrap(),
            PendingSidecarSync::Written
        ));
        let row = db.get_image_path(path).unwrap();
        assert_eq!(row.sidecar_mtime_ns, 101);
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn opening_a_legacy_database_adds_journal_columns_without_changing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let raw = dir.path().join("legacy.arw");
        std::fs::write(&raw, b"raw").unwrap();
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
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, last_seen)
                 VALUES (?1, 42, 7, 3, 123, 0)",
                [path_value(&raw)],
            )
            .unwrap();
        }

        {
            let db = Db::open(&path).unwrap();
            let row = db.get_image(raw.to_str().unwrap()).unwrap();
            assert_eq!(row.rating, Some(3));
            assert_eq!(row.sidecar_mtime_ns, 123);
            assert!(!row.sidecar_dirty);
            assert_eq!(
                db.rating_revision_snapshot(&raw).unwrap(),
                ImageRevisionSnapshot::Present { revision: 0 }
            );
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), 42, 7, 4)
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let row = db.get_image(raw.to_str().unwrap()).unwrap();
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 123);
        assert!(row.sidecar_dirty);
        assert_eq!(
            db.rating_revision_snapshot(&normalize_physical_path(&raw))
                .unwrap(),
            ImageRevisionSnapshot::Present { revision: 1 }
        );
    }

    #[test]
    fn migration_durably_quarantines_unverifiable_legacy_dirty_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-dirty.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                VALUES
                    ('/p/dirty-legacy.arw', 42, 7, 4, 123, 1, 0),
                    ('/p/clean-legacy.arw', 42, 7, 3, 123, 0, 0);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();

        assert!(db.get_image("/p/dirty-legacy.arw").is_none());
        assert_eq!(db.get_image("/p/clean-legacy.arw").unwrap().rating, Some(3));
        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT rating
                       FROM quarantined_legacy_ratings
                      WHERE path = '/p/dirty-legacy.arw'",
                    [],
                    |row| row.get::<_, u8>(0),
                )
                .unwrap(),
            4
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*)
                       FROM images
                      WHERE path = '/p/dirty-legacy.arw'",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        assert!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty)
                     VALUES ('/p/old-writer.arw', 42, 7, 5, 1)",
                    [],
                )
                .is_err(),
            "pre-owner SQL must not recreate a live unowned dirty row"
        );
        drop(db);

        let reopened = Db::open(&path).unwrap();
        assert!(reopened.get_image("/p/dirty-legacy.arw").is_none());
        assert!(reopened.pending_sidecars().unwrap().is_empty());
    }

    #[test]
    fn migration_recovers_an_unambiguous_identity_valid_legacy_rating() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("recoverable-legacy.db");
        let raw = directory.path().join("recoverable.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let metadata = std::fs::metadata(&raw).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE images (
                    path PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                 VALUES (?1, ?2, ?3, 5, 7, 1, 0)",
                rusqlite::params![path_value(&raw), metadata.len(), mtime_ns],
            )
            .unwrap();
        drop(connection);

        let db = Db::open(&database_path).unwrap();

        assert_eq!(
            db.pending_sidecars().unwrap(),
            vec![PendingSidecar {
                path: raw.clone(),
                size: metadata.len(),
                mtime_ns,
                rating: 5,
            }]
        );
        assert_eq!(
            db.dirty_rating_for_owner(&sidecar_owner_key(&raw).unwrap())
                .unwrap()
                .unwrap()
                .rating,
            5
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn migration_quarantines_a_dirty_owner_with_a_clean_legacy_alias() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("conflicting-legacy.db");
        let dirty_raw = directory.path().join("photo.ARW");
        let clean_raw = directory.path().join("photo.DNG");
        std::fs::write(&dirty_raw, b"raw").unwrap();
        std::fs::write(&clean_raw, b"dng").unwrap();
        let identity = |path: &Path| {
            let metadata = std::fs::metadata(path).unwrap();
            let mtime_ns = metadata
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
            (metadata.len(), mtime_ns)
        };
        let dirty_identity = identity(&dirty_raw);
        let clean_identity = identity(&clean_raw);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE images (
                    path PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                 VALUES
                    (?1, ?2, ?3, 1, 0, 1, 1),
                    (?4, ?5, ?6, 5, 10, 0, 2)",
                rusqlite::params![
                    path_value(&dirty_raw),
                    dirty_identity.0,
                    dirty_identity.1,
                    path_value(&clean_raw),
                    clean_identity.0,
                    clean_identity.1,
                ],
            )
            .unwrap();
        drop(connection);

        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(db.get_image_path(&dirty_raw).is_none());
        assert_eq!(db.get_image_path(&clean_raw).unwrap().rating, Some(5));
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT rating
                       FROM quarantined_legacy_ratings
                      WHERE path = ?1",
                    [path_value(&dirty_raw)],
                    |row| row.get::<_, u8>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn owner_ledger_migration_preserves_owned_pending_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner-ledger-v6.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE images (
                    path PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    sidecar_quarantined INTEGER NOT NULL DEFAULT 0,
                    sidecar_owner,
                    revision INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE sidecar_owner_revisions (
                    owner PRIMARY KEY,
                    revision INTEGER NOT NULL
                ) WITHOUT ROWID;
                CREATE TABLE rating_global_revision (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    revision INTEGER NOT NULL
                ) WITHOUT ROWID;
                INSERT INTO rating_global_revision VALUES (1, 7);
                CREATE TABLE viewr_schema_migrations (
                    name TEXT PRIMARY KEY
                ) WITHOUT ROWID;
                INSERT INTO viewr_schema_migrations
                VALUES ('rating-generation-and-owner-v6');
                INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty,
                     sidecar_owner)
                VALUES
                    ('/p/owned.arw', 10, 1, 5, 1, '/p/owned.xmp'),
                    ('/p/legacy.arw', 20, 2, 2, 1, NULL);",
            )
            .unwrap();
        drop(connection);

        let compatible = Db::try_open_for_read(&path).unwrap();
        assert_eq!(
            compatible.pending_sidecars().unwrap(),
            vec![PendingSidecar {
                path: PathBuf::from("/p/owned.arw"),
                size: 10,
                mtime_ns: 1,
                rating: 5,
            }]
        );
        assert!(
            !has_column(
                &compatible.conn,
                "sidecar_owner_revisions",
                "global_revision"
            )
            .unwrap(),
            "the latency-sensitive read must not migrate v6"
        );
        drop(compatible);

        let db = Db::open(&path).unwrap();

        assert_eq!(
            db.pending_sidecars().unwrap(),
            vec![PendingSidecar {
                path: PathBuf::from("/p/owned.arw"),
                size: 10,
                mtime_ns: 1,
                rating: 5,
            }]
        );
        assert!(db.get_image_path(Path::new("/p/legacy.arw")).is_none());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT rating
                       FROM quarantined_legacy_ratings
                      WHERE path = '/p/legacy.arw'",
                    [],
                    |row| row.get::<_, u8>(0),
                )
                .unwrap(),
            2
        );
        assert!(has_column(&db.conn, "sidecar_owner_revisions", "global_revision").unwrap());
    }

    #[test]
    fn v6_owner_updates_advance_the_v7_global_owner_position() {
        fn advance_like_v6(conn: &Connection, owner: &Path) {
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            conn.execute(
                "INSERT INTO sidecar_owner_revisions (owner, revision)
                 VALUES (?1, 1)
                 ON CONFLICT(owner) DO UPDATE SET
                    revision = sidecar_owner_revisions.revision + 1",
                [path_value(owner)],
            )
            .unwrap();
            conn.execute(
                "UPDATE rating_global_revision
                    SET revision = revision + 1
                  WHERE singleton = 1",
                [],
            )
            .unwrap();
            conn.execute_batch("COMMIT").unwrap();
        }

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("insert.ARW");
        let second = directory.path().join("update.ARW");
        std::fs::write(&first, b"raw").unwrap();
        std::fs::write(&second, b"raw").unwrap();
        let db = Db::open_in_memory().unwrap();

        let first_predecessor = db.rating_global_snapshot().unwrap();
        let first_owner = sidecar_owner_key(&first).unwrap();
        advance_like_v6(&db.conn, &first_owner);
        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(
                &first,
                3,
                1,
                1,
                first_predecessor,
            )
            .unwrap()
        );

        db.record_rating_pending_sidecar_canonical(&second, 3, 1, 2)
            .unwrap();
        let second_predecessor = db.rating_global_snapshot().unwrap();
        let second_owner = sidecar_owner_key(&second).unwrap();
        advance_like_v6(&db.conn, &second_owner);
        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(
                &second,
                3,
                1,
                3,
                second_predecessor,
            )
            .unwrap()
        );

        let (global, last_owner) = db
            .conn
            .query_row(
                "SELECT rating_global_revision.revision,
                        sidecar_owner_revisions.global_revision
                   FROM rating_global_revision
                   JOIN sidecar_owner_revisions
                     ON sidecar_owner_revisions.owner = ?1
                  WHERE rating_global_revision.singleton = 1",
                [path_value(&second_owner)],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(last_owner, global);
    }

    #[test]
    fn warm_schema_reinitialization_performs_no_row_dml() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("warm-open.arw");
        std::fs::write(&raw, b"raw").unwrap();
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar(raw.to_str().unwrap(), 42, 7, 4)
            .unwrap();
        let changes_before = db.conn.total_changes();

        initialize_schema(&db.conn).unwrap();

        assert_eq!(db.conn.total_changes(), changes_before);
        assert!(
            migration_is_complete(&db.conn, RATING_GENERATION_MIGRATION).unwrap(),
            "the durable marker must make later initialization read-only"
        );
        assert!(
            db.conn
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1
                           FROM sqlite_schema
                          WHERE type = 'index'
                            AND name = 'images_pending_sidecars'
                     )",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
    }

    #[test]
    fn latency_sensitive_read_open_never_migrates_a_legacy_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-read.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
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
        drop(connection);

        assert!(matches!(
            Db::try_open_for_read(&path),
            Err(DbError::SchemaNotReady)
        ));
        let connection = Connection::open(&path).unwrap();
        assert!(!has_column(&connection, "images", "sidecar_owner").unwrap());
        assert!(
            !migration_is_complete(&connection, RATING_GENERATION_MIGRATION).unwrap(),
            "the UI read path must leave migration to the persistence worker"
        );
    }

    #[test]
    fn latency_sensitive_read_open_does_not_wait_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("contended-read.db");
        drop(Db::open(&path).unwrap());
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let opened = Db::try_open_for_read(&path);
        let elapsed = started.elapsed();

        assert!(opened.is_ok(), "WAL readers should coexist with a writer");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "latency-sensitive read open waited {elapsed:?}"
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn concurrent_legacy_opens_apply_the_migration_once() {
        const OPENERS: usize = 8;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent-legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
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
                VALUES ('/p/concurrent-legacy.arw', 42, 7, 3, 123, 0);",
            )
            .unwrap();
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(OPENERS));
        let openers = (0..OPENERS)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let db = Db::open(&path).unwrap();
                    assert_eq!(
                        db.get_image("/p/concurrent-legacy.arw").unwrap().rating,
                        Some(3)
                    );
                })
            })
            .collect::<Vec<_>>();
        for opener in openers {
            opener.join().unwrap();
        }

        let db = Db::open(&path).unwrap();
        let migration_rows = db
            .conn
            .query_row(
                "SELECT COUNT(*)
                   FROM viewr_schema_migrations
                  WHERE name = ?1",
                [RATING_GENERATION_MIGRATION],
                |row| row.get::<_, usize>(0),
            )
            .unwrap();
        assert_eq!(migration_rows, 1);
        assert_eq!(
            db.rating_revision_snapshot(Path::new("/p/concurrent-legacy.arw"))
                .unwrap(),
            ImageRevisionSnapshot::Present { revision: 0 }
        );
    }

    #[test]
    fn concurrent_fresh_opens_all_initialize_successfully() {
        const OPENERS: usize = 8;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent-fresh.db");
        let raw = dir.path().join("concurrent-fresh.arw");
        std::fs::write(&raw, b"raw").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(OPENERS));
        let openers = (0..OPENERS)
            .map(|_| {
                let barrier = barrier.clone();
                let path = path.clone();
                let raw = raw.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let db = Db::open(&path).unwrap();
                    db.record_rating_pending_sidecar(raw.to_str().unwrap(), 42, 7, 3)
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for opener in openers {
            opener.join().unwrap();
        }

        let db = Db::open(&path).unwrap();
        assert_eq!(db.get_image(raw.to_str().unwrap()).unwrap().rating, Some(3));
    }

    #[test]
    fn multi_process_owner_writer_helper() {
        let Some(database_path) = std::env::var_os("VIEWR_TEST_WRITER_DATABASE") else {
            return;
        };
        let raw_path: PathBuf = std::env::var_os("VIEWR_TEST_WRITER_RAW")
            .expect("child RAW path")
            .into();
        let gate_path: PathBuf = std::env::var_os("VIEWR_TEST_WRITER_GATE")
            .expect("child gate path")
            .into();
        let rating = std::env::var("VIEWR_TEST_WRITER_RATING")
            .expect("child rating")
            .parse::<u8>()
            .expect("numeric child rating");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gate_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not release the process gate"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let metadata = std::fs::metadata(&raw_path).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let db = Db::open(Path::new(&database_path)).unwrap();
        db.record_rating_pending_sidecar(
            raw_path.to_str().unwrap(),
            metadata.len(),
            mtime_ns,
            rating,
        )
        .unwrap();
    }

    #[cfg(not(miri))]
    #[test]
    fn independent_processes_share_one_sidecar_owner() {
        const WRITERS: usize = 8;

        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("multi-process.db");
        let arw = directory.path().join("photo.ARW");
        let dng = directory.path().join("photo.DNG");
        let gate_path = directory.path().join("start-writers");
        std::fs::write(&arw, b"raw").unwrap();
        std::fs::write(&dng, b"raw").unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = (0..WRITERS)
            .map(|index| {
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "db::tests::multi_process_owner_writer_helper",
                        "--nocapture",
                    ])
                    .env("VIEWR_TEST_WRITER_DATABASE", &database_path)
                    .env(
                        "VIEWR_TEST_WRITER_RAW",
                        if index % 2 == 0 { &arw } else { &dng },
                    )
                    .env("VIEWR_TEST_WRITER_GATE", &gate_path)
                    .env("VIEWR_TEST_WRITER_RATING", ((index % 5) + 1).to_string())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        std::fs::write(&gate_path, b"go").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }

        let db = Db::open(&database_path).unwrap();
        let active_rows = db
            .conn
            .query_row(
                "SELECT COUNT(*)
                   FROM images
                  WHERE sidecar_quarantined = 0",
                [],
                |row| row.get::<_, usize>(0),
            )
            .unwrap();
        assert_eq!(active_rows, 1);
        assert_eq!(db.pending_sidecars().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip_without_lossy_collisions() {
        use std::os::unix::ffi::OsStringExt as _;

        let db = Db::open_in_memory().unwrap();
        let first = PathBuf::from(OsString::from_vec(b"/p/photo-\x80.arw".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/p/photo-\x81.arw".to_vec()));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        db.record_rating_pending_sidecar_path(&first, 10, 1, 3)
            .unwrap();
        db.record_rating_pending_sidecar_path(&second, 20, 2, 5)
            .unwrap();

        assert_eq!(db.get_image_path(&first).unwrap().rating, Some(3));
        assert_eq!(db.get_image_path(&second).unwrap().rating, Some(5));
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

    #[cfg(windows)]
    #[test]
    fn non_unicode_windows_paths_round_trip_without_lossy_collisions() {
        use std::os::windows::ffi::OsStringExt as _;

        let make_path = |surrogate| {
            let mut units = "C:\\photos\\photo-".encode_utf16().collect::<Vec<_>>();
            units.push(surrogate);
            units.extend(".arw".encode_utf16());
            PathBuf::from(OsString::from_wide(&units))
        };
        let first = make_path(0xd800);
        let second = make_path(0xd801);
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());

        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&first, 10, 1, 3)
            .unwrap();
        db.record_rating_pending_sidecar_path(&second, 20, 2, 5)
            .unwrap();

        assert_eq!(db.get_image_path(&first).unwrap().rating, Some(3));
        assert_eq!(db.get_image_path(&second).unwrap().rating, Some(5));
        let mut pending = db.pending_sidecars().unwrap();
        pending.sort_by_key(|item| item.size);
        assert_eq!(pending[0].path, first);
        assert_eq!(pending[1].path, second);

        let malformed = Db::open_in_memory().unwrap();
        malformed
            .conn
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty, sidecar_owner)
                 VALUES (?1, 1, 1, 1, 1, '/p/owner.xmp')",
                rusqlite::params![vec![0xff_u8]],
            )
            .unwrap();
        assert!(malformed.pending_sidecars().is_err());
    }
}
