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

use crate::folder::{
    normalize_physical_path, raw_path_from_sidecar_owner, sidecar_owner_collision_token,
    sidecar_owner_key,
};

const RATING_GENERATION_MIGRATION: &str = "rating-generation-and-owner-v7";
const RATING_READ_COMPATIBILITY_MIGRATION: &str = "rating-generation-and-owner-v6";
const SIDECAR_OWNER_KEY_MIGRATION: &str = "sidecar-owner-filesystem-identity-v8";
const SIDECAR_OWNER_REPAIR_REQUIRED: &str = "sidecar-owner-repair-required-v8";
const CURRENT_OWNER_KEY_VERSION: i64 = 8;
const DATABASE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DATABASE_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);
const SIDECAR_OWNER_INDEX_SQL: &str = "CREATE UNIQUE INDEX images_sidecar_owners
    ON images(sidecar_owner)
 WHERE sidecar_owner IS NOT NULL";
const LEGACY_OWNER_INSERT_FENCE_SQL: &str = "CREATE TRIGGER images_reject_legacy_owner_insert
     BEFORE INSERT ON images
     WHEN COALESCE(NEW.owner_key_version, 0) < 8
     BEGIN
         UPDATE images
            SET sidecar_dirty = 1
          WHERE path = NEW.path
            AND sidecar_owner IS NOT NULL
            AND sidecar_quarantined = 0
            AND owner_key_version >= 8;
         SELECT CASE
             WHEN EXISTS(
                 SELECT 1
                   FROM images
                  WHERE path = NEW.path
                    AND sidecar_owner IS NOT NULL
                    AND sidecar_quarantined = 0
                    AND owner_key_version >= 8
             )
             THEN RAISE(IGNORE)
             ELSE RAISE(ABORT, 'rating writer uses an obsolete owner key')
         END;
     END";
const LEGACY_OWNER_UPDATE_FENCE_SQL: &str = "CREATE TRIGGER images_reject_legacy_rating_update
     BEFORE UPDATE OF size, mtime_ns, rating, sidecar_owner ON images
     WHEN COALESCE(NEW.owner_key_version, 0)
       <= COALESCE(OLD.owner_key_version, 0)
     BEGIN
         SELECT RAISE(ABORT, 'rating writer uses an obsolete owner key');
     END";
const PENDING_SIDECAR_INDEX_SQL: &str = "CREATE INDEX images_pending_sidecars
    ON images(path)
 WHERE sidecar_dirty = 1
   AND sidecar_quarantined = 0
   AND rating IS NOT NULL";
const UNOWNED_DIRTY_INSERT_FENCE_SQL: &str = "CREATE TRIGGER images_reject_unowned_dirty_insert
     BEFORE INSERT ON images
     WHEN NEW.sidecar_dirty = 1
      AND NEW.sidecar_owner IS NULL
      AND COALESCE(NEW.owner_key_version, 0) >= 8
     BEGIN
         SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
     END";
const UNOWNED_DIRTY_UPDATE_FENCE_SQL: &str = "CREATE TRIGGER images_reject_unowned_dirty_update
     BEFORE UPDATE OF sidecar_dirty, sidecar_owner ON images
     WHEN NEW.sidecar_dirty = 1
      AND NEW.sidecar_owner IS NULL
      AND COALESCE(NEW.owner_key_version, 0) >= 8
     BEGIN
         SELECT RAISE(ABORT, 'dirty rating requires a sidecar owner');
     END";
const IMAGE_REVISION_INSERT_SQL: &str = "CREATE TRIGGER images_revision_after_insert
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
     END";
const IMAGE_GENERATION_UPDATE_SQL: &str = "CREATE TRIGGER images_generation_after_update_v2
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
     END";
const IMAGE_REVISION_DELETE_SQL: &str = "CREATE TRIGGER images_revision_after_delete
     AFTER DELETE ON images
     BEGIN
         INSERT INTO image_revisions (path, revision)
         VALUES (OLD.path, MAX(OLD.revision, 0) + 1)
         ON CONFLICT(path) DO UPDATE SET
             revision = MAX(image_revisions.revision, OLD.revision) + 1;
     END";
const V6_OWNER_INSERT_ORDER_SQL: &str = "CREATE TRIGGER sidecar_owner_v6_insert_order
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
     END";
const V6_OWNER_UPDATE_ORDER_SQL: &str = "CREATE TRIGGER sidecar_owner_v6_update_order
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
     END";
const PENDING_SIDECARS_QUERY: &str = "SELECT path, size, mtime_ns, rating, sidecar_owner
   FROM images
  WHERE sidecar_dirty = 1
    AND sidecar_quarantined = 0
    AND rating IS NOT NULL";

#[derive(Debug, thiserror::Error)]
/// Failure while opening, migrating, or updating the metadata database.
pub enum DbError {
    /// SQLite access or database-value conversion failed.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// SQLite opened the database but declined the required WAL mode.
    #[error("SQLite declined WAL journal mode and selected {actual:?}")]
    WalUnavailable {
        /// Journal mode SQLite actually selected.
        actual: String,
    },
}

/// SQLite-backed rating and sidecar recovery journal.
///
/// A `Db` owns one connection and does not provide its own synchronization.
/// Opening enables WAL mode and applies additive schema migrations before
/// returning.
pub struct Db {
    conn: Connection,
    read_schema: RatingReadSchema,
    dynamic_read_schema: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatingReadSchema {
    Owner,
    LegacyOwner,
    LegacyDirty,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RatingOwnerSnapshot {
    owner: PathBuf,
    image: ImageRevisionSnapshot,
    owner_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RatingGlobalSnapshot {
    path: PathBuf,
    image: ImageRevisionSnapshot,
    global_revision: i64,
    ownerless_revision: i64,
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
    pub legacy_owners_require_derivation: bool,
}

#[derive(Debug)]
pub(crate) enum PendingSidecarSync<E> {
    Written,
    Discarded,
    OwnerChanged,
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
    #[error("sidecar owner is temporarily unavailable: {0}")]
    SidecarOwner(#[source] std::io::Error),
    #[error("sidecar ownership changed inside an immediate transaction")]
    OwnershipLost,
}

/// Returns the platform-default database path, creating its parent directory.
///
/// Returns `None` when no platform configuration directory is available or
/// the parent cannot be created.
pub fn default_db_path() -> Option<PathBuf> {
    let path = configured_db_path()?;
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    Some(path)
}

pub(crate) fn configured_db_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("viewr").join("viewr.db"))
}

impl Db {
    /// Opens or creates a database, enables WAL mode, and initializes its
    /// schema.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlite`] for open, pragma, or migration failures,
    /// and [`DbError::WalUnavailable`] when the storage location cannot use
    /// the WAL journal mode required by concurrent readers and persistence.
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
    /// failures. Returns `Ok(None)` when the background persistence worker
    /// must migrate the database before it is safe to read.
    pub fn try_open_for_read(path: &Path) -> Result<Option<Self>, DbError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::ZERO)?;
        let transaction = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(read_schema) = detect_read_schema(&transaction)? else {
            transaction.commit()?;
            return Ok(None);
        };
        transaction.commit()?;
        Ok(Some(Self {
            conn,
            read_schema,
            dynamic_read_schema: true,
        }))
    }

    /// Returns whether this handle opened a current owner-key schema.
    ///
    /// A read-compatible legacy handle remains useful for immediate display,
    /// but callers should refresh after the background writer migrates it.
    #[doc(hidden)]
    pub fn rating_schema_is_current(&self) -> bool {
        self.read_schema == RatingReadSchema::Owner
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
        Ok(Self {
            conn,
            read_schema: RatingReadSchema::Owner,
            dynamic_read_schema: false,
        })
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
        Ok(Self {
            conn,
            read_schema: RatingReadSchema::Owner,
            dynamic_read_schema: false,
        })
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
        let sql = match self.read_schema {
            RatingReadSchema::Owner | RatingReadSchema::LegacyOwner => {
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1
                    AND sidecar_quarantined = 0"
            }
            RatingReadSchema::LegacyDirty => {
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1"
            }
        };
        self.conn
            .prepare_cached(sql)
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
        let sql = match self.read_schema {
            RatingReadSchema::Owner | RatingReadSchema::LegacyOwner => {
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND sidecar_quarantined = 0"
            }
            RatingReadSchema::LegacyDirty => {
                "SELECT rating, sidecar_mtime_ns, sidecar_dirty
                   FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3"
            }
        };
        self.conn
            .prepare_cached(sql)
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
    /// validated by the database layer. If the RAW cannot currently be
    /// resolved, the clean row remains ownerless until a later resolvable
    /// update assigns its physical sidecar owner.
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
        let normalized = normalize_physical_path(source);
        let owner = sidecar_owner_key(&normalized).ok();
        let path = owner
            .as_ref()
            .map_or_else(|| source.to_path_buf(), |_| normalized);
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        migrate_legacy_source_alias(&transaction, source, &path)?;
        if let Some(owner) = &owner {
            delete_sidecar_owner_rows(&transaction, &path, owner)?;
        }
        transaction.execute(
            "INSERT INTO images
               (path, size, mtime_ns, rating, sidecar_mtime_ns, sidecar_dirty,
                sidecar_quarantined, sidecar_owner, owner_key_version, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, 8, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_mtime_ns = excluded.sidecar_mtime_ns,
               sidecar_dirty = 0,
               sidecar_quarantined = 0,
               sidecar_owner = excluded.sidecar_owner,
               owner_key_version = MAX(images.owner_key_version + 1, 8),
               last_seen = excluded.last_seen",
            rusqlite::params![
                path_value(&path),
                size,
                mtime_ns,
                rating,
                sidecar_mtime_ns,
                owner.as_deref().map(path_value)
            ],
        )?;
        if let Some(owner) = &owner {
            advance_sidecar_owner_revision(&transaction, owner)?;
        } else {
            advance_ownerless_revision(&transaction)?;
        }
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
                   (path, size, mtime_ns, rating, sidecar_mtime_ns,
                    sidecar_dirty, owner_key_version, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 8, unixepoch())
             ON CONFLICT(path) DO UPDATE SET
               size = excluded.size,
               mtime_ns = excluded.mtime_ns,
               rating = excluded.rating,
               sidecar_mtime_ns = excluded.sidecar_mtime_ns,
               sidecar_dirty = 0,
               sidecar_quarantined = 0,
               owner_key_version = MAX(images.owner_key_version + 1, 8),
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
    /// update fails, or if `path` cannot be resolved to a stable physical
    /// sidecar owner. An unresolved write is rejected so it cannot publish
    /// through a guessed ownership key.
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
        let owner = resolved_sidecar_owner(&path)?;
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
        self.rating_owner_snapshot_with_hook(path, || {})
    }

    fn rating_owner_snapshot_with_hook(
        &self,
        path: &Path,
        after_image_read: impl FnOnce(),
    ) -> Result<RatingOwnerSnapshot, DbError> {
        let path = normalize_physical_path(path);
        let owner = resolved_sidecar_owner(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let image = rating_revision_snapshot_on(&transaction, &path)?;
        after_image_read();
        let owner_revision = sidecar_owner_revision_on(&transaction, &owner)?;
        transaction.commit()?;
        Ok(RatingOwnerSnapshot {
            owner,
            image,
            owner_revision,
        })
    }

    pub(crate) fn rating_global_snapshot(
        &self,
        path: &Path,
    ) -> Result<RatingGlobalSnapshot, DbError> {
        let path = normalize_physical_path(path);
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let (global_revision, ownerless_revision) = rating_global_revisions_on(&transaction)?;
        let image = rating_revision_snapshot_on(&transaction, &path)?;
        transaction.commit()?;
        Ok(RatingGlobalSnapshot {
            path,
            image,
            global_revision,
            ownerless_revision,
        })
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
        let owner = resolved_sidecar_owner(&path)?;
        if owner != predecessor.owner {
            return Ok(false);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = RatingOwnerSnapshot {
            owner: owner.clone(),
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
        if path != normalize_physical_path(&predecessor.path) {
            return Ok(false);
        }
        let owner = resolved_sidecar_owner(&path)?;
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (_, ownerless_revision) = rating_global_revisions_on(&transaction)?;
        if ownerless_revision != predecessor.ownerless_revision
            || rating_revision_snapshot_on(&transaction, &predecessor.path)? != predecessor.image
            || sidecar_owner_last_global_revision_on(&transaction, &owner)?
                > predecessor.global_revision
        {
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
        self.rating_snapshot_with_legacy_strategy(paths, owners, false)
    }

    #[cfg(feature = "benchmarks")]
    pub(crate) fn benchmark_full_legacy_rating_snapshot<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        owners: &[Option<PathBuf>],
    ) -> Result<RatingSnapshot, DbError> {
        self.rating_snapshot_with_legacy_strategy(paths, owners, true)
    }

    fn rating_snapshot_with_legacy_strategy<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        owners: &[Option<PathBuf>],
        full_legacy_scan: bool,
    ) -> Result<RatingSnapshot, DbError> {
        const QUERY_KEYS_PER_CHUNK: usize = 900;

        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let read_schema = if self.dynamic_read_schema {
            detect_read_schema(&transaction)?.ok_or(rusqlite::Error::InvalidQuery)?
        } else {
            self.read_schema
        };
        let path_keys = deduplicated_paths(paths);
        let owner_keys = match (read_schema, full_legacy_scan) {
            (RatingReadSchema::LegacyOwner | RatingReadSchema::LegacyDirty, true) => Vec::new(),
            (RatingReadSchema::Owner, _) | (RatingReadSchema::LegacyOwner, false) => {
                deduplicated_paths(owners.iter().filter_map(Option::as_deref))
            }
            (RatingReadSchema::LegacyDirty, false) => Vec::new(),
        };
        let mut rows = HashMap::with_capacity(path_keys.len().saturating_add(owner_keys.len()));
        match read_schema {
            RatingReadSchema::LegacyDirty => {
                if full_legacy_scan {
                    query_all_legacy_rating_rows(&transaction, &mut rows)?;
                } else {
                    for chunk in path_keys.chunks(QUERY_KEYS_PER_CHUNK) {
                        query_legacy_rating_rows(&transaction, chunk, &mut rows)?;
                    }
                    query_all_dirty_legacy_rating_rows(&transaction, &mut rows)?;
                }
            }
            RatingReadSchema::LegacyOwner => {
                if full_legacy_scan {
                    query_all_legacy_owner_rating_rows(&transaction, &mut rows)?;
                } else {
                    for chunk in path_keys.chunks(QUERY_KEYS_PER_CHUNK) {
                        query_rating_rows(&transaction, "path", chunk, &mut rows)?;
                    }
                    query_all_dirty_owner_rating_rows(&transaction, &mut rows)?;
                }
            }
            RatingReadSchema::Owner => {
                for chunk in path_keys.chunks(QUERY_KEYS_PER_CHUNK) {
                    query_rating_rows(&transaction, "path", chunk, &mut rows)?;
                }
            }
        }
        for chunk in owner_keys.chunks(QUERY_KEYS_PER_CHUNK) {
            query_rating_rows(&transaction, "sidecar_owner", chunk, &mut rows)?;
        }
        if read_schema != RatingReadSchema::Owner && !full_legacy_scan {
            // Legacy schemas can contain a clean or dirty rating under an
            // older directory spelling, or under a sibling RAW extension
            // that shares the requested XMP owner. Scan only the lightweight
            // path column and load full rows for conservative filename-token
            // matches. Exact filesystem-derived owner checks in
            // `resolve_rating_snapshot` remain authoritative.
            let relevant_tokens = path_keys
                .iter()
                .chain(owners.iter().filter_map(Option::as_ref))
                .filter_map(|path| sidecar_owner_collision_token(path))
                .collect::<HashSet<_>>();
            if !relevant_tokens.is_empty() {
                let candidates =
                    query_legacy_clean_alias_paths(&transaction, read_schema, &relevant_tokens)?;
                for chunk in candidates.chunks(QUERY_KEYS_PER_CHUNK) {
                    match read_schema {
                        RatingReadSchema::LegacyDirty => {
                            query_legacy_rating_rows(&transaction, chunk, &mut rows)?;
                        }
                        RatingReadSchema::LegacyOwner => {
                            query_rating_rows(&transaction, "path", chunk, &mut rows)?;
                        }
                        RatingReadSchema::Owner => unreachable!("legacy branch"),
                    }
                }
            }
        }
        if read_schema == RatingReadSchema::LegacyOwner {
            // Pre-v8 owner spellings are only query hints. Filesystem-derived
            // identity remains authoritative for grouping these rows.
            for row in rows.values_mut() {
                row.sidecar_owner = None;
            }
        }
        transaction.commit()?;

        let mut snapshot = RatingSnapshot {
            by_path: HashMap::with_capacity(rows.len()),
            by_owner: HashMap::with_capacity(owner_keys.len()),
            legacy_owners_require_derivation: read_schema != RatingReadSchema::Owner,
        };
        for row in rows.into_values() {
            snapshot.by_path.insert(row.path.clone(), row.clone());
            if let Some(owner) = &row.sidecar_owner
                && snapshot.by_owner.insert(owner.clone(), row).is_some()
            {
                return Err(rusqlite::Error::InvalidQuery.into());
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
        write: impl FnOnce(&Path) -> PendingSidecarWrite<E>,
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

        // Legacy journals can retain an aliased path even after their owner
        // was migrated to a physical key. Resolve the publication path only
        // after acquiring database ownership, then require it to identify the
        // same XMP target. This prevents a retargeted parent symlink from
        // redirecting a recovered rating to a different RAW.
        let mut publication_path = normalize_physical_path(path);
        let publication_owner = match sidecar_owner_key(&publication_path) {
            Ok(owner) => owner,
            Err(primary_error) => {
                // A v7 journal may retain a parent-symlink spelling even
                // though v8 validated and stored its physical XMP owner. If
                // that alias disappears, recover the RAW spelling from the
                // physical owner plus the journaled RAW extension.
                match raw_path_from_sidecar_owner(&current_owner, path) {
                    Ok(recovered_path) => {
                        if !raw_identity_matches(&recovered_path, size, mtime_ns) {
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
                        publication_path = recovered_path;
                        current_owner.clone()
                    }
                    Err(_) => {
                        transaction.rollback()?;
                        return Err(PendingSidecarSyncError::SidecarOwner(primary_error));
                    }
                }
            }
        };
        if publication_owner != current_owner {
            transaction.execute(
                "INSERT OR REPLACE INTO quarantined_legacy_ratings
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                     last_seen, quarantined_at)
                 SELECT path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                        last_seen, unixepoch()
                   FROM images
                  WHERE path = ?1
                    AND size = ?2
                    AND mtime_ns = ?3
                    AND rating = ?4
                    AND sidecar_dirty = 1
                    AND sidecar_quarantined = 0",
                rusqlite::params![path_value(path), size, mtime_ns, rating],
            )?;
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
            return Ok(PendingSidecarSync::OwnerChanged);
        }

        let sidecar_mtime_ns = match write(&publication_path) {
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
        match self.read_schema {
            RatingReadSchema::Owner => self
                .pending_sidecars_with_owners()
                .map(|rows| rows.into_iter().map(|row| row.pending).collect::<Vec<_>>()),
            RatingReadSchema::LegacyOwner | RatingReadSchema::LegacyDirty => {
                Err(rusqlite::Error::InvalidQuery.into())
            }
        }
    }

    pub(crate) fn pending_sidecars_with_owners(&self) -> Result<Vec<OwnedPendingSidecar>, DbError> {
        match self.read_schema {
            RatingReadSchema::Owner => pending_sidecars_on(&self.conn).map_err(Into::into),
            RatingReadSchema::LegacyOwner | RatingReadSchema::LegacyDirty => {
                Err(rusqlite::Error::InvalidQuery.into())
            }
        }
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
            sidecar_owner, owner_key_version, last_seen)
         VALUES (?1, ?2, ?3, ?4, 0, 1, ?5, 8, unixepoch())
         ON CONFLICT(path) DO UPDATE SET
           size = excluded.size,
           mtime_ns = excluded.mtime_ns,
           rating = excluded.rating,
           sidecar_dirty = 1,
           sidecar_quarantined = 0,
           sidecar_owner = COALESCE(excluded.sidecar_owner, images.sidecar_owner),
           owner_key_version = MAX(images.owner_key_version + 1, 8),
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
    let global_revision = advance_global_revision(conn)?;
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

fn advance_global_revision(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "UPDATE rating_global_revision
            SET revision = revision + 1
          WHERE singleton = 1
        RETURNING revision",
        [],
        |row| row.get::<_, i64>(0),
    )
}

fn advance_ownerless_revision(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "UPDATE rating_global_revision
            SET revision = revision + 1,
                ownerless_revision = ownerless_revision + 1
          WHERE singleton = 1
        RETURNING revision",
        [],
        |row| row.get::<_, i64>(0),
    )
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

fn rating_global_revisions_on(conn: &Connection) -> Result<(i64, i64), rusqlite::Error> {
    conn.query_row(
        "SELECT revision, ownerless_revision
           FROM rating_global_revision
          WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

fn pending_sidecars_on(conn: &Connection) -> Result<Vec<OwnedPendingSidecar>, rusqlite::Error> {
    let mut statement = conn.prepare(PENDING_SIDECARS_QUERY)?;
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
    let rows = rows.collect::<Result<Vec<_>, _>>()?;
    let mut owners = HashSet::with_capacity(rows.len());
    if rows.iter().any(|row| !owners.insert(row.owner.clone())) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(rows)
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

fn query_legacy_rating_rows(
    conn: &Connection,
    keys: &[PathBuf],
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    if keys.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", keys.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty
           FROM images
          WHERE path IN ({placeholders})"
    );
    let mut statement = conn.prepare(&sql)?;
    let mapped = statement.query_map(
        rusqlite::params_from_iter(keys.iter().map(|path| path_value(path))),
        legacy_rating_row,
    )?;
    for row in mapped {
        let row = row?;
        rows.insert(row.path.clone(), row);
    }
    Ok(())
}

fn query_all_dirty_legacy_rating_rows(
    conn: &Connection,
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty
           FROM images
          WHERE sidecar_dirty = 1",
    )?;
    let mapped = statement.query_map([], legacy_rating_row)?;
    for row in mapped {
        let row = row?;
        rows.insert(row.path.clone(), row);
    }
    Ok(())
}

fn query_all_legacy_rating_rows(
    conn: &Connection,
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty
           FROM images",
    )?;
    let mapped = statement.query_map([], legacy_rating_row)?;
    for row in mapped {
        let row = row?;
        rows.insert(row.path.clone(), row);
    }
    Ok(())
}

fn query_legacy_clean_alias_paths(
    conn: &Connection,
    read_schema: RatingReadSchema,
    relevant_tokens: &HashSet<OsString>,
) -> Result<Vec<PathBuf>, rusqlite::Error> {
    let sql = match read_schema {
        RatingReadSchema::LegacyDirty => {
            "SELECT path
               FROM images
              WHERE sidecar_dirty = 0"
        }
        RatingReadSchema::LegacyOwner => {
            "SELECT path
               FROM images
              WHERE sidecar_dirty = 0
                AND sidecar_quarantined = 0"
        }
        RatingReadSchema::Owner => return Err(rusqlite::Error::InvalidQuery),
    };
    let mut statement = conn.prepare(sql)?;
    let paths = statement.query_map([], |row| row_path(row, 0))?;
    let mut candidates = Vec::new();
    for path in paths {
        let path = path?;
        if sidecar_owner_collision_token(&path)
            .is_some_and(|token| relevant_tokens.contains(&token))
        {
            candidates.push(path);
        }
    }
    Ok(candidates)
}

fn legacy_rating_row(row: &Row<'_>) -> Result<StoredRatingRow, rusqlite::Error> {
    Ok(StoredRatingRow {
        path: row_path(row, 0)?,
        size: row.get(1)?,
        mtime_ns: row.get(2)?,
        rating: row.get(3)?,
        sidecar_mtime_ns: row.get(4)?,
        sidecar_dirty: row.get(5)?,
        sidecar_owner: None,
    })
}

fn query_all_dirty_owner_rating_rows(
    conn: &Connection,
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty, sidecar_owner
           FROM images
          WHERE sidecar_dirty = 1
            AND sidecar_quarantined = 0",
    )?;
    let mapped = statement.query_map([], |row| {
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
    })?;
    for row in mapped {
        let row = row?;
        rows.insert(row.path.clone(), row);
    }
    Ok(())
}

fn query_all_legacy_owner_rating_rows(
    conn: &Connection,
    rows: &mut HashMap<PathBuf, StoredRatingRow>,
) -> Result<(), rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, size, mtime_ns, rating, sidecar_mtime_ns,
                sidecar_dirty, sidecar_owner
           FROM images
          WHERE sidecar_quarantined = 0",
    )?;
    let mapped = statement.query_map([], |row| {
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
    })?;
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
            sidecar_quarantined, sidecar_owner, owner_key_version, last_seen)
         VALUES (?1, ?2, ?3, ?4, 1, 0, 0, ?5, 8, unixepoch())
         ON CONFLICT(path) DO UPDATE SET
           size = excluded.size,
           mtime_ns = excluded.mtime_ns,
           rating = excluded.rating,
           sidecar_mtime_ns = excluded.sidecar_mtime_ns,
           sidecar_dirty = 0,
           sidecar_quarantined = 0,
           sidecar_owner = excluded.sidecar_owner,
           owner_key_version = MAX(images.owner_key_version + 1, 8),
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

/// Runs the production pending-sidecar query and returns its result count.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_pending_sidecars(db: &Db) -> Result<usize, DbError> {
    db.pending_sidecars().map(|pending| pending.len())
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

fn resolved_sidecar_owner(path: &Path) -> Result<PathBuf, DbError> {
    sidecar_owner_key(path)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)).into())
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
        unsafe_alias_history: bool,
        owner: Option<PathBuf>,
    }

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
                    unsafe_alias_history: false,
                    owner: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut rows = rows;
    for row in &mut rows {
        row.owner = sidecar_owner_key(&row.path).ok();
        row.unsafe_alias_history =
            normalize_physical_path(&row.path) != row.path || (row.dirty && row.owner.is_none());
    }
    let mut unsafe_history_tokens = rows
        .iter()
        // A clean unresolved row cannot write during recovery, so it may
        // remain useful as offline fallback state. It must still poison dirty
        // same-name candidates: the missing spelling may have been an alias
        // whose later completed write superseded their unfinished work.
        .filter(|row| row.unsafe_alias_history || row.owner.is_none())
        .filter_map(|row| sidecar_owner_collision_token(&row.path))
        .collect::<HashSet<_>>();
    let dirty_ownerless_tokens = rows
        .iter()
        .filter(|row| row.dirty)
        .filter_map(|row| sidecar_owner_collision_token(&row.path))
        .collect::<HashSet<_>>();
    if !dirty_ownerless_tokens.is_empty() {
        let mut statement = conn.prepare(
            "SELECT path
               FROM images
              WHERE sidecar_owner IS NOT NULL
                AND owner_key_version < 8",
        )?;
        let histories = statement
            .query_map([], |row| row_path(row, 0))?
            .collect::<Result<Vec<_>, _>>()?;
        for path in histories {
            let Some(token) = sidecar_owner_collision_token(&path) else {
                continue;
            };
            if dirty_ownerless_tokens.contains(&token)
                && (normalize_physical_path(&path) != path || sidecar_owner_key(&path).is_err())
            {
                unsafe_history_tokens.insert(token);
            }
        }
    }

    // A partially migrated v6/v7 database can contain ownerless and owned
    // rows together. Resolve their ambiguity in one transaction: otherwise
    // deleting an unsafe ownerless alias here would hide its history from the
    // subsequent owner-key migration and allow an unordered owned sibling to
    // publish.
    let ambiguous_owned = if unsafe_history_tokens.is_empty() {
        Vec::new()
    } else {
        let mut statement = conn.prepare(
            "SELECT path, sidecar_owner, sidecar_dirty
               FROM images
              WHERE sidecar_owner IS NOT NULL
                AND owner_key_version < 8",
        )?;
        statement
            .query_map([], |row| {
                Ok((row_path(row, 0)?, row_path(row, 1)?, row.get::<_, bool>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(path, _, _)| {
                sidecar_owner_collision_token(path)
                    .is_some_and(|token| unsafe_history_tokens.contains(&token))
            })
            .collect()
    };
    let mut quarantined = 0;
    for (path, owner, dirty) in ambiguous_owned {
        if dirty {
            conn.execute(
                "INSERT OR REPLACE INTO quarantined_legacy_ratings
                    (path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                     last_seen, quarantined_at)
                 SELECT path, size, mtime_ns, rating, sidecar_mtime_ns, revision,
                        last_seen, unixepoch()
                   FROM images
                  WHERE path = ?1
                    AND sidecar_owner = ?2
                    AND sidecar_dirty = 1
                    AND owner_key_version < 8",
                rusqlite::params![path_value(&path), path_value(&owner)],
            )?;
        }
        let changed = conn.execute(
            "DELETE FROM images
              WHERE path = ?1
                AND sidecar_owner = ?2
                AND owner_key_version < 8",
            rusqlite::params![path_value(&path), path_value(&owner)],
        )?;
        if changed == 1 {
            advance_sidecar_owner_revision(conn, &owner)?;
            quarantined += usize::from(dirty);
        }
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
    let mut by_owner: HashMap<PathBuf, Vec<LegacyDirty>> = HashMap::new();
    let mut quarantine = Vec::new();
    let mut discard = Vec::new();
    for row in rows {
        // Ownerless formats recorded only a path spelling, not the physical
        // publication owner observed when the rating was accepted. A path
        // that is now unresolved or resolves through an alias could have
        // referred to any same-name physical RAW before it was removed or
        // retargeted. Without a durable ordering key, publishing another
        // legacy row with that collision token could overwrite newer intent.
        let shares_unsafe_history = row.unsafe_alias_history
            || (row.dirty
                && sidecar_owner_collision_token(&row.path)
                    .is_some_and(|token| unsafe_history_tokens.contains(&token)));
        if shares_unsafe_history {
            if row.dirty {
                quarantine.push(row.path);
            } else {
                discard.push(row.path);
            }
            continue;
        }
        let Some(owner) = row.owner.clone() else {
            if row.dirty {
                quarantine.push(row.path);
            }
            // A clean row is historical fallback state, not unfinished
            // publication. Preserve it when the filesystem is merely
            // offline; it cannot write a sidecar on startup.
            continue;
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
                SET sidecar_owner = ?2,
                    owner_key_version = MAX(owner_key_version + 1, 8)
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
    for path in discard {
        conn.execute(
            "DELETE FROM images
              WHERE path = ?1
                AND sidecar_dirty = 0
                AND sidecar_owner IS NULL",
            [path_value(&path)],
        )?;
    }
    Ok((recovered, quarantined))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredOwnerKeyRow {
    owner: PathBuf,
    size: u64,
    mtime_ns: i64,
    rating: Option<u8>,
    sidecar_mtime_ns: i64,
    sidecar_dirty: bool,
    sidecar_quarantined: bool,
    revision: i64,
    last_seen: i64,
    owner_key_version: i64,
}

#[derive(Debug, Clone)]
struct PlannedOwnerKeyRow {
    path: PathBuf,
    stored: StoredOwnerKeyRow,
    owner: Option<PathBuf>,
    identity_matches: bool,
    path_is_physical: bool,
}

fn read_stored_owner_key_rows(
    conn: &Connection,
) -> Result<HashMap<PathBuf, StoredOwnerKeyRow>, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT path, sidecar_owner, size, mtime_ns, rating,
                sidecar_mtime_ns, sidecar_dirty, sidecar_quarantined,
                revision, last_seen, owner_key_version
           FROM images
          WHERE sidecar_owner IS NOT NULL",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row_path(row, 0)?,
                StoredOwnerKeyRow {
                    owner: row_path(row, 1)?,
                    size: row.get(2)?,
                    mtime_ns: row.get(3)?,
                    rating: row.get(4)?,
                    sidecar_mtime_ns: row.get(5)?,
                    sidecar_dirty: row.get(6)?,
                    sidecar_quarantined: row.get(7)?,
                    revision: row.get(8)?,
                    last_seen: row.get(9)?,
                    owner_key_version: row.get(10)?,
                },
            ))
        })?
        .collect()
}

fn plan_owner_key_rows(stored: &HashMap<PathBuf, StoredOwnerKeyRow>) -> Vec<PlannedOwnerKeyRow> {
    stored
        .iter()
        .map(|(path, stored)| PlannedOwnerKeyRow {
            path: path.clone(),
            stored: stored.clone(),
            owner: sidecar_owner_key(path).ok(),
            identity_matches: raw_identity_matches(path, stored.size, stored.mtime_ns),
            path_is_physical: normalize_physical_path(path) == *path,
        })
        .collect()
}

fn raw_identity_matches(path: &Path, size: u64, mtime_ns: i64) -> bool {
    std::fs::metadata(path).ok().is_some_and(|metadata| {
        let current_mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64)
            .unwrap_or(0);
        metadata.len() == size && current_mtime_ns == mtime_ns
    })
}

fn ensure_owner_key_version_column(conn: &Connection) -> Result<(), rusqlite::Error> {
    if has_column(conn, "images", "owner_key_version")? {
        return Ok(());
    }
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_column(&transaction, "images", "owner_key_version")? {
        transaction.execute(
            "ALTER TABLE images
             ADD COLUMN owner_key_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    transaction.commit()
}

fn ensure_ownerless_revision_column(conn: &Connection) -> Result<(), rusqlite::Error> {
    if has_column(conn, "rating_global_revision", "ownerless_revision")? {
        return Ok(());
    }
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_column(&transaction, "rating_global_revision", "ownerless_revision")? {
        transaction.execute(
            "ALTER TABLE rating_global_revision
             ADD COLUMN ownerless_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    transaction.commit()
}

fn install_owner_key_fence(conn: &Connection) -> Result<(), rusqlite::Error> {
    for (name, sql) in [
        (
            "images_reject_legacy_owner_insert",
            LEGACY_OWNER_INSERT_FENCE_SQL,
        ),
        (
            "images_reject_legacy_rating_update",
            LEGACY_OWNER_UPDATE_FENCE_SQL,
        ),
    ] {
        if !schema_object_sql_is(conn, "trigger", name, sql)?
            || schema_object_exists(conn, "index", name)?
        {
            conn.execute_batch(&format!(
                "DROP INDEX IF EXISTS {name};
                 DROP TRIGGER IF EXISTS {name};
                 {sql};"
            ))?;
        }
    }
    Ok(())
}

fn install_rating_generation_objects(conn: &Connection) -> Result<(), rusqlite::Error> {
    for (object_type, name, sql) in [
        (
            "index",
            "images_pending_sidecars",
            PENDING_SIDECAR_INDEX_SQL,
        ),
        (
            "trigger",
            "images_reject_unowned_dirty_insert",
            UNOWNED_DIRTY_INSERT_FENCE_SQL,
        ),
        (
            "trigger",
            "images_reject_unowned_dirty_update",
            UNOWNED_DIRTY_UPDATE_FENCE_SQL,
        ),
        (
            "trigger",
            "images_revision_after_insert",
            IMAGE_REVISION_INSERT_SQL,
        ),
        (
            "trigger",
            "images_generation_after_update_v2",
            IMAGE_GENERATION_UPDATE_SQL,
        ),
        (
            "trigger",
            "images_revision_after_delete",
            IMAGE_REVISION_DELETE_SQL,
        ),
        (
            "trigger",
            "sidecar_owner_v6_insert_order",
            V6_OWNER_INSERT_ORDER_SQL,
        ),
        (
            "trigger",
            "sidecar_owner_v6_update_order",
            V6_OWNER_UPDATE_ORDER_SQL,
        ),
    ] {
        let other_type = if object_type == "index" {
            "trigger"
        } else {
            "index"
        };
        if !schema_object_sql_is(conn, object_type, name, sql)?
            || schema_object_exists(conn, other_type, name)?
        {
            // SQLite permits an index and trigger to share a name. Remove
            // both namespaces so a canonical object cannot mask a hostile
            // opposite-type object during readiness checks or repair.
            conn.execute_batch(&format!(
                "DROP INDEX IF EXISTS {name};
                 DROP TRIGGER IF EXISTS {name};
                 {sql};"
            ))?;
        }
    }
    conn.execute_batch("DROP TRIGGER IF EXISTS images_revision_after_update;")?;
    Ok(())
}

fn install_sidecar_owner_index(conn: &Connection) -> Result<(), rusqlite::Error> {
    if !sidecar_owner_index_is_valid(conn)? {
        conn.execute_batch(&format!(
            "DROP TRIGGER IF EXISTS images_sidecar_owners;
             DROP INDEX IF EXISTS images_sidecar_owners;
             {SIDECAR_OWNER_INDEX_SQL};"
        ))?;
    }
    Ok(())
}

#[derive(Debug)]
struct ColumnShape {
    declared_type: String,
    not_null: bool,
    default: Option<String>,
    primary_key_position: i64,
}

fn table_column_shapes(
    conn: &Connection,
    table: &str,
) -> Result<HashMap<String, ColumnShape>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT name, type, \"notnull\", dflt_value, pk
           FROM pragma_table_info('{table}')"
    ))?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ColumnShape {
                    declared_type: row.get(1)?,
                    not_null: row.get(2)?,
                    default: row.get(3)?,
                    primary_key_position: row.get(4)?,
                },
            ))
        })?
        .collect()
}

fn column_names_include(shapes: &HashMap<String, ColumnShape>, required: &[&str]) -> bool {
    required.iter().all(|name| shapes.contains_key(*name))
}

fn primary_key_is(shapes: &HashMap<String, ColumnShape>, expected: &[&str]) -> bool {
    let mut actual = shapes
        .iter()
        .filter(|(_, shape)| shape.primary_key_position > 0)
        .map(|(name, shape)| (shape.primary_key_position, name.as_str()))
        .collect::<Vec<_>>();
    actual.sort_unstable_by_key(|(position, _)| *position);
    actual
        .iter()
        .map(|(_, name)| *name)
        .eq(expected.iter().copied())
}

fn integer_not_null_is(shapes: &HashMap<String, ColumnShape>, column: &str) -> bool {
    shapes
        .get(column)
        .is_some_and(|shape| shape.declared_type.eq_ignore_ascii_case("INTEGER") && shape.not_null)
}

fn rating_generation_table_shapes_are_ready(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let images = table_column_shapes(conn, "images")?;
    let image_revisions = table_column_shapes(conn, "image_revisions")?;
    let owner_revisions = table_column_shapes(conn, "sidecar_owner_revisions")?;
    let global_revision = table_column_shapes(conn, "rating_global_revision")?;
    let owner_key_is_valid = images.get("owner_key_version").is_some_and(|shape| {
        shape.declared_type.eq_ignore_ascii_case("INTEGER")
            && shape.not_null
            && shape.default.as_deref().map(normalize_column_default) == Some("0")
    });

    Ok(column_names_include(
        &images,
        &[
            "path",
            "size",
            "mtime_ns",
            "rating",
            "sidecar_mtime_ns",
            "sidecar_dirty",
            "sidecar_quarantined",
            "sidecar_owner",
            "owner_key_version",
            "revision",
            "last_seen",
        ],
    ) && primary_key_is(&images, &["path"])
        && owner_key_is_valid
        && integer_not_null_is(&images, "revision")
        && column_names_include(&image_revisions, &["path", "revision"])
        && primary_key_is(&image_revisions, &["path"])
        && integer_not_null_is(&image_revisions, "revision")
        && column_names_include(&owner_revisions, &["owner", "revision", "global_revision"])
        && primary_key_is(&owner_revisions, &["owner"])
        && integer_not_null_is(&owner_revisions, "revision")
        && integer_not_null_is(&owner_revisions, "global_revision")
        && column_names_include(
            &global_revision,
            &["singleton", "revision", "ownerless_revision"],
        )
        && primary_key_is(&global_revision, &["singleton"])
        && integer_not_null_is(&global_revision, "singleton")
        && integer_not_null_is(&global_revision, "revision")
        && integer_not_null_is(&global_revision, "ownerless_revision"))
}

fn current_owner_schema_is_ready(conn: &Connection) -> Result<bool, rusqlite::Error> {
    Ok(rating_generation_schema_is_ready(conn)?
        && migration_is_complete(conn, SIDECAR_OWNER_KEY_MIGRATION)?
        && !migration_is_complete(conn, SIDECAR_OWNER_REPAIR_REQUIRED)?
        && sidecar_owner_index_is_valid(conn)?
        && schema_object_sql_is(
            conn,
            "trigger",
            "images_reject_legacy_owner_insert",
            LEGACY_OWNER_INSERT_FENCE_SQL,
        )?
        && !schema_object_exists(conn, "index", "images_reject_legacy_owner_insert")?
        && schema_object_sql_is(
            conn,
            "trigger",
            "images_reject_legacy_rating_update",
            LEGACY_OWNER_UPDATE_FENCE_SQL,
        )?
        && !schema_object_exists(conn, "index", "images_reject_legacy_rating_update")?)
}

fn rating_generation_schema_is_ready(conn: &Connection) -> Result<bool, rusqlite::Error> {
    if !migration_is_complete(conn, RATING_GENERATION_MIGRATION)?
        || !rating_generation_table_shapes_are_ready(conn)?
    {
        return Ok(false);
    }
    let singleton_is_valid = conn.query_row(
        "SELECT COUNT(*) = 1
           AND MIN(singleton) = 1
           AND MAX(singleton) = 1
           AND MIN(revision) >= 0
           AND MIN(ownerless_revision) >= 0
           FROM rating_global_revision",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !singleton_is_valid {
        return Ok(false);
    }
    rating_generation_objects_are_ready(conn)
}

fn rating_generation_objects_are_ready(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT type, name, sql
           FROM sqlite_schema
          WHERE type IN ('index', 'trigger')",
    )?;
    let objects = statement
        .query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    for (object_type, name, sql) in [
        (
            "index",
            "images_pending_sidecars",
            PENDING_SIDECAR_INDEX_SQL,
        ),
        (
            "trigger",
            "images_reject_unowned_dirty_insert",
            UNOWNED_DIRTY_INSERT_FENCE_SQL,
        ),
        (
            "trigger",
            "images_reject_unowned_dirty_update",
            UNOWNED_DIRTY_UPDATE_FENCE_SQL,
        ),
        (
            "trigger",
            "images_revision_after_insert",
            IMAGE_REVISION_INSERT_SQL,
        ),
        (
            "trigger",
            "images_generation_after_update_v2",
            IMAGE_GENERATION_UPDATE_SQL,
        ),
        (
            "trigger",
            "images_revision_after_delete",
            IMAGE_REVISION_DELETE_SQL,
        ),
        (
            "trigger",
            "sidecar_owner_v6_insert_order",
            V6_OWNER_INSERT_ORDER_SQL,
        ),
        (
            "trigger",
            "sidecar_owner_v6_update_order",
            V6_OWNER_UPDATE_ORDER_SQL,
        ),
    ] {
        let Some(actual_sql) = objects.get(&(object_type.to_owned(), name.to_owned())) else {
            return Ok(false);
        };
        if actual_sql
            .as_deref()
            .is_none_or(|actual| normalize_schema_sql(actual) != normalize_schema_sql(sql))
            || objects
                .keys()
                .any(|(actual_type, actual_name)| actual_name == name && actual_type != object_type)
        {
            return Ok(false);
        }
    }
    Ok(!objects.contains_key(&(
        "trigger".to_owned(),
        "images_revision_after_update".to_owned(),
    )))
}

fn rating_table_key_shapes_are_safe(conn: &Connection) -> Result<bool, rusqlite::Error> {
    for (table, required, key) in [
        (
            "images",
            &["path", "size", "mtime_ns", "rating"][..],
            &["path"][..],
        ),
        ("image_revisions", &["path", "revision"][..], &["path"][..]),
        (
            "sidecar_owner_revisions",
            &["owner", "revision"][..],
            &["owner"][..],
        ),
        (
            "rating_global_revision",
            &["singleton", "revision"][..],
            &["singleton"][..],
        ),
    ] {
        if schema_object_exists(conn, "table", table)?
            && (!has_columns(conn, table, required)? || primary_key_columns(conn, table)? != key)
        {
            return Ok(false);
        }
    }
    if schema_object_exists(conn, "table", "images")?
        && has_column(conn, "images", "owner_key_version")?
        && !column_definition_is(
            conn,
            "images",
            "owner_key_version",
            "INTEGER",
            true,
            Some("0"),
        )?
    {
        return Ok(false);
    }
    rating_revision_integer_columns_are_valid(conn, false)
}

fn detect_read_schema(conn: &Connection) -> Result<Option<RatingReadSchema>, rusqlite::Error> {
    if migration_is_complete(conn, SIDECAR_OWNER_REPAIR_REQUIRED)? {
        // A previous opener deliberately invalidated the owner capability and
        // then exited before repair completed. Do not expose possibly stale
        // ownership through the latency-sensitive reader.
        return Ok(None);
    }
    let current_owner_marker = migration_is_complete(conn, SIDECAR_OWNER_KEY_MIGRATION)?;
    let legacy_owner_marker = migration_is_complete(conn, RATING_GENERATION_MIGRATION)?
        || migration_is_complete(conn, RATING_READ_COMPATIBILITY_MIGRATION)?;
    let owner_shape = has_columns(
        conn,
        "images",
        &[
            "path",
            "size",
            "mtime_ns",
            "rating",
            "sidecar_mtime_ns",
            "sidecar_dirty",
            "sidecar_quarantined",
            "sidecar_owner",
        ],
    )?;
    let legacy_shape = has_columns(
        conn,
        "images",
        &[
            "path",
            "size",
            "mtime_ns",
            "rating",
            "sidecar_mtime_ns",
            "sidecar_dirty",
        ],
    )? && !has_column(conn, "images", "sidecar_quarantined")?
        && !has_column(conn, "images", "sidecar_owner")?;
    let legacy_key_shapes_are_safe =
        if !current_owner_marker && (legacy_owner_marker || legacy_shape) {
            rating_table_key_shapes_are_safe(conn)?
        } else {
            false
        };
    let owner_index = owner_shape && sidecar_owner_index_is_valid(conn)?;

    if current_owner_marker
        && owner_index
        && has_column(conn, "images", "owner_key_version")?
        && current_owner_schema_is_ready(conn)?
    {
        Ok(Some(RatingReadSchema::Owner))
    } else if legacy_owner_marker
        && !current_owner_marker
        && owner_index
        && legacy_key_shapes_are_safe
    {
        Ok(Some(RatingReadSchema::LegacyOwner))
    } else if !current_owner_marker
        && !legacy_owner_marker
        && legacy_shape
        && legacy_key_shapes_are_safe
    {
        Ok(Some(RatingReadSchema::LegacyDirty))
    } else {
        Ok(None)
    }
}

fn sidecar_owner_index_is_valid(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let metadata = conn
        .query_row(
            "SELECT \"unique\", partial
               FROM pragma_index_list('images')
              WHERE name = 'images_sidecar_owners'",
            [],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?;
    if metadata != Some((true, true)) {
        return Ok(false);
    }
    let mut statement =
        conn.prepare("SELECT name FROM pragma_index_info('images_sidecar_owners') ORDER BY seqno")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns == ["sidecar_owner"]
        && !schema_object_exists(conn, "trigger", "images_sidecar_owners")?
        && schema_object_sql_is(
            conn,
            "index",
            "images_sidecar_owners",
            SIDECAR_OWNER_INDEX_SQL,
        )?)
}

fn schema_object_sql_is(
    conn: &Connection,
    object_type: &str,
    name: &str,
    expected: &str,
) -> Result<bool, rusqlite::Error> {
    let sql = conn
        .query_row(
            "SELECT sql
               FROM sqlite_schema
              WHERE type = ?1
                AND name = ?2",
            [object_type, name],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(sql.is_some_and(|sql| normalize_schema_sql(&sql) == normalize_schema_sql(expected)))
}

fn schema_object_exists(
    conn: &Connection,
    object_type: &str,
    name: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM sqlite_schema
              WHERE type = ?1
                AND name = ?2
         )",
        [object_type, name],
        |row| row.get(0),
    )
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn migrate_sidecar_owner_keys(
    conn: &Connection,
    force_repair_from_initial_state: bool,
) -> Result<(), DbError> {
    let force_repair = force_repair_from_initial_state
        || migration_is_complete(conn, SIDECAR_OWNER_REPAIR_REQUIRED)?
        || (migration_is_complete(conn, SIDECAR_OWNER_KEY_MIGRATION)?
            && !current_owner_schema_is_ready(conn)?);
    ensure_owner_key_version_column(conn)?;
    ensure_ownerless_revision_column(conn)?;
    // Stop every legacy rating writer before filesystem planning, including
    // released writers whose SQL omits `sidecar_owner` entirely. Current
    // completion updates do not touch the fenced columns and can still drain.
    // Without this early fence, an active legacy writer could invalidate every
    // optimistic snapshot or bypass the ownerless retry epoch. Processes from
    // before the journal gate must not coexist with v8: no database trigger
    // can prevent such a process from writing XMP after its SQL is rejected.
    let fence_transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    install_owner_key_fence(&fence_transaction)?;
    fence_transaction.commit()?;

    loop {
        let (global_revision, stored) = {
            let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
            let global_revision = rating_global_revisions_on(&transaction)?.0;
            let stored = read_stored_owner_key_rows(&transaction)?;
            transaction.commit()?;
            (global_revision, stored)
        };
        let planned = plan_owner_key_rows(&stored);
        let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        if current_owner_schema_is_ready(&transaction)? && !force_repair {
            transaction.rollback()?;
            return Ok(());
        }
        if rating_global_revisions_on(&transaction)?.0 != global_revision
            || read_stored_owner_key_rows(&transaction)? != stored
        {
            transaction.rollback()?;
            continue;
        }
        // A marker-present repair can encounter same-name triggers whose
        // bodies are too permissive or too strict. Remove them only after the
        // optimistic snapshot is validated, then reinstall the canonical
        // fences in this same transaction after every row is current.
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS images_reject_legacy_owner_insert;
             DROP TRIGGER IF EXISTS images_reject_legacy_rating_update;",
        )?;

        let unsafe_history_tokens = planned
            .iter()
            .filter(|row| {
                row.stored.owner_key_version < CURRENT_OWNER_KEY_VERSION
                    && (!row.path_is_physical || row.owner.is_none())
            })
            .filter_map(|row| sidecar_owner_collision_token(&row.path))
            .collect::<HashSet<_>>();
        let mut removals = Vec::new();
        let mut by_owner: HashMap<PathBuf, Vec<PlannedOwnerKeyRow>> = HashMap::new();
        for row in planned {
            let shares_unsafe_legacy_history = row.stored.owner_key_version
                < CURRENT_OWNER_KEY_VERSION
                && ((!row.path_is_physical || row.owner.is_none())
                    || sidecar_owner_collision_token(&row.path)
                        .is_some_and(|token| unsafe_history_tokens.contains(&token)));
            if row.stored.sidecar_quarantined
                || !row.identity_matches
                || row.owner.is_none()
                || (row.stored.sidecar_dirty && row.stored.rating.is_none())
                // Pre-v8 path spellings have no durable proof of their
                // historical physical owner. An ambiguous alias makes every
                // same-name legacy candidate unordered, so remove clean
                // fallbacks and quarantine unfinished work.
                || shares_unsafe_legacy_history
            {
                removals.push(row);
            } else {
                by_owner
                    .entry(row.owner.clone().expect("checked owner"))
                    .or_default()
                    .push(row);
            }
        }

        let mut retained = Vec::new();
        for (_, mut rows) in by_owner {
            if rows.len() == 1 {
                retained.push(rows.pop().expect("single owner-key row"));
            } else {
                removals.extend(rows);
            }
        }
        let mut touched_owners = HashSet::new();
        for row in &removals {
            touched_owners.insert(row.stored.owner.clone());
            if let Some(owner) = &row.owner {
                touched_owners.insert(owner.clone());
            }
        }
        for row in &retained {
            let owner = row.owner.as_ref().expect("retained owner");
            if &row.stored.owner != owner {
                touched_owners.insert(row.stored.owner.clone());
                touched_owners.insert(owner.clone());
            }
        }

        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS images_sidecar_owners;
             DROP INDEX IF EXISTS images_sidecar_owners;",
        )?;
        let barrier = if force_repair {
            let barrier = advance_ownerless_revision(&transaction)?;
            transaction.execute(
                "UPDATE sidecar_owner_revisions
                    SET revision = revision + 1,
                        global_revision = ?1",
                [barrier],
            )?;
            transaction.execute(
                "INSERT INTO sidecar_owner_revisions
                    (owner, revision, global_revision)
                 SELECT sidecar_owner, 1, ?1
                   FROM images
                  WHERE sidecar_owner IS NOT NULL
                 ON CONFLICT(owner) DO NOTHING",
                [barrier],
            )?;
            barrier
        } else {
            advance_global_revision(&transaction)?
        };
        for owner in touched_owners {
            transaction.execute(
                "INSERT INTO sidecar_owner_revisions
                    (owner, revision, global_revision)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT(owner) DO UPDATE SET
                    revision = sidecar_owner_revisions.revision + 1,
                    global_revision = excluded.global_revision",
                rusqlite::params![path_value(&owner), barrier],
            )?;
        }

        let mut quarantined = 0_usize;
        for row in &removals {
            if row.stored.sidecar_dirty {
                transaction.execute(
                    "INSERT OR REPLACE INTO quarantined_legacy_ratings
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         revision, last_seen, quarantined_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch())",
                    rusqlite::params![
                        path_value(&row.path),
                        row.stored.size,
                        row.stored.mtime_ns,
                        row.stored.rating,
                        row.stored.sidecar_mtime_ns,
                        row.stored.revision,
                        row.stored.last_seen,
                    ],
                )?;
                quarantined += 1;
            }
            let changed = delete_stored_owner_key_row(&transaction, row)?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed).into());
            }
        }

        for row in &retained {
            let owner = row.owner.as_deref().expect("retained owner");
            if row.stored.owner == owner {
                continue;
            }
            let changed = transaction.execute(
                "UPDATE images
                    SET sidecar_owner = ?12,
                        owner_key_version = MAX(owner_key_version + 1, ?13)
                  WHERE path = ?1
                    AND sidecar_owner = ?2
                    AND size = ?3
                    AND mtime_ns = ?4
                    AND rating IS ?5
                    AND sidecar_mtime_ns = ?6
                    AND sidecar_dirty = ?7
                    AND sidecar_quarantined = ?8
                    AND revision = ?9
                    AND last_seen = ?10
                    AND owner_key_version = ?11",
                rusqlite::params![
                    path_value(&row.path),
                    path_value(&row.stored.owner),
                    row.stored.size,
                    row.stored.mtime_ns,
                    row.stored.rating,
                    row.stored.sidecar_mtime_ns,
                    row.stored.sidecar_dirty,
                    row.stored.sidecar_quarantined,
                    row.stored.revision,
                    row.stored.last_seen,
                    row.stored.owner_key_version,
                    path_value(owner),
                    CURRENT_OWNER_KEY_VERSION,
                ],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::StatementChangedRows(changed).into());
            }
        }
        transaction.execute(
            "UPDATE images
                SET owner_key_version = ?1
              WHERE sidecar_owner IS NOT NULL
                AND owner_key_version < ?1",
            [CURRENT_OWNER_KEY_VERSION],
        )?;

        install_sidecar_owner_index(&transaction)?;
        install_owner_key_fence(&transaction)?;
        transaction.execute(
            "DELETE FROM viewr_schema_migrations WHERE name = ?1",
            [SIDECAR_OWNER_REPAIR_REQUIRED],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO viewr_schema_migrations (name) VALUES (?1)",
            [SIDECAR_OWNER_KEY_MIGRATION],
        )?;
        transaction.commit()?;

        if quarantined > 0 {
            eprintln!(
                "quarantined {quarantined} unfinished rating(s) whose legacy owner key was \
                 ambiguous or unverifiable; rate those photos again to publish a sidecar safely"
            );
        }
        return Ok(());
    }
}

fn delete_stored_owner_key_row(
    conn: &Connection,
    row: &PlannedOwnerKeyRow,
) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "DELETE FROM images
          WHERE path = ?1
            AND sidecar_owner = ?2
            AND size = ?3
            AND mtime_ns = ?4
            AND rating IS ?5
            AND sidecar_mtime_ns = ?6
            AND sidecar_dirty = ?7
            AND sidecar_quarantined = ?8
            AND revision = ?9
            AND last_seen = ?10
            AND owner_key_version = ?11",
        rusqlite::params![
            path_value(&row.path),
            path_value(&row.stored.owner),
            row.stored.size,
            row.stored.mtime_ns,
            row.stored.rating,
            row.stored.sidecar_mtime_ns,
            row.stored.sidecar_dirty,
            row.stored.sidecar_quarantined,
            row.stored.revision,
            row.stored.last_seen,
            row.stored.owner_key_version,
        ],
    )
}

#[cfg(test)]
fn invalidate_owner_key_marker(conn: &Connection) -> Result<(), rusqlite::Error> {
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    invalidate_owner_key_marker_on(&transaction)?;
    transaction.commit()
}

fn invalidate_owner_key_marker_on(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO viewr_schema_migrations (name) VALUES (?1)",
        [SIDECAR_OWNER_REPAIR_REQUIRED],
    )?;
    conn.execute(
        "DELETE FROM viewr_schema_migrations WHERE name = ?1",
        [SIDECAR_OWNER_KEY_MIGRATION],
    )?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<(), DbError> {
    let snapshot = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    let ready = current_owner_schema_is_ready(&snapshot)?;
    snapshot.commit()?;
    if ready {
        return Ok(());
    }

    // Capability checks span several SQLite catalog queries. Make each
    // repair decision from one immediate transaction so concurrent first
    // opens cannot combine pre- and post-migration observations. A waiter may
    // still have decided to repeat a forced repair; retry final verification
    // rather than treating its temporary sentinel as permanent corruption.
    for _ in 0..32 {
        let gate = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        if current_owner_schema_is_ready(&gate)? {
            gate.commit()?;
            return Ok(());
        }
        if !rating_revision_storage_values_are_valid(&gate)? {
            // Repair and migration perform counter arithmetic. SQLite's
            // INTEGER affinity does not prevent a manually damaged database
            // from storing TEXT or REAL values, whose numeric coercion could
            // recreate an old retry token.
            gate.rollback()?;
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let force_owner_repair = migration_is_complete(&gate, SIDECAR_OWNER_KEY_MIGRATION)?
            || migration_is_complete(&gate, SIDECAR_OWNER_REPAIR_REQUIRED)?;
        if force_owner_repair {
            // The marker becomes false in the same snapshot that selected
            // forced repair. A crash between phases therefore leaves durable
            // repair intent for the next opener.
            invalidate_owner_key_marker_on(&gate)?;
        }
        gate.commit()?;

        initialize_rating_generation_schema(conn)?;
        migrate_sidecar_owner_keys(conn, force_owner_repair)?;

        let verification = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        if current_owner_schema_is_ready(&verification)? {
            verification.commit()?;
            return Ok(());
        }
        verification.rollback()?;
    }
    Err(rusqlite::Error::InvalidQuery.into())
}

fn initialize_rating_generation_schema(conn: &Connection) -> Result<(), DbError> {
    if rating_generation_schema_is_ready(conn)? {
        return Ok(());
    }
    if !rating_table_key_shapes_are_safe(conn)? {
        return Err(rusqlite::Error::InvalidQuery.into());
    }
    let repairing_existing = migration_is_complete(conn, RATING_GENERATION_MIGRATION)?;

    // The capability check above keeps ordinary opens read-only. An immediate
    // transaction serializes first-time migration and marker-present repair;
    // the second check prevents a waiter from repeating either operation.
    let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if rating_generation_schema_is_ready(&transaction)? {
        transaction.rollback()?;
        return Ok(());
    }
    // Marker-present repair can encounter hostile same-name objects from
    // either SQLite namespace. Remove every owner-key object that can block
    // generation repair DML while this immediate transaction excludes other
    // writers; canonical fences are restored before commit.
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS images_sidecar_owners;
         DROP INDEX IF EXISTS images_reject_legacy_owner_insert;
         DROP TRIGGER IF EXISTS images_reject_legacy_owner_insert;
         DROP INDEX IF EXISTS images_reject_legacy_rating_update;
         DROP TRIGGER IF EXISTS images_reject_legacy_rating_update;",
    )?;

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
            owner_key_version INTEGER NOT NULL DEFAULT 0,
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
    if !has_column(&transaction, "images", "owner_key_version")? {
        transaction.execute(
            "ALTER TABLE images
             ADD COLUMN owner_key_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
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
            revision INTEGER NOT NULL,
            ownerless_revision INTEGER NOT NULL DEFAULT 0
        ) WITHOUT ROWID;",
    )?;
    if !has_column(&transaction, "sidecar_owner_revisions", "global_revision")? {
        transaction.execute(
            "ALTER TABLE sidecar_owner_revisions
             ADD COLUMN global_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_column(&transaction, "rating_global_revision", "ownerless_revision")? {
        transaction.execute(
            "ALTER TABLE rating_global_revision
             ADD COLUMN ownerless_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // A marker-present object can be actively hostile to the reconciliation
    // DML below. Remove every known generation trigger only after its target
    // tables exist, then recreate canonical definitions in this transaction
    // before another connection can write.
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS images_pending_sidecars;
         DROP TRIGGER IF EXISTS images_reject_unowned_dirty_insert;
         DROP TRIGGER IF EXISTS images_reject_unowned_dirty_update;
         DROP TRIGGER IF EXISTS images_revision_after_insert;
         DROP TRIGGER IF EXISTS images_generation_after_update_v2;
         DROP TRIGGER IF EXISTS images_revision_after_delete;
         DROP TRIGGER IF EXISTS images_revision_after_update;
         DROP TRIGGER IF EXISTS sidecar_owner_v6_insert_order;
         DROP TRIGGER IF EXISTS sidecar_owner_v6_update_order;
         DROP TRIGGER IF EXISTS images_reject_legacy_owner_insert;
         DROP TRIGGER IF EXISTS images_reject_legacy_rating_update;",
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
            revision INTEGER NOT NULL,
            ownerless_revision INTEGER NOT NULL DEFAULT 0
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

    install_rating_generation_objects(&transaction)?;
    let (recovered, quarantined) = migrate_legacy_dirty_ratings(&transaction)?;
    if repairing_existing {
        // A missing or incorrect ordering/generation object may already have
        // failed to record a mutation. Advance every retry domain before
        // declaring the repaired schema ready so a snapshot from before this
        // repair cannot publish stale state afterward.
        let barrier = advance_ownerless_revision(&transaction)?;
        transaction.execute(
            "INSERT INTO sidecar_owner_revisions
                (owner, revision, global_revision)
             SELECT sidecar_owner, 1, ?1
               FROM images
              WHERE sidecar_owner IS NOT NULL
             ON CONFLICT(owner) DO UPDATE SET
                revision = sidecar_owner_revisions.revision + 1,
                global_revision = excluded.global_revision",
            [barrier],
        )?;
    }
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
    install_rating_generation_objects(&transaction)?;
    install_owner_key_fence(&transaction)?;
    transaction.execute(
        "INSERT OR IGNORE INTO viewr_schema_migrations (name) VALUES (?1)",
        [RATING_GENERATION_MIGRATION],
    )?;
    transaction.commit()?;
    Ok(())
}

fn enable_wal(conn: &Connection, lock_timeout: std::time::Duration) -> Result<(), DbError> {
    let deadline = std::time::Instant::now() + lock_timeout;
    loop {
        match conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))
        {
            Ok(actual) if actual.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(actual) => return Err(DbError::WalUnavailable { actual }),
            Err(error) if database_is_locked(&error) && std::time::Instant::now() < deadline => {
                // SQLITE_LOCKED does not invoke SQLite's busy handler. A
                // short bounded retry covers simultaneous first opens while
                // retaining the connection-level handler for SQLITE_BUSY.
                std::thread::sleep(DATABASE_LOCK_RETRY);
            }
            Err(error) => return Err(error.into()),
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

fn has_columns(conn: &Connection, table: &str, required: &[&str]) -> Result<bool, rusqlite::Error> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(required.iter().all(|column| names.contains(*column)))
}

fn primary_key_columns(conn: &Connection, table: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!(
        "SELECT name
           FROM pragma_table_info('{table}')
          WHERE pk > 0
          ORDER BY pk"
    ))?;
    statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()
}

fn column_definition_is(
    conn: &Connection,
    table: &str,
    column: &str,
    declared_type: &str,
    not_null: bool,
    default: Option<&str>,
) -> Result<bool, rusqlite::Error> {
    let definition = conn
        .query_row(
            &format!(
                "SELECT type, \"notnull\", dflt_value
                   FROM pragma_table_info('{table}')
                  WHERE name = ?1"
            ),
            [column],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(
        definition.is_some_and(|(actual_type, actual_not_null, actual_default)| {
            actual_type.eq_ignore_ascii_case(declared_type)
                && actual_not_null == not_null
                && actual_default.as_deref().map(normalize_column_default)
                    == default.map(normalize_column_default)
        }),
    )
}

fn normalize_column_default(default: &str) -> &str {
    default
        .trim()
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or_else(|| default.trim())
}

fn column_is_not_null_integer(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT UPPER(TRIM(type)) = 'INTEGER' AND \"notnull\" = 1
               FROM pragma_table_info('{table}')
              WHERE name = ?1"
        ),
        [column],
        |row| row.get(0),
    )
    .optional()
    .map(|value| value.unwrap_or(false))
}

fn rating_revision_integer_columns_are_valid(
    conn: &Connection,
    require_current_columns: bool,
) -> Result<bool, rusqlite::Error> {
    for (table, column) in [
        ("images", "revision"),
        ("image_revisions", "revision"),
        ("sidecar_owner_revisions", "revision"),
        ("sidecar_owner_revisions", "global_revision"),
        ("rating_global_revision", "singleton"),
        ("rating_global_revision", "revision"),
        ("rating_global_revision", "ownerless_revision"),
    ] {
        if !schema_object_exists(conn, "table", table)? {
            if require_current_columns {
                return Ok(false);
            }
            continue;
        }
        if !has_column(conn, table, column)? {
            if require_current_columns {
                return Ok(false);
            }
            continue;
        }
        if !column_is_not_null_integer(conn, table, column)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn rating_revision_storage_values_are_valid(conn: &Connection) -> Result<bool, rusqlite::Error> {
    for (table, column) in [
        ("images", "revision"),
        ("image_revisions", "revision"),
        ("sidecar_owner_revisions", "revision"),
        ("sidecar_owner_revisions", "global_revision"),
        ("rating_global_revision", "singleton"),
        ("rating_global_revision", "revision"),
        ("rating_global_revision", "ownerless_revision"),
    ] {
        if !schema_object_exists(conn, "table", table)? || !has_column(conn, table, column)? {
            continue;
        }
        let valid = conn.query_row(
            &format!(
                "SELECT NOT EXISTS(
                     SELECT 1
                       FROM {table}
                      WHERE typeof({column}) <> 'integer'
                         OR {column} < 0
                         OR {column} >= 9223372036854775807
                 )"
            ),
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !valid {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_identity(path: &Path) -> (u64, i64) {
        let metadata = std::fs::metadata(path).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        (metadata.len(), mtime_ns)
    }

    #[cfg(unix)]
    fn write_equal_identity_raws(first: &Path, second: &Path) -> (u64, i64) {
        std::fs::write(first, b"raw").unwrap();
        std::fs::write(second, b"raw").unwrap();
        let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for path in [first, second] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(fixed)
                .unwrap();
        }
        let identity = file_identity(first);
        assert_eq!(file_identity(second), identity);
        identity
    }

    fn remove_v8_marker_and_fence(conn: &Connection) {
        conn.execute(
            "DELETE FROM viewr_schema_migrations WHERE name = ?1",
            [SIDECAR_OWNER_KEY_MIGRATION],
        )
        .unwrap();
        conn.execute_batch(
            "DROP TRIGGER images_reject_legacy_owner_insert;
             DROP TRIGGER images_reject_legacy_rating_update;",
        )
        .unwrap();
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn legacy_string_method_signatures_remain_compatible() {
        let _: fn(&Db, &str) -> Option<ImageRow> = Db::get_image;
        let _: fn(&Db, &str, u64, i64, Option<u8>, i64) -> Result<(), DbError> = Db::upsert_rating;
        let _: fn(&Db, &str, u64, i64, u8) -> Result<(), DbError> =
            Db::record_rating_pending_sidecar;
    }

    #[test]
    fn wal_enablement_fails_when_sqlite_declines_the_required_mode() {
        let connection = Connection::open_in_memory().unwrap();

        assert!(matches!(
            enable_wal(&connection, std::time::Duration::ZERO),
            Err(DbError::WalUnavailable { actual }) if actual.eq_ignore_ascii_case("memory")
        ));
    }

    #[test]
    fn public_clean_upsert_remains_available_for_an_unresolved_raw() {
        let db = Db::open_in_memory().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing").join("photo.ARW");
        let path_text = path.to_str().unwrap();

        db.upsert_rating(path_text, 10, 1, Some(4), 99).unwrap();

        let row = db.get_image(path_text).unwrap();
        assert_eq!(row.rating, Some(4));
        assert_eq!(row.sidecar_mtime_ns, 99);
        assert!(!row.sidecar_dirty);
        assert!(
            db.conn
                .query_row(
                    "SELECT sidecar_owner IS NULL
                      FROM images
                      WHERE path = ?1",
                    [path_value(&path)],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
    }

    #[test]
    fn public_dirty_journal_rejects_an_unresolved_sidecar_owner() {
        let db = Db::open_in_memory().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing").join("photo.ARW");

        assert!(matches!(
            db.record_rating_pending_sidecar(path.to_str().unwrap(), 10, 1, 4),
            Err(DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(_)))
        ));
        assert!(db.pending_sidecars().unwrap().is_empty());
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
    fn migration_discards_a_clean_legacy_symlink_spelling() {
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

        assert!(db.get_image(legacy_raw.to_str().unwrap()).is_none());
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
            db.record_rating_pending_sidecar_if_unchanged(
                &path,
                10,
                1,
                2,
                initially_missing.clone(),
            )
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
    fn owner_snapshot_uses_one_sqlite_read_view() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("snapshot-read-view.db");
        let first_raw = directory.path().join("photo.ARW");
        let sibling_raw = directory.path().join("photo.DNG");
        std::fs::write(&first_raw, b"arw").unwrap();
        std::fs::write(&sibling_raw, b"dng").unwrap();
        let first = Db::open(&database_path).unwrap();
        let second = Db::open(&database_path).unwrap();

        let predecessor = first
            .rating_owner_snapshot_with_hook(&first_raw, || {
                second
                    .record_rating_pending_sidecar_canonical(&sibling_raw, 3, 1, 5)
                    .unwrap();
            })
            .unwrap();

        assert_eq!(
            predecessor.image,
            ImageRevisionSnapshot::Missing { revision: 0 }
        );
        assert_eq!(predecessor.owner_revision, 0);
        assert!(
            !first
                .record_rating_pending_sidecar_if_unchanged(&first_raw, 3, 1, 2, predecessor,)
                .unwrap(),
            "the sibling write committed after the snapshot and must win"
        );
    }

    #[test]
    fn owner_retry_rejects_a_different_equal_revision_owner() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.ARW");
        let second = directory.path().join("second.ARW");
        std::fs::write(&first, b"raw").unwrap();
        std::fs::write(&second, b"raw").unwrap();
        let db = Db::open_in_memory().unwrap();
        let predecessor = db.rating_owner_snapshot(&first).unwrap();
        let second_owner = sidecar_owner_key(&second).unwrap();
        assert_ne!(predecessor.owner, second_owner);
        assert_eq!(predecessor.owner_revision, 0);
        assert_eq!(
            sidecar_owner_revision_on(&db.conn, &second_owner).unwrap(),
            0
        );

        assert!(
            !db.record_rating_pending_sidecar_if_unchanged(&second, 3, 1, 5, predecessor,)
                .unwrap(),
            "equal numeric counters cannot make a different owner equivalent"
        );
        assert!(db.get_image_path(&second).is_none());
    }

    #[test]
    fn global_retry_observes_an_intervening_ownerless_path_update() {
        let directory = tempfile::tempdir().unwrap();
        let missing_parent = directory.path().join("later");
        let raw = missing_parent.join("photo.ARW");
        let db = Db::open_in_memory().unwrap();
        let predecessor = db.rating_global_snapshot(&raw).unwrap();

        db.upsert_rating(raw.to_str().unwrap(), 3, 1, Some(5), 99)
            .unwrap();
        std::fs::create_dir(&missing_parent).unwrap();
        std::fs::write(&raw, b"raw").unwrap();

        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(&raw, 3, 1, 2, predecessor,)
                .unwrap(),
            "the later ownerless rating mutation must supersede the queued retry"
        );
        assert_eq!(db.get_image(raw.to_str().unwrap()).unwrap().rating, Some(5));
    }

    #[test]
    fn global_retry_observes_an_intervening_ownerless_sibling_update() {
        let directory = tempfile::tempdir().unwrap();
        let missing_parent = directory.path().join("later");
        let arw = missing_parent.join("photo.ARW");
        let dng = missing_parent.join("photo.DNG");
        let db = Db::open_in_memory().unwrap();
        let predecessor = db.rating_global_snapshot(&arw).unwrap();

        db.upsert_rating(dng.to_str().unwrap(), 3, 1, Some(5), 99)
            .unwrap();
        assert_eq!(
            rating_revision_snapshot_on(&db.conn, &arw).unwrap(),
            predecessor.image,
            "the per-image guard must remain unchanged so this exercises the ownerless epoch"
        );
        std::fs::create_dir(&missing_parent).unwrap();
        std::fs::write(&arw, b"raw").unwrap();
        std::fs::write(&dng, b"raw").unwrap();
        assert_eq!(
            sidecar_owner_key(&arw).unwrap(),
            sidecar_owner_key(&dng).unwrap()
        );

        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(&arw, 3, 1, 2, predecessor,)
                .unwrap(),
            "a newer unresolved sibling rating must supersede the queued retry"
        );
        assert!(db.get_image_path(&arw).is_none());
        assert_eq!(db.get_image(dng.to_str().unwrap()).unwrap().rating, Some(5));
    }

    #[test]
    fn rating_generation_advances_only_for_rating_ownership_changes() {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let path = directory.path().join("revisions.arw");
        std::fs::write(&path, b"raw").unwrap();
        let path = normalize_physical_path(&path);

        db.upsert_rating_path(&path, 10, 1, Some(1), 10).unwrap();
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 1 }
        );
        db.upsert_rating_path(&path, 10, 1, Some(2), 11).unwrap();
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 2 }
        );
        db.record_rating_pending_sidecar_path(&path, 10, 1, 3)
            .unwrap();
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 3 }
        );
        assert!(db.complete_pending_sidecar(&path, 10, 1, 3, 12).unwrap());
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 3 }
        );
        db.record_rating_pending_sidecar_path(&path, 10, 1, 4)
            .unwrap();
        assert!(matches!(
            db.synchronize_pending_sidecar(&path, 10, 1, 4, |_| {
                PendingSidecarWrite::<()>::Written(13)
            })
            .unwrap(),
            PendingSidecarSync::Written
        ));
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
            ImageRevisionSnapshot::Present { revision: 4 }
        );
        db.record_rating_pending_sidecar_path(&path, 10, 1, 5)
            .unwrap();
        assert!(db.discard_pending_sidecar(&path, 10, 1, 5).unwrap());
        assert_eq!(
            db.rating_revision_snapshot(&path).unwrap(),
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
        let path = directory.path().join("concurrent.arw");
        std::fs::write(&path, b"raw").unwrap();
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
                .synchronize_pending_sidecar(&owner_path, 10, 1, 2, |_| {
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
                .synchronize_pending_sidecar(&path, 10, 1, 5, |_| {
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
        let path = directory.path().join("discard-race.arw");
        std::fs::write(&path, b"raw").unwrap();
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
                .synchronize_pending_sidecar(&owner_path, 10, 1, 4, |_| {
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
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let path = directory.path().join("retry.arw");
        std::fs::write(&path, b"raw").unwrap();
        db.record_rating_pending_sidecar_path(&path, 10, 1, 4)
            .unwrap();

        let result = db
            .synchronize_pending_sidecar(&path, 10, 1, 4, |_| {
                PendingSidecarWrite::Failed("injected sidecar failure")
            })
            .unwrap();

        assert!(matches!(
            result,
            PendingSidecarSync::WriteFailed("injected sidecar failure")
        ));
        assert!(db.get_image_path(&path).unwrap().sidecar_dirty);
        assert!(matches!(
            db.synchronize_pending_sidecar(&path, 10, 1, 4, |_| {
                PendingSidecarWrite::<()>::Written(101)
            })
            .unwrap(),
            PendingSidecarSync::Written
        ));
        let row = db.get_image_path(&path).unwrap();
        assert_eq!(row.sidecar_mtime_ns, 101);
        assert!(!row.sidecar_dirty);
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_sync_quarantines_a_parent_alias_retarget_without_publishing() {
        use std::cell::Cell;
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("photo.ARW"), b"raw").unwrap();
        std::fs::write(second.join("photo.ARW"), b"raw").unwrap();
        symlink(&first, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let first_owner = sidecar_owner_key(&aliased_raw).unwrap();
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&aliased_raw, 3, 1, 5)
            .unwrap();

        std::fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert_ne!(sidecar_owner_key(&aliased_raw).unwrap(), first_owner);
        let published = Cell::new(false);
        let result = db
            .synchronize_pending_sidecar(&aliased_raw, 3, 1, 5, |_| {
                published.set(true);
                PendingSidecarWrite::<()>::Written(99)
            })
            .unwrap();

        assert!(matches!(result, PendingSidecarSync::OwnerChanged));
        assert!(
            !published.get(),
            "the retargeted alias must never be written"
        );
        assert!(db.get_image_path(&aliased_raw).is_none());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*)
                       FROM quarantined_legacy_ratings
                      WHERE path = ?1
                        AND rating = 5",
                    [path_value(&aliased_raw)],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            1
        );
        assert!(!first.join("photo.xmp").exists());
        assert!(!second.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_sync_recovers_through_the_stored_owner_after_alias_removal() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        let physical_raw = physical.join("photo.ARW");
        std::fs::write(&physical_raw, b"raw").unwrap();
        symlink(&physical, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let (size, mtime_ns) = file_identity(&physical_raw);
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&aliased_raw, size, mtime_ns, 5)
            .unwrap();

        std::fs::remove_file(&alias).unwrap();
        let result = db
            .synchronize_pending_sidecar(&aliased_raw, size, mtime_ns, 5, |publication_path| {
                assert_eq!(publication_path, normalize_physical_path(&physical_raw));
                PendingSidecarWrite::<()>::Written(99)
            })
            .unwrap();

        assert!(matches!(result, PendingSidecarSync::Written));
        assert!(!db.get_image_path(&aliased_raw).unwrap().sidecar_dirty);
    }

    #[test]
    fn unavailable_sidecar_owner_remains_pending_until_the_path_recovers() {
        use std::cell::Cell;

        let directory = tempfile::tempdir().unwrap();
        let mounted = directory.path().join("mounted");
        let offline = directory.path().join("offline");
        std::fs::create_dir(&mounted).unwrap();
        let raw = mounted.join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&raw, size, mtime_ns, 5)
            .unwrap();

        std::fs::rename(&mounted, &offline).unwrap();
        let published = Cell::new(false);
        let error = db
            .synchronize_pending_sidecar(&raw, size, mtime_ns, 5, |_| {
                published.set(true);
                PendingSidecarWrite::<()>::Written(99)
            })
            .expect_err("a missing owner directory must remain retryable");
        assert!(matches!(error, PendingSidecarSyncError::SidecarOwner(_)));
        assert!(!published.get());
        assert!(db.get_image_path(&raw).unwrap().sidecar_dirty);

        std::fs::rename(&offline, &mounted).unwrap();
        assert!(matches!(
            db.synchronize_pending_sidecar(&raw, size, mtime_ns, 5, |publication_path| {
                assert_eq!(publication_path, raw);
                PendingSidecarWrite::<()>::Written(100)
            })
            .unwrap(),
            PendingSidecarSync::Written
        ));
        assert!(!db.get_image_path(&raw).unwrap().sidecar_dirty);
    }

    #[test]
    fn opening_a_legacy_database_adds_journal_columns_without_changing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let raw = normalize_physical_path(&dir.path().join("legacy.arw"));
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
        let raw = normalize_physical_path(&directory.path().join("recoverable.ARW"));
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

    #[cfg(unix)]
    #[test]
    fn migration_quarantines_ownerless_dirty_alias_retarget_with_equal_identity() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let identity =
            write_equal_identity_raws(&first_dir.join("photo.ARW"), &second_dir.join("photo.ARW"));
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let database_path = directory.path().join("ownerless-alias-retarget.db");
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
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES (?1, ?2, ?3, 5, 1)",
                rusqlite::params![path_value(&aliased_raw), identity.0, identity.1],
            )
            .unwrap();
        drop(connection);

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(!second_dir.join("photo.xmp").exists());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT rating
                       FROM quarantined_legacy_ratings
                      WHERE path = ?1",
                    [path_value(&aliased_raw)],
                    |row| row.get::<_, u8>(0),
                )
                .unwrap(),
            5
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_quarantines_all_unordered_same_name_dirty_histories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let first_raw = first_dir.join("photo.ARW");
        let second_raw = second_dir.join("photo.ARW");
        let identity = write_equal_identity_raws(&first_raw, &second_raw);
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let database_path = directory.path().join("unordered-ownerless-history.db");
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
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES
                    (?1, ?2, ?3, 1, 1),
                    (?4, ?2, ?3, 5, 1)",
                rusqlite::params![
                    path_value(&first_raw),
                    identity.0,
                    identity.1,
                    path_value(&aliased_raw),
                ],
            )
            .unwrap();
        drop(connection);

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            2,
            "neither legacy path has a provable order after alias retargeting"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        assert!(!first_dir.join("photo.xmp").exists());
        assert!(!second_dir.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn migration_clean_removed_alias_blocks_dirty_same_name_recovery() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = normalize_physical_path(directory.path());
        let physical = root.join("physical");
        let alias = root.join("alias");
        std::fs::create_dir(&physical).unwrap();
        let dirty_raw = physical.join("photo.ARW");
        let clean_raw = physical.join("photo.DNG");
        std::fs::write(&dirty_raw, b"dirty raw").unwrap();
        std::fs::write(&clean_raw, b"clean raw").unwrap();
        symlink(&physical, &alias).unwrap();
        let clean_alias = alias.join("photo.DNG");
        let dirty_identity = file_identity(&dirty_raw);
        let clean_identity = file_identity(&clean_raw);
        let database_path = root.join("removed-clean-alias.db");
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
                    path_value(&clean_alias),
                    clean_identity.0,
                    clean_identity.1,
                ],
            )
            .unwrap();
        drop(connection);

        std::fs::remove_file(&alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            1,
            "clean unresolved fallback state may remain, but must block recovery"
        );
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
        assert!(!physical.join("photo.xmp").exists());
    }

    #[test]
    fn migration_quarantines_a_dirty_owner_with_a_clean_legacy_alias() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("conflicting-legacy.db");
        let dirty_raw = normalize_physical_path(&directory.path().join("photo.ARW"));
        let clean_raw = normalize_physical_path(&directory.path().join("photo.DNG"));
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
        let owned_raw = normalize_physical_path(&directory.path().join("owned.arw"));
        std::fs::write(&owned_raw, b"owned raw").unwrap();
        let owned_metadata = std::fs::metadata(&owned_raw).unwrap();
        let owned_mtime_ns = owned_metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let owned_owner = sidecar_owner_key(&owned_raw).unwrap();
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
                CREATE UNIQUE INDEX images_sidecar_owners
                    ON images(sidecar_owner)
                 WHERE sidecar_owner IS NOT NULL;
                INSERT INTO viewr_schema_migrations
                VALUES ('rating-generation-and-owner-v6');",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty,
                     sidecar_owner)
                VALUES
                    (?1, ?2, ?3, 5, 1, ?4),
                    ('/p/legacy.arw', 20, 2, 2, 1, NULL)",
                rusqlite::params![
                    path_value(&owned_raw),
                    owned_metadata.len(),
                    owned_mtime_ns,
                    path_value(&owned_owner),
                ],
            )
            .unwrap();
        drop(connection);

        let compatible = Db::try_open_for_read(&path).unwrap().unwrap();
        assert!(!compatible.rating_schema_is_current());
        assert!(compatible.pending_sidecars().is_err());
        assert_eq!(
            compatible.get_image_path(&owned_raw).unwrap().rating,
            Some(5)
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
                path: owned_raw,
                size: owned_metadata.len(),
                mtime_ns: owned_mtime_ns,
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
    fn owner_key_migration_rekeys_one_valid_pending_row_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("owner-key-v7.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let current_owner = sidecar_owner_key(&raw).unwrap();
        let legacy_owner = directory.path().join("legacy-photo.xmp");
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "UPDATE images
                        SET sidecar_owner = ?2,
                            owner_key_version = 0
                      WHERE path = ?1",
                    rusqlite::params![path_value(&raw), path_value(&legacy_owner)],
                )
                .unwrap();
        }

        let db = Db::open(&database_path).unwrap();

        assert_eq!(
            db.pending_sidecars().unwrap(),
            vec![PendingSidecar {
                path: raw.clone(),
                size,
                mtime_ns,
                rating: 5,
            }]
        );
        let (owner, version) = db
            .conn
            .query_row(
                "SELECT sidecar_owner, owner_key_version
                   FROM images
                  WHERE path = ?1",
                [path_value(&raw)],
                |row| Ok((row_path(row, 0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(owner, current_owner);
        assert!(version >= CURRENT_OWNER_KEY_VERSION);
        assert!(migration_is_complete(&db.conn, SIDECAR_OWNER_KEY_MIGRATION).unwrap());
        for owner in [&legacy_owner, &current_owner] {
            assert!(
                sidecar_owner_revision_on(&db.conn, owner).unwrap() > 0,
                "both old and new owner spellings must be fenced"
            );
        }

        let changes = db.conn.total_changes();
        initialize_schema(&db.conn).unwrap();
        assert_eq!(db.conn.total_changes(), changes);
    }

    #[test]
    fn owner_key_migration_quarantines_every_colliding_dirty_row() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("colliding-owner-keys.db");
        let arw = directory.path().join("photo.ARW");
        let dng = directory.path().join("photo.DNG");
        std::fs::write(&arw, b"arw").unwrap();
        std::fs::write(&dng, b"dng").unwrap();
        let arw_identity = file_identity(&arw);
        let dng_identity = file_identity(&dng);
        let sidecar = arw.with_extension("xmp");
        crate::xmp::write_rating(&sidecar, 1).unwrap();
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute_batch("DROP INDEX images_sidecar_owners;")
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 2, 1, ?4, 0),
                        (?5, ?6, ?7, 5, 1, ?8, 0)",
                    rusqlite::params![
                        path_value(&arw),
                        arw_identity.0,
                        arw_identity.1,
                        path_value(&directory.path().join("legacy-arw.xmp")),
                        path_value(&dng),
                        dng_identity.0,
                        dng_identity.1,
                        path_value(&directory.path().join("legacy-dng.xmp")),
                    ],
                )
                .unwrap();
        }

        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            2
        );
        assert_eq!(crate::xmp::read_rating(&sidecar), Some(1));
        assert!(sidecar_owner_index_is_valid(&db.conn).unwrap());
    }

    #[test]
    fn owner_key_migration_quarantines_dirty_and_discards_clean_collision() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("mixed-colliding-owner-keys.db");
        let dirty = normalize_physical_path(&directory.path().join("photo.ARW"));
        let clean = normalize_physical_path(&directory.path().join("photo.DNG"));
        std::fs::write(&dirty, b"dirty").unwrap();
        std::fs::write(&clean, b"clean").unwrap();
        let dirty_identity = file_identity(&dirty);
        let clean_identity = file_identity(&clean);
        let current_owner = sidecar_owner_key(&dirty).unwrap();
        assert_eq!(sidecar_owner_key(&clean).unwrap(), current_owner);
        {
            let db = Db::open(&database_path).unwrap();
            db.conn
                .execute_batch(
                    "DROP INDEX images_sidecar_owners;
                     CREATE UNIQUE INDEX images_sidecar_owners
                         ON images(sidecar_owner)
                      WHERE 0;",
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 5, 1, ?4, 8),
                        (?5, ?6, ?7, 2, 0, ?4, 8)",
                    rusqlite::params![
                        path_value(&dirty),
                        dirty_identity.0,
                        dirty_identity.1,
                        path_value(&current_owner),
                        path_value(&clean),
                        clean_identity.0,
                        clean_identity.1,
                    ],
                )
                .unwrap();
        }

        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0,
            "no arbitrary row may survive a newly discovered owner collision"
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*)
                       FROM quarantined_legacy_ratings
                      WHERE rating = 5",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            1,
            "only unfinished work needs archival"
        );
        assert!(
            sidecar_owner_revision_on(&db.conn, &current_owner).unwrap() > 0,
            "the newly discovered owner must fence delayed work"
        );
    }

    #[test]
    fn owner_key_migration_removes_unresolvable_rows_and_archives_only_dirty_work() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("unresolvable-owner-keys.db");
        let missing_parent = directory.path().join("missing-parent");
        let dirty = missing_parent.join("dirty.ARW");
        let clean = missing_parent.join("clean.DNG");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, 5, 1, 4, 1, ?2, 0),
                        (?3, 5, 1, 2, 0, ?4, 0)",
                    rusqlite::params![
                        path_value(&dirty),
                        path_value(&directory.path().join("legacy-dirty.xmp")),
                        path_value(&clean),
                        path_value(&directory.path().join("legacy-clean.xmp")),
                    ],
                )
                .unwrap();
        }

        let db = Db::open(&database_path).unwrap();

        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| row
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            db.conn
                .query_row("SELECT path FROM quarantined_legacy_ratings", [], |row| {
                    row_path(row, 0)
                },)
                .unwrap(),
            dirty
        );
    }

    #[test]
    fn owner_key_migration_rekeys_a_filesystem_alias_spelling_when_supported() {
        let directory = tempfile::tempdir().unwrap();
        let physical_directory = std::fs::canonicalize(directory.path()).unwrap();
        let candidates = ["Photo.ARW", "caf\u{e9}.ARW"];
        let selected = candidates.into_iter().find_map(|name| {
            let raw = physical_directory.join(name);
            std::fs::write(&raw, b"raw").unwrap();
            let legacy_owner = raw.with_extension("xmp");
            let current_owner = sidecar_owner_key(&raw).ok()?;
            (legacy_owner != current_owner).then_some((raw, legacy_owner, current_owner))
        });
        let Some((raw, legacy_owner, current_owner)) = selected else {
            // Case-sensitive filesystems with no alternate Unicode spelling
            // cannot reproduce the pre-v8 key mismatch.
            return;
        };
        let database_path = directory.path().join("filesystem-alias-owner-key.db");
        let (size, mtime_ns) = file_identity(&raw);
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES (?1, ?2, ?3, 5, 1, ?4, 0)",
                    rusqlite::params![path_value(&raw), size, mtime_ns, path_value(&legacy_owner)],
                )
                .unwrap();
        }

        let db = Db::open(&database_path).unwrap();

        assert_eq!(
            db.conn
                .query_row(
                    "SELECT sidecar_owner FROM images WHERE path = ?1",
                    [path_value(&raw)],
                    |row| row_path(row, 0),
                )
                .unwrap(),
            current_owner
        );
        for owner in [&legacy_owner, &current_owner] {
            assert!(sidecar_owner_revision_on(&db.conn, owner).unwrap() > 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_key_migration_quarantines_dirty_alias_retarget_with_equal_identity() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let identity =
            write_equal_identity_raws(&first_dir.join("photo.ARW"), &second_dir.join("photo.ARW"));
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let first_owner = sidecar_owner_key(&aliased_raw).unwrap();
        let database_path = directory.path().join("owned-alias-retarget.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES (?1, ?2, ?3, 5, 1, ?4, 0)",
                    rusqlite::params![
                        path_value(&aliased_raw),
                        identity.0,
                        identity.1,
                        path_value(&first_owner),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert!(!second_dir.join("photo.xmp").exists());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT rating
                       FROM quarantined_legacy_ratings
                      WHERE path = ?1",
                    [path_value(&aliased_raw)],
                    |row| row.get::<_, u8>(0),
                )
                .unwrap(),
            5
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_key_migration_quarantines_all_unordered_same_name_dirty_histories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = normalize_physical_path(directory.path());
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let alias = root.join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let first_raw = first_dir.join("photo.ARW");
        let second_raw = second_dir.join("photo.ARW");
        let identity = write_equal_identity_raws(&first_raw, &second_raw);
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let database_path = root.join("unordered-owner-history.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 1, 1, ?4, 0),
                        (?5, ?2, ?3, 5, 1, ?6, 0)",
                    rusqlite::params![
                        path_value(&first_raw),
                        identity.0,
                        identity.1,
                        path_value(&first_raw.with_extension("xmp")),
                        path_value(&aliased_raw),
                        path_value(&aliased_raw.with_extension("xmp")),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            2,
            "neither legacy owner spelling has a provable order after alias retargeting"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        assert!(!first_dir.join("photo.xmp").exists());
        assert!(!second_dir.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mixed_migration_quarantines_ownerless_and_owned_unordered_dirty_histories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = normalize_physical_path(directory.path());
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let alias = root.join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let first_raw = first_dir.join("photo.ARW");
        let second_raw = second_dir.join("photo.ARW");
        let identity = write_equal_identity_raws(&first_raw, &second_raw);
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let database_path = root.join("mixed-unordered-owner-history.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "DELETE FROM viewr_schema_migrations WHERE name = ?1",
                    [RATING_GENERATION_MIGRATION],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 1, 1, ?4, 0),
                        (?5, ?2, ?3, 5, 1, NULL, 0)",
                    rusqlite::params![
                        path_value(&first_raw),
                        identity.0,
                        identity.1,
                        path_value(&first_raw.with_extension("xmp")),
                        path_value(&aliased_raw),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            2,
            "ownerless alias ambiguity must carry into the owned-row migration"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        assert!(!first_dir.join("photo.xmp").exists());
        assert!(!second_dir.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mixed_migration_quarantines_owned_alias_and_ownerless_physical_history() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = normalize_physical_path(directory.path());
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let alias = root.join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let first_raw = first_dir.join("photo.ARW");
        let second_raw = second_dir.join("photo.ARW");
        let identity = write_equal_identity_raws(&first_raw, &second_raw);
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let aliased_owner = aliased_raw.with_extension("xmp");
        let database_path = root.join("inverse-mixed-owner-history.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "DELETE FROM viewr_schema_migrations WHERE name = ?1",
                    [RATING_GENERATION_MIGRATION],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 1, 1, NULL, 0),
                        (?4, ?2, ?3, 5, 1, ?5, 0)",
                    rusqlite::params![
                        path_value(&first_raw),
                        identity.0,
                        identity.1,
                        path_value(&aliased_raw),
                        path_value(&aliased_owner),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            2,
            "owned alias ambiguity must carry into ownerless-row migration"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        assert!(!first_dir.join("photo.xmp").exists());
        assert!(!second_dir.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn owner_key_migration_discards_clean_alias_retarget_with_equal_identity() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let identity =
            write_equal_identity_raws(&first_dir.join("photo.ARW"), &second_dir.join("photo.ARW"));
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let first_owner = sidecar_owner_key(&aliased_raw).unwrap();
        let database_path = directory.path().join("owned-clean-alias-retarget.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES (?1, ?2, ?3, 5, 0, ?4, 0)",
                    rusqlite::params![
                        path_value(&aliased_raw),
                        identity.0,
                        identity.1,
                        path_value(&first_owner),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.get_image_path(&aliased_raw).is_none());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0,
            "clean fallback state is discarded rather than quarantined"
        );
        assert!(!second_dir.join("photo.xmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn owner_key_migration_clean_removed_alias_blocks_dirty_same_name_recovery() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = normalize_physical_path(directory.path());
        let physical = root.join("physical");
        let alias = root.join("alias");
        std::fs::create_dir(&physical).unwrap();
        let dirty_raw = physical.join("photo.ARW");
        let clean_raw = physical.join("photo.DNG");
        std::fs::write(&dirty_raw, b"dirty raw").unwrap();
        std::fs::write(&clean_raw, b"clean raw").unwrap();
        symlink(&physical, &alias).unwrap();
        let clean_alias = alias.join("photo.DNG");
        let dirty_identity = file_identity(&dirty_raw);
        let clean_identity = file_identity(&clean_raw);
        let database_path = root.join("owned-removed-clean-alias.db");
        {
            let db = Db::open(&database_path).unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner, owner_key_version)
                     VALUES
                        (?1, ?2, ?3, 1, 1, ?4, 0),
                        (?5, ?6, ?7, 5, 0, ?8, 0)",
                    rusqlite::params![
                        path_value(&dirty_raw),
                        dirty_identity.0,
                        dirty_identity.1,
                        path_value(&dirty_raw.with_extension("xmp")),
                        path_value(&clean_alias),
                        clean_identity.0,
                        clean_identity.1,
                        path_value(&clean_alias.with_extension("xmp")),
                    ],
                )
                .unwrap();
        }

        std::fs::remove_file(&alias).unwrap();
        let db = Db::open(&database_path).unwrap();

        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM images", [], |row| {
                    row.get::<_, usize>(0)
                })
                .unwrap(),
            0,
            "the unresolved clean alias makes the dirty owner history unordered"
        );
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
        assert!(!physical.join("photo.xmp").exists());
    }

    #[test]
    fn marker_present_schema_damage_is_replanned_and_repaired() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("damaged-v8-capabilities.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let current_owner = sidecar_owner_key(&raw).unwrap();
        let legacy_owner = directory.path().join("legacy-photo.xmp");
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.conn
                .execute_batch(
                    "DROP INDEX images_sidecar_owners;
                     CREATE UNIQUE INDEX images_sidecar_owners
                         ON images(sidecar_owner)
                      WHERE 0;
                     DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     UPDATE images
                        SET sidecar_owner = 'legacy-photo.xmp',
                            owner_key_version = 0;
                     CREATE TRIGGER images_reject_legacy_owner_insert
                     BEFORE INSERT ON images BEGIN SELECT 1; END;
                     CREATE TRIGGER images_reject_legacy_rating_update
                     BEFORE UPDATE ON images
                     BEGIN
                         SELECT RAISE(ABORT, 'over-strict damaged fence');
                     END;",
                )
                .unwrap();
            db.conn
                .execute(
                    "UPDATE images SET sidecar_owner = ?1",
                    [path_value(&legacy_owner)],
                )
                .expect_err("the deliberately over-strict trigger must be active");
        }
        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());

        {
            let db = Db::open(&database_path).unwrap();
            assert!(current_owner_schema_is_ready(&db.conn).unwrap());
            assert_eq!(db.pending_sidecars().unwrap().len(), 1);
            assert_eq!(
                db.conn
                    .query_row(
                        "SELECT sidecar_owner FROM images WHERE path = ?1",
                        [path_value(&raw)],
                        |row| row_path(row, 0),
                    )
                    .unwrap(),
                current_owner
            );
            assert!(
                db.conn
                    .execute(
                        "UPDATE images SET rating = 1 WHERE path = ?1",
                        [path_value(&raw)],
                    )
                    .is_err(),
                "the repaired fence must reject an obsolete writer"
            );
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     DROP TRIGGER images_reject_unowned_dirty_insert;
                     DROP TRIGGER images_reject_unowned_dirty_update;
                     ALTER TABLE images DROP COLUMN owner_key_version;",
                )
                .unwrap();
        }
        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());

        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT owner_key_version FROM images WHERE path = ?1",
                    [path_value(&raw)],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            CURRENT_OWNER_KEY_VERSION
        );
        assert_eq!(db.pending_sidecars().unwrap().len(), 1);
    }

    #[test]
    fn marker_present_trigger_damage_alone_forces_owner_revalidation() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("damaged-v8-fences.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let current_owner = sidecar_owner_key(&raw).unwrap();
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     UPDATE images
                        SET sidecar_owner = 'stale-owner.xmp',
                            owner_key_version = 0;
                     CREATE TRIGGER images_reject_legacy_owner_insert
                     BEFORE INSERT ON images BEGIN SELECT 1; END;
                     CREATE TRIGGER images_reject_legacy_rating_update
                     BEFORE UPDATE ON images BEGIN SELECT 1; END;",
                )
                .unwrap();
            assert!(sidecar_owner_index_is_valid(&db.conn).unwrap());
        }
        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());

        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT sidecar_owner, owner_key_version
                       FROM images
                      WHERE path = ?1",
                    [path_value(&raw)],
                    |row| Ok((row_path(row, 0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            (current_owner, CURRENT_OWNER_KEY_VERSION)
        );
    }

    #[test]
    fn interrupted_forced_repair_leaves_the_owner_marker_invalidated() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("interrupted-owner-repair.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let owner = sidecar_owner_key(&raw).unwrap();
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     UPDATE images
                        SET sidecar_owner = 'stale-owner.xmp',
                            owner_key_version = 0;",
                )
                .unwrap();
            invalidate_owner_key_marker(&db.conn).unwrap();
            assert!(
                !migration_is_complete(&db.conn, SIDECAR_OWNER_KEY_MIGRATION).unwrap(),
                "the simulated crash point must already be durable"
            );
            assert!(
                migration_is_complete(&db.conn, SIDECAR_OWNER_REPAIR_REQUIRED).unwrap(),
                "the forced-repair intent must survive the simulated crash"
            );
        }
        assert!(
            Db::try_open_for_read(&database_path).unwrap().is_none(),
            "a durable repair sentinel must keep read-only startup fail-closed"
        );

        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert!(!migration_is_complete(&db.conn, SIDECAR_OWNER_REPAIR_REQUIRED).unwrap());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT sidecar_owner, owner_key_version
                       FROM images
                      WHERE path = ?1",
                    [path_value(&raw)],
                    |row| Ok((row_path(row, 0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            (owner, CURRENT_OWNER_KEY_VERSION)
        );
    }

    #[test]
    fn repair_sentinel_dominates_a_still_present_current_marker() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("dominant-owner-repair-sentinel.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let owner = sidecar_owner_key(&raw).unwrap();
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     UPDATE images
                        SET sidecar_owner = 'stale-owner.xmp',
                            owner_key_version = 0;",
                )
                .unwrap();
            install_owner_key_fence(&db.conn).unwrap();
            db.conn
                .execute(
                    "INSERT OR IGNORE INTO viewr_schema_migrations (name) VALUES (?1)",
                    [SIDECAR_OWNER_REPAIR_REQUIRED],
                )
                .unwrap();
            assert!(migration_is_complete(&db.conn, SIDECAR_OWNER_KEY_MIGRATION).unwrap());
            assert!(!current_owner_schema_is_ready(&db.conn).unwrap());
        }

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT sidecar_owner, owner_key_version
                       FROM images
                      WHERE path = ?1",
                    [path_value(&raw)],
                    |row| Ok((row_path(row, 0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            (owner, CURRENT_OWNER_KEY_VERSION)
        );
    }

    #[test]
    fn fence_only_forced_repair_invalidates_every_retry_domain() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("fence-only-repair.db");
        let missing_sibling = directory.path().join("photo.ARW");
        let existing_sibling = directory.path().join("photo.DNG");
        let unresolved = directory.path().join("later").join("other.ARW");
        std::fs::write(&missing_sibling, b"raw").unwrap();
        std::fs::write(&existing_sibling, b"raw").unwrap();
        let (size, mtime_ns) = file_identity(&existing_sibling);
        let (owner_predecessor, global_predecessor) = {
            let db = Db::open(&database_path).unwrap();
            db.upsert_rating(
                existing_sibling.to_str().unwrap(),
                size,
                mtime_ns,
                Some(2),
                1,
            )
            .unwrap();
            let owner_predecessor = db.rating_owner_snapshot(&missing_sibling).unwrap();
            let global_predecessor = db.rating_global_snapshot(&unresolved).unwrap();
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_reject_legacy_owner_insert;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     UPDATE images SET rating = 5;
                     CREATE TRIGGER images_reject_legacy_owner_insert
                     BEFORE INSERT ON images BEGIN SELECT 1; END;
                     CREATE TRIGGER images_reject_legacy_rating_update
                     BEFORE UPDATE ON images BEGIN SELECT 1; END;",
                )
                .unwrap();
            std::fs::remove_file(&missing_sibling).unwrap();
            (owner_predecessor, global_predecessor)
        };

        let db = Db::open(&database_path).unwrap();
        std::fs::write(&missing_sibling, b"raw").unwrap();
        assert!(
            !db.record_rating_pending_sidecar_if_unchanged(
                &missing_sibling,
                3,
                1,
                3,
                owner_predecessor,
            )
            .unwrap(),
            "an owner snapshot from before fence repair must be invalidated"
        );
        std::fs::create_dir(unresolved.parent().unwrap()).unwrap();
        std::fs::write(&unresolved, b"raw").unwrap();
        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(
                &unresolved,
                3,
                1,
                3,
                global_predecessor,
            )
            .unwrap(),
            "an unresolved snapshot from before fence repair must be invalidated"
        );
        assert_eq!(
            db.get_image(existing_sibling.to_str().unwrap())
                .unwrap()
                .rating,
            Some(5)
        );
    }

    #[test]
    fn marker_present_generation_trigger_damage_is_repaired_behind_a_retry_barrier() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("damaged-generation-trigger.db");
        let raw = directory.path().join("photo.ARW");
        let unresolved = directory.path().join("later").join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let (owner_predecessor, global_predecessor) = {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            let owner_predecessor = db.rating_owner_snapshot(&raw).unwrap();
            let global_predecessor = db.rating_global_snapshot(&unresolved).unwrap();
            db.conn
                .execute_batch(
                    "DROP TRIGGER images_generation_after_update_v2;
                     UPDATE image_revisions
                        SET revision = revision + 10
                      WHERE path IN (SELECT path FROM images);
                     CREATE TRIGGER images_generation_after_update_v2
                     BEFORE UPDATE ON images
                     BEGIN
                         SELECT RAISE(ABORT, 'over-strict damaged generation trigger');
                     END;
                     DROP TRIGGER sidecar_owner_v6_update_order;
                     CREATE TRIGGER sidecar_owner_v6_update_order
                     AFTER UPDATE OF revision ON sidecar_owner_revisions
                     BEGIN SELECT 1; END;
                     DROP TRIGGER images_reject_legacy_rating_update;
                     CREATE TRIGGER images_reject_legacy_rating_update
                     BEFORE UPDATE ON images
                     BEGIN
                         SELECT RAISE(ABORT, 'over-strict damaged owner fence');
                     END;",
                )
                .unwrap();
            (owner_predecessor, global_predecessor)
        };

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert!(
            schema_object_sql_is(
                &db.conn,
                "trigger",
                "sidecar_owner_v6_update_order",
                V6_OWNER_UPDATE_ORDER_SQL,
            )
            .unwrap()
        );
        assert!(
            !db.record_rating_pending_sidecar_if_unchanged(
                &raw,
                size,
                mtime_ns,
                2,
                owner_predecessor,
            )
            .unwrap(),
            "a pre-repair owner snapshot must not survive capability repair"
        );
        std::fs::create_dir(unresolved.parent().unwrap()).unwrap();
        std::fs::write(&unresolved, b"raw").unwrap();
        assert!(
            !db.record_rating_pending_sidecar_if_global_unchanged(
                &unresolved,
                3,
                1,
                2,
                global_predecessor,
            )
            .unwrap(),
            "a pre-repair unresolved snapshot must not survive capability repair"
        );
    }

    #[test]
    fn opposite_type_schema_objects_are_removed_during_repair() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("opposite-type-generation-object.db");
        let raw = directory.path().join("removed.ARW");
        let unresolved = directory.path().join("missing").join("unresolved.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.upsert_rating(unresolved.to_str().unwrap(), 3, 1, Some(4), 0)
                .unwrap();
            db.conn
                .execute_batch("DROP TRIGGER images_reject_unowned_dirty_update;")
                .unwrap();
            db.conn
                .execute(
                    "UPDATE images
                        SET sidecar_dirty = 1
                      WHERE path = ?1",
                    [path_value(&unresolved)],
                )
                .unwrap();
            std::fs::remove_file(&raw).unwrap();
            db.conn
                .execute_batch(
                    "DROP INDEX images_pending_sidecars;
                     CREATE TRIGGER images_pending_sidecars
                     BEFORE UPDATE ON images
                     BEGIN
                         SELECT RAISE(ABORT, 'hostile same-name trigger');
                     END;
                     CREATE INDEX images_pending_sidecars
                         ON images(path)
                      WHERE sidecar_dirty = 1
                        AND sidecar_quarantined = 0
                        AND rating IS NOT NULL;
                     CREATE TRIGGER images_sidecar_owners
                     BEFORE DELETE ON images
                     BEGIN
                         SELECT RAISE(ABORT, 'hostile same-name trigger');
                     END;
                     CREATE INDEX images_reject_legacy_owner_insert
                         ON images(size);
                     CREATE INDEX images_reject_legacy_rating_update
                         ON images(mtime_ns);",
                )
                .unwrap();
            assert!(!rating_generation_objects_are_ready(&db.conn).unwrap());
            assert!(!sidecar_owner_index_is_valid(&db.conn).unwrap());
            assert!(!current_owner_schema_is_ready(&db.conn).unwrap());
        }

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert!(schema_object_exists(&db.conn, "index", "images_pending_sidecars").unwrap());
        assert!(!schema_object_exists(&db.conn, "trigger", "images_pending_sidecars").unwrap());
        assert!(schema_object_exists(&db.conn, "index", "images_sidecar_owners").unwrap());
        assert!(!schema_object_exists(&db.conn, "trigger", "images_sidecar_owners").unwrap());
        assert!(db.pending_sidecars().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM quarantined_legacy_ratings",
                    [],
                    |row| { row.get::<_, usize>(0) }
                )
                .unwrap(),
            2,
            "repair DML must run after removing hostile opposite-type objects"
        );
        for name in [
            "images_reject_legacy_owner_insert",
            "images_reject_legacy_rating_update",
        ] {
            assert!(schema_object_exists(&db.conn, "trigger", name).unwrap());
            assert!(!schema_object_exists(&db.conn, "index", name).unwrap());
        }
    }

    #[test]
    fn marker_present_missing_revision_ledgers_are_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("missing-revision-ledgers.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            db.conn
                .execute_batch(
                    "DROP TABLE image_revisions;
                     DROP TABLE sidecar_owner_revisions;
                     DROP TABLE rating_global_revision;",
                )
                .unwrap();
        }

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        assert_eq!(db.pending_sidecars().unwrap().len(), 1);
        assert!(db.rating_owner_snapshot(&raw).is_ok());
        assert!(db.rating_global_snapshot(&raw).is_ok());
    }

    #[test]
    fn marker_present_malformed_revision_ledger_keys_fail_closed() {
        for (name, damage) in [
            (
                "image",
                "DROP TABLE image_revisions;
                 CREATE TABLE image_revisions (
                     path TEXT,
                     revision INTEGER NOT NULL
                 );",
            ),
            (
                "owner",
                "DROP TABLE sidecar_owner_revisions;
                 CREATE TABLE sidecar_owner_revisions (
                     owner,
                     revision INTEGER NOT NULL,
                     global_revision INTEGER NOT NULL DEFAULT 0
                 );",
            ),
            (
                "image-type",
                "DROP TABLE image_revisions;
                 CREATE TABLE image_revisions (
                     path TEXT PRIMARY KEY,
                     revision TEXT NOT NULL
                 );",
            ),
            (
                "global",
                "DROP TABLE rating_global_revision;
                 CREATE TABLE rating_global_revision (
                     singleton INTEGER,
                     revision INTEGER NOT NULL,
                     ownerless_revision INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO rating_global_revision
                    (singleton, revision, ownerless_revision)
                 VALUES (1, 0, 0);",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let database_path = directory.path().join(format!("malformed-{name}.db"));
            {
                let db = Db::open(&database_path).unwrap();
                db.conn.execute_batch(damage).unwrap();
            }

            assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
            assert!(
                matches!(
                    Db::open(&database_path),
                    Err(DbError::Sqlite(rusqlite::Error::InvalidQuery))
                ),
                "malformed {name} ledger must not be accepted or rewritten"
            );
        }
    }

    #[test]
    fn malformed_revision_values_fail_before_repair_arithmetic() {
        for (name, damage) in [
            ("image", "UPDATE images SET revision = 'abc';"),
            (
                "image-ledger",
                "UPDATE image_revisions SET revision = 'abc';",
            ),
            (
                "owner-ledger",
                "UPDATE sidecar_owner_revisions SET revision = 'abc';",
            ),
            (
                "owner-global",
                "UPDATE sidecar_owner_revisions SET global_revision = 'abc';",
            ),
            (
                "global",
                "UPDATE rating_global_revision SET revision = 'abc';",
            ),
            (
                "ownerless",
                "UPDATE rating_global_revision SET ownerless_revision = 'abc';",
            ),
            (
                "overflow",
                "UPDATE image_revisions SET revision = 9223372036854775807;",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let database_path = directory.path().join(format!("malformed-value-{name}.db"));
            let raw = directory.path().join("photo.ARW");
            std::fs::write(&raw, b"raw").unwrap();
            let raw = normalize_physical_path(&raw);
            let (size, mtime_ns) = file_identity(&raw);
            {
                let db = Db::open(&database_path).unwrap();
                db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                    .unwrap();
                db.conn
                    .execute_batch(&format!(
                        "DROP TRIGGER images_generation_after_update_v2;
                         {damage}"
                    ))
                    .unwrap();
            }

            assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
            assert!(
                matches!(
                    Db::open(&database_path),
                    Err(DbError::Sqlite(rusqlite::Error::InvalidQuery))
                ),
                "malformed {name} counter must not be coerced during repair"
            );
        }
    }

    #[test]
    fn nullable_owner_key_version_cannot_bypass_fences_and_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("nullable-owner-version.db");
        {
            let db = Db::open(&database_path).unwrap();
            db.conn
                .execute_batch(
                    "PRAGMA writable_schema = ON;
                     UPDATE sqlite_schema
                        SET sql = replace(
                            sql,
                            'owner_key_version INTEGER NOT NULL DEFAULT 0',
                            'owner_key_version INTEGER'
                        )
                      WHERE type = 'table'
                        AND name = 'images';
                     PRAGMA writable_schema = OFF;",
                )
                .unwrap();
        }
        {
            let connection = Connection::open(&database_path).unwrap();
            assert!(
                connection
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating, sidecar_dirty)
                         VALUES ('/obsolete/photo.ARW', 3, 1, 2, 0)",
                        [],
                    )
                    .is_err(),
                "NULL must be treated as an obsolete owner-key version"
            );
        }

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        assert!(matches!(
            Db::open(&database_path),
            Err(DbError::Sqlite(rusqlite::Error::InvalidQuery))
        ));
    }

    #[test]
    fn marker_present_unowned_dirty_contamination_is_promoted_during_repair() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("unowned-dirty-repair.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let owner = sidecar_owner_key(&raw).unwrap();
        let (size, mtime_ns) = file_identity(&raw);
        {
            let db = Db::open(&database_path).unwrap();
            db.conn
                .execute_batch("DROP TRIGGER images_reject_unowned_dirty_insert;")
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         owner_key_version)
                     VALUES (?1, ?2, ?3, 5, 1, 8)",
                    rusqlite::params![path_value(&raw), size, mtime_ns],
                )
                .unwrap();
        }

        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
        let db = Db::open(&database_path).unwrap();
        assert!(current_owner_schema_is_ready(&db.conn).unwrap());
        let pending = db.pending_sidecars_with_owners().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].owner, owner);
        assert_eq!(pending[0].pending.path, raw);
        assert!(
            db.conn
                .query_row(
                    "SELECT owner_key_version FROM images WHERE path = ?1",
                    [path_value(&pending[0].pending.path)],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                > CURRENT_OWNER_KEY_VERSION
        );
    }

    #[test]
    fn v8_fence_contains_exact_obsolete_inserts_and_allows_completion() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("photo.ARW");
        let missing = directory.path().join("missing").join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let owner = sidecar_owner_key(&raw).unwrap();
        let db = Db::open_in_memory().unwrap();

        assert!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner)
                     VALUES (?1, ?2, ?3, 2, 1, ?4)",
                    rusqlite::params![path_value(&raw), size, mtime_ns, path_value(&owner)],
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty)
                     VALUES (?1, 3, 1, 2, 0)",
                    [path_value(&missing)],
                )
                .is_err(),
            "a released clean insert cannot bypass the ownerless epoch"
        );
        assert!(db.get_image_path(&missing).is_none());

        db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 4)
            .unwrap();
        assert!(
            db.complete_pending_sidecar(&raw, size, mtime_ns, 4, 10)
                .unwrap()
        );
        assert_eq!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         sidecar_dirty, last_seen)
                     VALUES (?1, ?2, ?3, 2, 0, 1, unixepoch())
                     ON CONFLICT(path) DO UPDATE SET
                        size = excluded.size,
                        mtime_ns = excluded.mtime_ns,
                        rating = excluded.rating,
                        sidecar_dirty = 1,
                        last_seen = excluded.last_seen",
                    rusqlite::params![path_value(&raw), size, mtime_ns],
                )
                .unwrap(),
            0,
            "the released ownerless dirty upsert must reach the legacy fence"
        );
        let protected = db.get_image_path(&raw).unwrap();
        assert_eq!(protected.rating, Some(4));
        assert!(protected.sidecar_dirty);
        assert!(
            db.complete_pending_sidecar(&raw, size, mtime_ns, 4, 11)
                .unwrap()
        );
        assert_eq!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty)
                     VALUES (?1, ?2, ?3, 2, 0)
                     ON CONFLICT(path) DO UPDATE SET
                        size = excluded.size,
                        mtime_ns = excluded.mtime_ns,
                        rating = excluded.rating,
                        sidecar_dirty = 0",
                    rusqlite::params![path_value(&raw), size, mtime_ns],
                )
                .unwrap(),
            0,
            "an obsolete exact-path clean upsert must be contained"
        );
        let protected = db.get_image_path(&raw).unwrap();
        assert_eq!(protected.rating, Some(4));
        assert!(
            protected.sidecar_dirty,
            "the current value must remain authoritative over obsolete XMP"
        );
        {
            let transaction =
                Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).unwrap();
            assert_eq!(
                transaction
                    .execute(
                        "DELETE FROM images WHERE sidecar_owner = ?1",
                        [path_value(&owner)],
                    )
                    .unwrap(),
                1
            );
            assert!(
                transaction
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating, sidecar_dirty,
                             sidecar_owner)
                         VALUES (?1, ?2, ?3, 2, 0, ?4)",
                        rusqlite::params![path_value(&raw), size, mtime_ns, path_value(&owner),],
                    )
                    .is_err(),
                "the obsolete upsert must abort after any preceding owner delete"
            );
        }
        assert_eq!(
            db.get_image_path(&raw).unwrap().rating,
            Some(4),
            "dropping the failed obsolete transaction must restore prior pending work"
        );
        assert_eq!(
            db.conn
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_dirty,
                         sidecar_owner)
                     VALUES (?1, ?2, ?3, 2, 1, ?4)
                     ON CONFLICT(path) DO UPDATE SET
                        rating = excluded.rating,
                        sidecar_dirty = 1,
                        sidecar_owner = excluded.sidecar_owner",
                    rusqlite::params![path_value(&raw), size, mtime_ns, path_value(&owner)],
                )
                .unwrap(),
            0
        );
        assert_eq!(db.get_image_path(&raw).unwrap().rating, Some(4));
        assert!(
            db.conn
                .execute(
                    "UPDATE images
                        SET rating = 3,
                            sidecar_dirty = 1
                      WHERE path = ?1",
                    [path_value(&raw)],
                )
                .is_err()
        );
        assert_eq!(
            db.conn
                .execute(
                    "UPDATE images
                        SET sidecar_mtime_ns = 99,
                            sidecar_dirty = 0
                      WHERE path = ?1",
                    [path_value(&raw)],
                )
                .unwrap(),
            1
        );
        assert!(!db.get_image_path(&raw).unwrap().sidecar_dirty);
    }

    #[test]
    fn read_only_handle_redetects_owner_schema_after_background_migration() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("dynamic-read-schema.db");
        let raw = directory.path().join("photo.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        let raw = normalize_physical_path(&raw);
        let (size, mtime_ns) = file_identity(&raw);
        let current_owner = sidecar_owner_key(&raw).unwrap();
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(raw.to_str().unwrap(), size, mtime_ns, 5)
                .unwrap();
            remove_v8_marker_and_fence(&db.conn);
            db.conn
                .execute(
                    "UPDATE images
                        SET sidecar_owner = ?2,
                            owner_key_version = 0
                      WHERE path = ?1",
                    rusqlite::params![
                        path_value(&raw),
                        path_value(&directory.path().join("legacy.xmp"))
                    ],
                )
                .unwrap();
        }
        let read = Db::try_open_for_read(&database_path)
            .unwrap()
            .expect("v7 owner schema is display-compatible");
        assert!(!read.rating_schema_is_current());
        assert!(read.pending_sidecars().is_err());

        drop(Db::open(&database_path).unwrap());
        let snapshot = read
            .rating_snapshot(
                std::iter::once(raw.as_path()),
                &[Some(current_owner.clone())],
            )
            .unwrap();

        assert_eq!(
            snapshot.by_owner.get(&current_owner).unwrap().rating,
            Some(5)
        );
        assert!(!snapshot.legacy_owners_require_derivation);
    }

    #[test]
    fn latency_sensitive_read_rejects_incomplete_owner_capabilities() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("incomplete-owner.db");
        {
            let db = Db::open(&database_path).unwrap();
            db.conn
                .execute_batch("DROP INDEX images_sidecar_owners;")
                .unwrap();
        }
        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());

        drop(Db::open(&database_path).unwrap());
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute("DELETE FROM viewr_schema_migrations", [])
            .unwrap();
        drop(connection);
        assert!(Db::try_open_for_read(&database_path).unwrap().is_none());
    }

    #[test]
    fn pending_sidecar_scan_uses_the_partial_index_and_fails_closed() {
        let db = Db::open_in_memory().unwrap();
        let explain = format!("EXPLAIN QUERY PLAN {PENDING_SIDECARS_QUERY}");
        let mut statement = db.conn.prepare(&explain).unwrap();
        let details = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("images_pending_sidecars")),
            "unexpected pending-sidecar query plan: {details:?}"
        );
        drop(statement);

        db.conn
            .execute_batch(
                "DROP TRIGGER images_reject_unowned_dirty_insert;
                 DROP TRIGGER images_reject_unowned_dirty_update;
                 INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty,
                     owner_key_version)
                 VALUES ('/p/unowned.arw', 10, 1, 4, 1, 8);",
            )
            .unwrap();
        assert!(
            db.pending_sidecars().is_err(),
            "an invariant-violating journal must block automatic recovery"
        );
    }

    #[test]
    fn duplicate_pending_owners_fail_closed_without_the_unique_index() {
        let db = Db::open_in_memory().unwrap();
        db.conn
            .execute_batch("DROP INDEX images_sidecar_owners;")
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty,
                     sidecar_owner, owner_key_version)
                 VALUES
                    ('/p/one.arw', 10, 1, 2, 1, '/p/shared.xmp', 8),
                    ('/p/two.dng', 20, 2, 5, 1, '/p/shared.xmp', 8)",
                [],
            )
            .unwrap();

        assert!(db.pending_sidecars().is_err());
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

        let first_predecessor = db.rating_global_snapshot(&first).unwrap();
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
        let second_predecessor = db.rating_global_snapshot(&second).unwrap();
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

        assert!(Db::try_open_for_read(&path).unwrap().is_none());
        let connection = Connection::open(&path).unwrap();
        assert!(!has_column(&connection, "images", "sidecar_owner").unwrap());
        assert!(
            !migration_is_complete(&connection, RATING_GENERATION_MIGRATION).unwrap(),
            "the UI read path must leave migration to the persistence worker"
        );
    }

    #[test]
    fn latency_sensitive_read_rejects_legacy_duplicate_path_shape() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("duplicate-legacy-read.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE images (
                    path TEXT,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                VALUES
                    ('/p/duplicate.arw', 42, 7, 2, 1),
                    ('/p/duplicate.arw', 42, 7, 5, 1);",
            )
            .unwrap();
        drop(connection);

        assert!(
            Db::try_open_for_read(&path).unwrap().is_none(),
            "a duplicate-path legacy table is repair-only, never display-compatible"
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

        assert!(
            opened.is_ok_and(|db| db.is_some_and(|db| db.rating_schema_is_current())),
            "WAL readers should coexist with a writer"
        );
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
        const ROUNDS: usize = 4;

        let directory = tempfile::tempdir().unwrap();
        let arw = directory.path().join("photo.ARW");
        let dng = directory.path().join("photo.DNG");
        std::fs::write(&arw, b"raw").unwrap();
        std::fs::write(&dng, b"raw").unwrap();
        let executable = std::env::current_exe().unwrap();
        for round in 0..ROUNDS {
            let database_path = directory.path().join(format!("multi-process-{round}.db"));
            let gate_path = directory.path().join(format!("start-writers-{round}"));
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
            let statuses = children
                .iter_mut()
                .map(|child| child.wait().unwrap())
                .collect::<Vec<_>>();
            assert!(
                statuses.iter().all(std::process::ExitStatus::success),
                "round {round} child statuses: {statuses:?}"
            );

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
