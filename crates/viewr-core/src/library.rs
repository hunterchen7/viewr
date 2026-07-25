//! Ratings orchestration and best-effort persistence.
//!
//! An unfinished dirty database journal entry remains authoritative regardless
//! of sidecar modification time until it is flushed. Otherwise, a current
//! sidecar wins over the clean database row.
//! Embedded ratings arrive later through the metadata wave and fill only
//! entries still missing a rating. The persistence thread attempts to journal
//! updates before debouncing XMP sidecar writes (~400 ms per image).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::db::{
    Db, PendingSidecarSync, PendingSidecarWrite, RatingGlobalSnapshot, RatingOwnerSnapshot,
    default_db_path,
};
use crate::folder::{FolderEntry, normalize_physical_path, sidecar_owner_key, sidecar_owner_keys};
use crate::xmp;

const SIDECAR_DEBOUNCE: Duration = Duration::from_millis(400);
const SIDECAR_RETRY: Duration = Duration::from_secs(5);
const SHUTDOWN_FLUSH_ATTEMPTS: usize = 3;
const SHUTDOWN_RETRY_DELAY: Duration = Duration::from_millis(50);

enum Cmd {
    SetRating {
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    },
    Flush {
        done: Option<Sender<bool>>,
    },
    #[cfg(test)]
    Barrier {
        done: Sender<()>,
    },
    Shutdown,
}

/// Asynchronous rating persistence service.
///
/// [`set_rating`](Self::set_rating) queues the update on a dedicated thread,
/// which attempts to journal it and coalesces repeated changes to the same RAW
/// before writing its XMP sidecar. The optional SQLite database is an
/// accelerator and crash-recovery journal; sidecars remain the interoperable
/// source of truth. Dropping the service makes a bounded sequence of
/// best-effort flush attempts and joins its worker. A failed sidecar remains
/// recoverable on restart only if its dirty database journal write succeeded.
pub struct Library {
    tx: Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
    /// Avoid a channel allocation and worker round-trip on ordinary
    /// navigation when no rating has changed since the last flush.
    dirty: Arc<AtomicBool>,
}

/// Initial persisted per-index ratings resolved across the journal, sidecars,
/// and clean database rows. Embedded camera ratings arrive later via the
/// metadata wave; apply them only where this map has no entry.
///
/// A journal row marked dirty wins over any sidecar because it represents a
/// rating accepted before an interrupted sidecar write. Database lookup is
/// best-effort when `db` is `None` or contains no matching row.
pub fn load_ratings(entries: &[FolderEntry], db: Option<&Db>) -> HashMap<usize, u8> {
    load_ratings_with_owners(entries, db).0
}

/// Resolves initial ratings together with stable XMP ownership keys.
///
/// The companion owner vector lets callers keep sibling RAW containers that
/// share one XMP target consistent after a user rating without repeating
/// filesystem probes.
#[doc(hidden)]
pub fn load_ratings_with_owners(
    entries: &[FolderEntry],
    db: Option<&Db>,
) -> (HashMap<usize, u8>, Vec<Option<PathBuf>>) {
    let mut out = HashMap::new();
    let owners = sidecar_owner_keys(entries);
    let mut dirty_by_owner: HashMap<PathBuf, Option<u8>> = HashMap::new();
    for (i, (entry, owner)) in entries.iter().zip(&owners).enumerate() {
        // One XMP target can be shared by multiple RAW containers (for
        // example, `photo.ARW` and `photo.DNG`). A crash-recovery journal is
        // authoritative for that owner, not just for the exact RAW path that
        // most recently set it.
        let dirty_rating = db.and_then(|db| {
            let owner = owner.as_ref()?;
            *dirty_by_owner.entry(owner.clone()).or_insert_with(|| {
                db.dirty_rating_for_owner(owner)
                    .ok()
                    .flatten()
                    .filter(|row| {
                        current_raw_identity(&row.path).ok() == Some((row.size, row.mtime_ns))
                    })
                    .map(|row| row.rating)
            })
        });
        if let Some(rating) = dirty_rating {
            out.insert(i, rating);
            continue;
        }

        let sidecar = entry.sidecar_path();
        let sidecar_mtime = std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        let row =
            db.and_then(|db| db.get_image_for_identity(&entry.path, entry.size, entry.mtime_ns));

        // A dirty row is a database rating that has not reached its sidecar.
        // It must win even when the old sidecar has an equal or newer mtime.
        let from_sidecar = sidecar_mtime.and_then(|mt| {
            let db_row = row.as_ref();
            let db_mt = db_row.map_or(0, |r| r.sidecar_mtime_ns);
            if !db_row.is_some_and(|r| r.sidecar_dirty) && mt >= db_mt {
                xmp::read_rating(&sidecar)
            } else {
                None
            }
        });
        let rating = from_sidecar.or(row.and_then(|r| r.rating));
        if let Some(r) = rating {
            out.insert(i, r);
        }
    }
    (out, owners)
}

impl Library {
    /// Spawns the persistence thread.
    ///
    /// The platform-default database is best-effort: failure to locate or open
    /// it does not prevent XMP sidecar writes. Journaled sidecar writes left by
    /// a prior process are resumed automatically when the database opens.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the persistence thread.
    pub fn start() -> Self {
        Self::start_with(default_db_path(), SIDECAR_DEBOUNCE)
    }

    fn start_with(db_path: Option<PathBuf>, debounce: Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let dirty = Arc::new(AtomicBool::new(false));
        let worker_dirty = dirty.clone();
        let worker = std::thread::spawn(move || {
            let db = db_path.and_then(|path| match Db::open(&path) {
                Ok(db) => Some(db),
                Err(error) => {
                    eprintln!(
                        "rating database open failed for {}: {error}",
                        path.display()
                    );
                    None
                }
            });
            persist_thread(&rx, db.as_ref(), debounce, &worker_dirty);
        });
        Self {
            tx,
            worker: Some(worker),
            dirty,
        }
    }

    /// Queues a rating for attempted journaling and debounced sidecar writing.
    ///
    /// The method is non-blocking and repeated ratings for the same path
    /// coalesce. The worker normalizes physical path aliases before touching
    /// the journal or XMP. Values are not range-checked; the UI convention is
    /// `0..=5`, where zero means unrated. If the worker has already stopped,
    /// the update is silently ignored.
    pub fn set_rating(&self, entry: &FolderEntry, rating: u8) {
        if self
            .tx
            .send(Cmd::SetRating {
                path: entry.path.clone(),
                size: entry.size,
                mtime_ns: entry.mtime_ns,
                rating,
            })
            .is_ok()
        {
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Requests one immediate attempt for locally dirty sidecars and waits for
    /// that attempt to finish.
    ///
    /// A failed sidecar or database synchronization remains locally dirty and
    /// is retried by a later flush or the worker's retry timer. Recovery after
    /// process exit additionally requires a successful dirty database journal
    /// write. Errors are logged because this best-effort API intentionally does
    /// not return a `Result`.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let (done, wait) = std::sync::mpsc::channel();
        let clean = self.tx.send(Cmd::Flush { done: Some(done) }).is_ok()
            && wait.recv().is_ok_and(|clean| clean);
        if !clean {
            // Keep the fast-path state dirty so a later flush retries any
            // sidecar that failed. The DB remains the crash-safe authority.
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Requests one immediate persistence attempt without blocking the caller.
    ///
    /// Commands use one FIFO channel, so a preceding [`set_rating`](Self::set_rating)
    /// is journaled before this flush is handled. Failures restore the dirty
    /// marker on the worker thread for a later retry. Dropping the library
    /// blocks for a bounded sequence of best-effort persistence attempts.
    pub fn request_flush(&self) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        if self.tx.send(Cmd::Flush { done: None }).is_err() {
            self.dirty.store(true, Ordering::Release);
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Pending {
    path: PathBuf,
    owner_resolved: bool,
    sequence: u64,
    size: u64,
    mtime_ns: i64,
    rating: u8,
    journaled: bool,
    journal_predecessor: Option<JournalPredecessor>,
    due: Instant,
}

#[derive(Clone, Copy)]
enum JournalPredecessor {
    Owner(RatingOwnerSnapshot),
    Global(RatingGlobalSnapshot),
}

fn persist_thread(rx: &Receiver<Cmd>, db: Option<&Db>, debounce: Duration, dirty: &AtomicBool) {
    let mut pending: HashMap<PathBuf, Pending> = db
        .map(|db| match db.pending_sidecars_with_owners() {
            Ok(rows) => rows
                .into_iter()
                .map(|row| {
                    (
                        row.owner,
                        Pending {
                            path: row.pending.path,
                            owner_resolved: true,
                            sequence: 0,
                            size: row.pending.size,
                            mtime_ns: row.pending.mtime_ns,
                            rating: row.pending.rating,
                            journaled: true,
                            journal_predecessor: None,
                            due: Instant::now(),
                        },
                    )
                })
                .collect(),
            Err(error) => {
                eprintln!("failed to load pending sidecars: {error}");
                HashMap::new()
            }
        })
        .unwrap_or_default();
    let mut latest_sequence_by_owner = pending
        .keys()
        .cloned()
        .map(|owner| (owner, 0_u64))
        .collect::<HashMap<_, _>>();
    let mut next_sequence = 0_u64;

    loop {
        let timeout = pending
            .values()
            .map(|p| p.due.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(Cmd::SetRating {
                path,
                size,
                mtime_ns,
                rating,
            }) => {
                next_sequence = next_sequence.saturating_add(1);
                // Keep the public caller non-blocking while converging parent
                // symlinks, case aliases, and temporarily missing leaves onto
                // one physical ownership key.
                let path = normalize_physical_path(&path);
                // Commit the rating and dirty marker together before the
                // debounced sidecar write. A restart can then recover both the
                // rating precedence and the unfinished sidecar operation.
                let (owner, owner_resolved) = match sidecar_owner_key(&path) {
                    Ok(owner) => (owner, true),
                    Err(error) => {
                        eprintln!(
                            "retaining rating with unresolved sidecar owner for {}: {error}",
                            path.display()
                        );
                        // This is only a private in-memory key. It must never
                        // be used for journaling or sidecar publication.
                        (path.with_extension("xmp"), false)
                    }
                };
                let journal_predecessor = db.and_then(|db| {
                    let snapshot = if owner_resolved {
                        db.rating_owner_snapshot(&path)
                            .map(JournalPredecessor::Owner)
                    } else {
                        db.rating_global_snapshot().map(JournalPredecessor::Global)
                    };
                    match snapshot {
                        Ok(predecessor) => Some(predecessor),
                        Err(error) => {
                            eprintln!(
                                "rating database ownership read failed for {}: {error}",
                                path.display()
                            );
                            None
                        }
                    }
                });
                let journaled = owner_resolved
                    && db.as_ref().is_some_and(|db| {
                        match db
                            .record_rating_pending_sidecar_canonical(&path, size, mtime_ns, rating)
                        {
                            Ok(()) => true,
                            Err(error) => {
                                eprintln!(
                                    "rating database write failed for {}: {error}",
                                    path.display()
                                );
                                false
                            }
                        }
                    });
                if owner_resolved {
                    latest_sequence_by_owner.insert(owner.clone(), next_sequence);
                }
                pending.insert(
                    owner,
                    Pending {
                        path,
                        owner_resolved,
                        sequence: next_sequence,
                        size,
                        mtime_ns,
                        rating,
                        journaled,
                        journal_predecessor: (!journaled).then_some(journal_predecessor).flatten(),
                        due: Instant::now() + debounce,
                    },
                );
            }
            Ok(Cmd::Flush { done }) => {
                let clean = flush_due(&mut pending, &mut latest_sequence_by_owner, db, true);
                if !clean {
                    dirty.store(true, Ordering::Release);
                }
                if let Some(done) = done {
                    let _ = done.send(clean);
                }
            }
            #[cfg(test)]
            Ok(Cmd::Barrier { done }) => {
                let _ = done.send(());
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                let clean = retry_shutdown_flush(
                    || flush_due(&mut pending, &mut latest_sequence_by_owner, db, true),
                    || std::thread::sleep(SHUTDOWN_RETRY_DELAY),
                );
                if !clean {
                    eprintln!(
                        "rating shutdown left {} update(s) unpersisted after \
                         {SHUTDOWN_FLUSH_ATTEMPTS} attempts",
                        pending.len()
                    );
                }
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_due(&mut pending, &mut latest_sequence_by_owner, db, false);
            }
        }
    }
}

fn flush_due(
    pending: &mut HashMap<PathBuf, Pending>,
    latest_sequence_by_owner: &mut HashMap<PathBuf, u64>,
    db: Option<&Db>,
    all: bool,
) -> bool {
    let now = Instant::now();
    let due: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, p)| all || p.due <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for queued_key in due {
        let Some(mut p) = pending.remove(&queued_key) else {
            continue;
        };
        let mut path = p.path.clone();
        let mut owner = queued_key;
        if !p.owner_resolved {
            let resolved_path = normalize_physical_path(&path);
            match sidecar_owner_key(&resolved_path) {
                Ok(resolved_owner) => {
                    if latest_sequence_by_owner
                        .get(&resolved_owner)
                        .is_some_and(|sequence| *sequence >= p.sequence)
                    {
                        eprintln!(
                            "discarding superseded rating after resolving {}",
                            path.display()
                        );
                        continue;
                    }
                    if let Some(other) = pending.get(&resolved_owner) {
                        if other.sequence >= p.sequence {
                            eprintln!(
                                "discarding superseded rating after resolving {}",
                                path.display()
                            );
                            continue;
                        }
                        pending.remove(&resolved_owner);
                    }
                    latest_sequence_by_owner.insert(resolved_owner.clone(), p.sequence);
                    owner = resolved_owner;
                    path = resolved_path;
                    p.path = path.clone();
                    p.owner_resolved = true;
                }
                Err(error) => {
                    eprintln!(
                        "retaining unjournaled rating for {} because its sidecar owner \
                         remains unresolved: {error}",
                        path.display()
                    );
                    retry_later(pending, owner, p);
                    continue;
                }
            }
        }
        if let Some(db) = db
            && !p.journaled
        {
            let Some(predecessor) = p.journal_predecessor else {
                eprintln!(
                    "retaining unjournaled rating for {} because safe retry ownership \
                     could not be established",
                    path.display()
                );
                retry_later(pending, owner, p);
                continue;
            };
            let result = match predecessor {
                JournalPredecessor::Owner(predecessor) => db
                    .record_rating_pending_sidecar_if_unchanged(
                        &path,
                        p.size,
                        p.mtime_ns,
                        p.rating,
                        predecessor,
                    ),
                JournalPredecessor::Global(predecessor) => db
                    .record_rating_pending_sidecar_if_global_unchanged(
                        &path,
                        p.size,
                        p.mtime_ns,
                        p.rating,
                        predecessor,
                    ),
            };
            match result {
                Ok(true) => {
                    p.journaled = true;
                    p.journal_predecessor = None;
                }
                Ok(false) => {
                    eprintln!(
                        "discarding superseded unjournaled rating for {}",
                        path.display()
                    );
                    continue;
                }
                Err(error) => {
                    eprintln!(
                        "rating database retry failed for {}: {error}",
                        path.display()
                    );
                    retry_later(pending, owner, p);
                    continue;
                }
            }
        }
        if let Some(db) = db {
            match db.synchronize_pending_sidecar(&path, p.size, p.mtime_ns, p.rating, || {
                match write_sidecar_for_identity(&path, p.size, p.mtime_ns, p.rating) {
                    Ok(mtime) => PendingSidecarWrite::Written(mtime),
                    Err(SidecarWriteError::RawReplaced { .. }) => PendingSidecarWrite::Discard,
                    Err(error) => PendingSidecarWrite::Failed(error),
                }
            }) {
                Ok(PendingSidecarSync::Written | PendingSidecarSync::Superseded) => {}
                Ok(PendingSidecarSync::Discarded) => {
                    eprintln!("discarding rating for replaced RAW file {}", path.display());
                }
                Ok(PendingSidecarSync::WriteFailed(error)) => {
                    eprintln!("{error}");
                    retry_later(pending, owner, p);
                }
                Err(error) => {
                    eprintln!(
                        "rating database sync failed for {}: {error}",
                        path.display()
                    );
                    retry_later(pending, owner, p);
                }
            }
        } else {
            match write_sidecar_for_identity(&path, p.size, p.mtime_ns, p.rating) {
                Ok(_) => {}
                Err(SidecarWriteError::RawReplaced { .. }) => {
                    eprintln!("discarding rating for replaced RAW file {}", path.display());
                }
                Err(error) => {
                    eprintln!("{error}");
                    retry_later(pending, owner, p);
                }
            }
        }
    }
    pending.is_empty()
}

#[derive(Debug, thiserror::Error)]
enum SidecarWriteError {
    #[error("RAW identity read failed for {}: {source}", path.display())]
    RawIdentity {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("RAW file was replaced: {}", path.display())]
    RawReplaced { path: PathBuf },
    #[error("{0}")]
    Sidecar(String),
}

fn write_sidecar_for_identity(
    path: &std::path::Path,
    size: u64,
    mtime_ns: i64,
    rating: u8,
) -> Result<i64, SidecarWriteError> {
    match current_raw_identity(path) {
        Ok(identity) if identity == (size, mtime_ns) => {}
        Ok(_) => {
            return Err(SidecarWriteError::RawReplaced {
                path: path.to_path_buf(),
            });
        }
        Err(source) => {
            return Err(SidecarWriteError::RawIdentity {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let sidecar = path.with_extension("xmp");
    xmp::write_rating(&sidecar, rating).map_err(|error| {
        SidecarWriteError::Sidecar(format!(
            "sidecar write failed for {}: {error}",
            sidecar.display()
        ))
    })?;
    std::fs::metadata(&sidecar)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .ok_or_else(|| {
            SidecarWriteError::Sidecar(format!(
                "sidecar metadata read failed for {}",
                sidecar.display()
            ))
        })
}

fn current_raw_identity(path: &std::path::Path) -> std::io::Result<(u64, i64)> {
    let metadata = std::fs::metadata(path)?;
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0);
    Ok((metadata.len(), mtime_ns))
}

fn retry_later(pending: &mut HashMap<PathBuf, Pending>, path: PathBuf, mut item: Pending) {
    item.due = Instant::now() + SIDECAR_RETRY;
    pending.insert(path, item);
}

fn retry_shutdown_flush(mut flush: impl FnMut() -> bool, mut pause: impl FnMut()) -> bool {
    for attempt in 0..SHUTDOWN_FLUSH_ATTEMPTS {
        if flush() {
            return true;
        }
        if attempt + 1 < SHUTDOWN_FLUSH_ATTEMPTS {
            pause();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entry(path: PathBuf) -> FolderEntry {
        std::fs::write(&path, b"raw").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let path = normalize_physical_path(&path);
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        FolderEntry {
            file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            path,
            size: metadata.len(),
            mtime_ns,
        }
    }

    fn sidecar_mtime(entry: &FolderEntry) -> i64 {
        std::fs::metadata(entry.sidecar_path())
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64
    }

    fn put_db_rating(db: &Db, entry: &FolderEntry, rating: Option<u8>, sidecar_mtime_ns: i64) {
        db.upsert_rating_path(
            &entry.path,
            entry.size,
            entry.mtime_ns,
            rating,
            sidecar_mtime_ns,
        )
        .unwrap();
    }

    fn worker_barrier(library: &Library) {
        let (done, wait) = std::sync::mpsc::channel();
        library.tx.send(Cmd::Barrier { done }).unwrap();
        wait.recv_timeout(Duration::from_secs(2))
            .expect("persistence worker must answer a FIFO barrier");
    }

    #[test]
    fn load_ratings_resolves_sidecar_database_and_missing_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let entries = [
            entry(dir.path().join("current-sidecar.ARW")),
            entry(dir.path().join("stale-sidecar.ARW")),
            entry(dir.path().join("database-only.ARW")),
            entry(dir.path().join("sidecar-only.ARW")),
            entry(dir.path().join("invalid-sidecar.ARW")),
            entry(dir.path().join("unrated.ARW")),
        ];
        let db = Db::open_in_memory().unwrap();

        xmp::write_rating(&entries[0].sidecar_path(), 5).unwrap();
        let current_mtime = sidecar_mtime(&entries[0]);
        // Equality is the boundary at which the sidecar wins over the DB.
        put_db_rating(&db, &entries[0], Some(2), current_mtime);

        xmp::write_rating(&entries[1].sidecar_path(), 4).unwrap();
        let stale_mtime = sidecar_mtime(&entries[1]);
        put_db_rating(&db, &entries[1], Some(1), stale_mtime + 1);

        put_db_rating(&db, &entries[2], Some(3), 0);
        xmp::write_rating(&entries[3].sidecar_path(), 0).unwrap();

        std::fs::write(entries[4].sidecar_path(), b"not xml").unwrap();
        put_db_rating(&db, &entries[4], Some(2), sidecar_mtime(&entries[4]));

        assert_eq!(
            load_ratings(&entries, Some(&db)),
            HashMap::from([(0, 5), (1, 1), (2, 3), (3, 0), (4, 2)])
        );
    }

    #[test]
    fn load_ratings_ignores_database_rows_for_a_replaced_raw() {
        let dir = tempfile::tempdir().unwrap();
        let entry = entry(dir.path().join("replaced.ARW"));
        let db = Db::open_in_memory().unwrap();

        xmp::write_rating(&entry.sidecar_path(), 2).unwrap();
        db.record_rating_pending_sidecar_path(
            &entry.path,
            entry.size.saturating_add(1),
            entry.mtime_ns,
            5,
        )
        .unwrap();

        // A dirty rating for an older file at the same path must neither
        // suppress nor replace the current file's sidecar rating.
        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 2)])
        );

        std::fs::remove_file(entry.sidecar_path()).unwrap();
        assert!(load_ratings(&[entry], Some(&db)).is_empty());
    }

    #[test]
    fn flush_waits_for_coalesced_sidecar_and_database_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 1);
        library.set_rating(&entry, 5);
        library.set_rating(&entry, 2);
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(2));
        assert!(!entry.sidecar_path().with_extension("xmp.tmp").exists());
        let db = Db::open(&db_path).unwrap();
        let row = db
            .get_image_path(&entry.path)
            .expect("flush must make the DB row visible");
        assert_eq!(row.rating, Some(2));
        assert!(row.sidecar_mtime_ns > 0);
        assert!(!row.sidecar_dirty);
        assert!(!library.dirty.load(Ordering::Acquire));
    }

    #[test]
    fn flush_without_a_new_rating_is_a_local_noop() {
        let library = Library::start_with(None, Duration::from_secs(60));

        assert!(!library.dirty.load(Ordering::Acquire));
        library.flush();
        assert!(!library.dirty.load(Ordering::Acquire));
    }

    #[test]
    fn worker_barrier_observes_fifo_progress_without_flushing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 4);
        worker_barrier(&library);

        assert!(!entry.sidecar_path().exists());
        let row = Db::open(&db_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(4));
        assert!(row.sidecar_dirty);
    }

    #[test]
    fn requested_flush_preserves_fifo_order_without_a_caller_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 4);
        library.request_flush();
        assert!(!library.dirty.load(Ordering::Acquire));
        worker_barrier(&library);

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        let row = db.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(4));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn requested_flush_failure_restores_dirty_state_for_a_later_retry() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let raw_path = dir.path().join("missing-parent/photo.ARW");
        let raw_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let entry = FolderEntry {
            file_name: "photo.ARW".into(),
            path: normalize_physical_path(&raw_path),
            size: 3,
            mtime_ns: raw_mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
        };
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 3);
        library.request_flush();
        worker_barrier(&library);
        assert!(library.dirty.load(Ordering::Acquire));
        let db = Db::open(&db_path).unwrap();
        assert!(
            db.get_image_path(&entry.path).is_none(),
            "an unresolved owner must not be journaled under a guessed key"
        );
        drop(db);

        std::fs::create_dir_all(entry.path.parent().unwrap()).unwrap();
        std::fs::write(&entry.path, b"raw").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&entry.path)
            .unwrap()
            .set_modified(raw_mtime)
            .unwrap();

        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(3));
        assert!(!library.dirty.load(Ordering::Acquire));
    }

    #[test]
    fn failed_initial_journal_write_cannot_leave_an_older_dirty_rating_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&library);

        let setup = Db::open(&db_path).unwrap();
        setup
            .record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 1)
            .unwrap();
        drop(setup);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_dirty_rating
                 BEFORE INSERT ON images
                 WHEN NEW.sidecar_dirty = 1
                 BEGIN
                   SELECT RAISE(FAIL, 'injected journal failure');
                 END;
                 CREATE TRIGGER reject_dirty_rating_update
                 BEFORE UPDATE ON images
                 WHEN NEW.sidecar_dirty = 1 AND NEW.rating = 5
                 BEGIN
                   SELECT RAISE(FAIL, 'injected journal retry failure');
                 END;",
            )
            .unwrap();

        library.set_rating(&entry, 5);
        library.flush();

        assert!(library.dirty.load(Ordering::Acquire));
        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(1));
        let row = Db::open(&db_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(1));
        assert!(row.sidecar_dirty);

        connection
            .execute_batch(
                "DROP TRIGGER reject_dirty_rating;
                 DROP TRIGGER reject_dirty_rating_update;",
            )
            .unwrap();
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        assert!(!library.dirty.load(Ordering::Acquire));
        let row = Db::open(&db_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn failed_initial_journal_retry_cannot_overwrite_a_newer_completed_rating() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let initial_sidecar_mtime = sidecar_mtime(&entry);
        {
            let db = Db::open(&db_path).unwrap();
            put_db_rating(&db, &entry, Some(1), initial_sidecar_mtime);
        }

        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&library);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_initial_dirty_rating
                 BEFORE INSERT ON images
                 WHEN NEW.sidecar_dirty = 1
                 BEGIN
                   SELECT RAISE(FAIL, 'injected initial journal failure');
                 END;",
            )
            .unwrap();

        library.set_rating(&entry, 2);
        worker_barrier(&library);
        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(1));
        connection
            .execute_batch("DROP TRIGGER reject_initial_dirty_rating;")
            .unwrap();

        let newer = Db::open(&db_path).unwrap();
        newer
            .record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 5)
            .unwrap();
        assert!(matches!(
            newer
                .synchronize_pending_sidecar(&entry.path, entry.size, entry.mtime_ns, 5, || {
                    match write_sidecar_for_identity(&entry.path, entry.size, entry.mtime_ns, 5) {
                        Ok(mtime) => PendingSidecarWrite::Written(mtime),
                        Err(error) => PendingSidecarWrite::Failed(error),
                    }
                },)
                .unwrap(),
            PendingSidecarSync::Written
        ));

        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        assert!(!library.dirty.load(Ordering::Acquire));
        let row = newer.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[cfg(unix)]
    #[test]
    fn physical_path_aliases_share_one_rating_publication_owner() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("physical");
        let alias_dir = dir.path().join("alias");
        std::fs::create_dir(&physical_dir).unwrap();
        symlink(&physical_dir, &alias_dir).unwrap();
        let direct = entry(physical_dir.join("photo.ARW"));
        let aliased = FolderEntry {
            file_name: direct.file_name.clone(),
            path: alias_dir.join("photo.ARW"),
            size: direct.size,
            mtime_ns: direct.mtime_ns,
        };
        let canonical_path = normalize_physical_path(&direct.path);
        assert_eq!(
            normalize_physical_path(&aliased.path),
            canonical_path,
            "the test aliases must resolve to one physical ownership key"
        );
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        let newer = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);
        worker_barrier(&newer);

        older.set_rating(&aliased, 1);
        worker_barrier(&older);
        newer.set_rating(&direct, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&direct.sidecar_path()), Some(5));
        let db = Db::open(&db_path).unwrap();
        let row = db.get_image_path(&canonical_path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*)
                       FROM images
                      WHERE sidecar_quarantined = 0",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn case_aliases_share_one_rating_publication_owner() {
        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("CaseAlias");
        let alias_dir = dir.path().join("casealias");
        std::fs::create_dir(&physical_dir).unwrap();
        let direct = entry(physical_dir.join("Photo.ARW"));
        let Ok(alias_root) = std::fs::canonicalize(&alias_dir) else {
            // A supported installation can use a case-sensitive volume.
            return;
        };
        assert_eq!(
            alias_root,
            std::fs::canonicalize(&physical_dir).unwrap(),
            "the test paths must be filesystem aliases"
        );
        let aliased = FolderEntry {
            file_name: direct.file_name.clone(),
            path: alias_dir.join("Photo.ARW"),
            size: direct.size,
            mtime_ns: direct.mtime_ns,
        };
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        let newer = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);
        worker_barrier(&newer);

        older.set_rating(&aliased, 1);
        worker_barrier(&older);
        newer.set_rating(&direct, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&direct.sidecar_path()), Some(5));
        assert_eq!(
            Db::open(&db_path)
                .unwrap()
                .get_image_path(&direct.path)
                .unwrap()
                .rating,
            Some(5)
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn case_variant_stems_sharing_one_xmp_have_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let upper = entry(dir.path().join("Photo.ARW"));
        let lower = entry(dir.path().join("photo.DNG"));
        std::fs::write(upper.sidecar_path(), b"probe").unwrap();
        if !lower.sidecar_path().exists() {
            // A supported installation can use a case-sensitive volume.
            std::fs::remove_file(upper.sidecar_path()).unwrap();
            return;
        }
        std::fs::remove_file(upper.sidecar_path()).unwrap();
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        let newer = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);
        worker_barrier(&newer);

        older.set_rating(&upper, 1);
        worker_barrier(&older);
        newer.set_rating(&lower, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&upper.sidecar_path()), Some(5));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image_path(&upper.path).is_none());
        assert_eq!(db.get_image_path(&lower.path).unwrap().rating, Some(5));
    }

    #[test]
    fn raw_files_with_one_xmp_target_share_publication_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let arw = entry(dir.path().join("photo.ARW"));
        let dng = entry(dir.path().join("photo.DNG"));
        assert_eq!(arw.sidecar_path(), dng.sidecar_path());
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        let newer = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);
        worker_barrier(&newer);

        older.set_rating(&arw, 1);
        worker_barrier(&older);
        newer.set_rating(&dng, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&arw.sidecar_path()), Some(5));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image_path(&arw.path).is_none());
        let row = db.get_image_path(&dng.path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn database_free_fallback_coalesces_one_xmp_target() {
        let dir = tempfile::tempdir().unwrap();
        let arw = entry(dir.path().join("photo.ARW"));
        let dng = entry(dir.path().join("photo.DNG"));
        let library = Library::start_with(None, Duration::from_secs(60));

        library.set_rating(&arw, 1);
        worker_barrier(&library);
        library.set_rating(&dng, 5);
        library.flush();

        assert_eq!(xmp::read_rating(&arw.sidecar_path()), Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_alias_cannot_publish_after_a_newer_database_rating() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("physical");
        let unresolved_alias = dir.path().join("alias");
        std::fs::create_dir(&physical_dir).unwrap();
        let direct = entry(physical_dir.join("photo.ARW"));
        let older_entry = FolderEntry {
            path: unresolved_alias.join("photo.ARW"),
            file_name: direct.file_name.clone(),
            size: direct.size,
            mtime_ns: direct.mtime_ns,
        };
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);

        older.set_rating(&older_entry, 1);
        worker_barrier(&older);
        symlink(&physical_dir, &unresolved_alias).unwrap();

        let newer = Library::start_with(Some(db_path), Duration::from_secs(60));
        worker_barrier(&newer);
        newer.set_rating(&direct, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&direct.sidecar_path()), Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_owner_survives_an_unrelated_database_rating() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("physical");
        let unresolved_alias = dir.path().join("alias");
        std::fs::create_dir(&physical_dir).unwrap();
        let eventual = entry(physical_dir.join("eventual.ARW"));
        let unrelated = entry(physical_dir.join("unrelated.ARW"));
        let unresolved_entry = FolderEntry {
            path: unresolved_alias.join("eventual.ARW"),
            file_name: eventual.file_name.clone(),
            size: eventual.size,
            mtime_ns: eventual.mtime_ns,
        };
        let db_path = dir.path().join("viewr.db");
        let library = Library::start_with(Some(db_path), Duration::from_secs(60));
        worker_barrier(&library);

        library.set_rating(&unresolved_entry, 1);
        worker_barrier(&library);
        library.set_rating(&unrelated, 5);
        library.flush();
        assert_eq!(xmp::read_rating(&unrelated.sidecar_path()), Some(5));

        symlink(&physical_dir, &unresolved_alias).unwrap();
        library.flush();

        assert_eq!(xmp::read_rating(&eventual.sidecar_path()), Some(1));
        assert_eq!(xmp::read_rating(&unrelated.sidecar_path()), Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn database_free_recovery_cannot_replay_before_a_newer_published_rating() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("physical");
        let unresolved_alias = dir.path().join("alias");
        std::fs::create_dir(&physical_dir).unwrap();
        let direct = entry(physical_dir.join("photo.ARW"));
        let older_entry = FolderEntry {
            path: unresolved_alias.join("photo.ARW"),
            file_name: direct.file_name.clone(),
            size: direct.size,
            mtime_ns: direct.mtime_ns,
        };
        let library = Library::start_with(None, Duration::from_secs(60));

        library.set_rating(&older_entry, 1);
        worker_barrier(&library);
        library.set_rating(&direct, 5);
        library.flush();
        assert_eq!(xmp::read_rating(&direct.sidecar_path()), Some(5));

        symlink(&physical_dir, &unresolved_alias).unwrap();
        library.flush();

        assert_eq!(xmp::read_rating(&direct.sidecar_path()), Some(5));
    }

    #[test]
    fn dirty_owner_rating_wins_for_every_raw_sharing_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let arw = entry(dir.path().join("photo.ARW"));
        let dng = entry(dir.path().join("photo.DNG"));
        xmp::write_rating(&arw.sidecar_path(), 2).unwrap();
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_canonical(&dng.path, dng.size, dng.mtime_ns, 5)
            .unwrap();

        assert_eq!(
            load_ratings(&[arw, dng], Some(&db)),
            HashMap::from([(0, 5), (1, 5)])
        );
    }

    #[test]
    fn failed_journal_retry_cannot_reclaim_a_newer_xmp_owner() {
        let dir = tempfile::tempdir().unwrap();
        let arw = entry(dir.path().join("photo.ARW"));
        let dng = entry(dir.path().join("photo.DNG"));
        let db_path = dir.path().join("viewr.db");
        let older = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&older);
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_initial_owner_rating
                 BEFORE INSERT ON images
                 WHEN NEW.sidecar_dirty = 1
                 BEGIN
                   SELECT RAISE(FAIL, 'injected initial owner journal failure');
                 END;",
            )
            .unwrap();

        older.set_rating(&arw, 1);
        worker_barrier(&older);
        connection
            .execute_batch("DROP TRIGGER reject_initial_owner_rating;")
            .unwrap();

        let newer = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&newer);
        newer.set_rating(&dng, 5);
        newer.flush();
        older.flush();

        assert_eq!(xmp::read_rating(&arw.sidecar_path()), Some(5));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image_path(&arw.path).is_none());
        assert_eq!(db.get_image_path(&dng.path).unwrap().rating, Some(5));
    }

    #[cfg(unix)]
    #[test]
    fn conflicting_legacy_aliases_do_not_publish_until_the_user_rates_again() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let physical_dir = dir.path().join("physical");
        let alias_dir = dir.path().join("alias");
        std::fs::create_dir(&physical_dir).unwrap();
        symlink(&physical_dir, &alias_dir).unwrap();
        let entry = entry(physical_dir.join("photo.ARW"));
        let aliased_path = alias_dir.join("photo.ARW");
        xmp::write_rating(&entry.sidecar_path(), 2).unwrap();
        let db_path = dir.path().join("viewr.db");
        let legacy = rusqlite::Connection::open(&db_path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE images (
                    path TEXT PRIMARY KEY,
                    size INTEGER NOT NULL,
                    mtime_ns INTEGER NOT NULL,
                    rating INTEGER,
                    sidecar_mtime_ns INTEGER NOT NULL DEFAULT 0,
                    sidecar_dirty INTEGER NOT NULL DEFAULT 0,
                    last_seen INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![
                    aliased_path.to_str().unwrap(),
                    entry.size,
                    entry.mtime_ns,
                    1
                ],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![entry.path.to_str().unwrap(), entry.size, entry.mtime_ns, 5],
            )
            .unwrap();
        drop(legacy);
        let db = Db::open(&db_path).unwrap();

        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 2)])
        );
        drop(db);

        let library = Library::start_with(Some(db_path.clone()), Duration::from_millis(10));
        worker_barrier(&library);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(2));

        library.set_rating(&entry, 4);
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image_path(&aliased_path).is_none());
        assert_eq!(db.get_image_path(&entry.path).unwrap().rating, Some(4));
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*)
                       FROM images
                      WHERE sidecar_quarantined = 0",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn completed_predecessor_does_not_discard_a_delayed_newer_rating() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
        worker_barrier(&library);

        let predecessor = Db::open(&db_path).unwrap();
        predecessor
            .record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 1)
            .unwrap();
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_new_rating_once
                 BEFORE INSERT ON images
                 WHEN NEW.sidecar_dirty = 1
                 BEGIN
                   SELECT RAISE(FAIL, 'injected initial journal failure');
                 END;",
            )
            .unwrap();

        library.set_rating(&entry, 5);
        worker_barrier(&library);
        connection
            .execute_batch("DROP TRIGGER reject_new_rating_once;")
            .unwrap();
        assert!(
            predecessor
                .complete_pending_sidecar(
                    &entry.path,
                    entry.size,
                    entry.mtime_ns,
                    1,
                    sidecar_mtime(&entry),
                )
                .unwrap()
        );

        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        assert!(!library.dirty.load(Ordering::Acquire));
        let row = predecessor.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn superseded_recovery_cannot_publish_after_a_newer_rating_completed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        let older = Db::open(&db_path).unwrap();
        let newer = Db::open(&db_path).unwrap();

        older
            .record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 1)
            .unwrap();
        newer
            .record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 5)
            .unwrap();
        xmp::write_rating(&entry.sidecar_path(), 5).unwrap();
        assert!(
            newer
                .complete_pending_sidecar(
                    &entry.path,
                    entry.size,
                    entry.mtime_ns,
                    5,
                    sidecar_mtime(&entry),
                )
                .unwrap()
        );

        let mut pending = HashMap::from([(
            entry.sidecar_path(),
            Pending {
                path: entry.path.clone(),
                owner_resolved: true,
                sequence: 0,
                size: entry.size,
                mtime_ns: entry.mtime_ns,
                rating: 1,
                journaled: true,
                journal_predecessor: None,
                due: Instant::now(),
            },
        )]);
        let mut latest_sequence_by_owner = HashMap::from([(entry.sidecar_path(), 0)]);
        assert!(flush_due(
            &mut pending,
            &mut latest_sequence_by_owner,
            Some(&older),
            true
        ));

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        let row = newer.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn drop_flushes_pending_rating_and_joins_worker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));

        {
            let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
            library.set_rating(&entry, 4);
        }

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.get_image_path(&entry.path).unwrap().rating, Some(4));
        assert!(!db.get_image_path(&entry.path).unwrap().sidecar_dirty);
    }

    #[test]
    fn dirty_database_rating_wins_after_a_crash_before_sidecar_flush() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 2).unwrap();
        let original_sidecar_mtime = sidecar_mtime(&entry);

        {
            let db = Db::open(&db_path).unwrap();
            put_db_rating(&db, &entry, Some(2), original_sidecar_mtime);
            db.record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 5)
                .unwrap();
        }

        let db = Db::open(&db_path).unwrap();
        let row = db.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(5));
        assert_eq!(row.sidecar_mtime_ns, original_sidecar_mtime);
        assert!(row.sidecar_dirty);
        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(2));
        assert_eq!(load_ratings(&[entry], Some(&db)), HashMap::from([(0, 5)]));
    }

    #[test]
    fn startup_recovers_a_sidecar_left_dirty_by_a_prior_process() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            put_db_rating(&db, &entry, Some(1), sidecar_mtime(&entry));
            db.record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 4)
                .unwrap();
        }

        {
            let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
            drop(library);
        }

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        let row = db.get_image_path(&entry.path).unwrap();
        assert_eq!(row.rating, Some(4));
        assert!(!row.sidecar_dirty);
        assert!(db.pending_sidecars().unwrap().is_empty());
    }

    #[test]
    fn startup_discards_a_pending_rating_when_the_raw_was_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            db.record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 5)
                .unwrap();
        }
        std::fs::write(&entry.path, b"a different raw payload").unwrap();

        {
            let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
            drop(library);
        }

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(1));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image_path(&entry.path).is_none());
        assert!(db.pending_sidecars().unwrap().is_empty());
    }

    #[test]
    fn raw_identity_is_revalidated_after_database_ownership_is_acquired() {
        let dir = tempfile::tempdir().unwrap();
        let entry = entry(dir.path().join("photo.ARW"));
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&entry.path, entry.size, entry.mtime_ns, 5)
            .unwrap();

        let result = db
            .synchronize_pending_sidecar(&entry.path, entry.size, entry.mtime_ns, 5, || {
                std::fs::write(&entry.path, b"a replacement RAW payload").unwrap();
                match write_sidecar_for_identity(&entry.path, entry.size, entry.mtime_ns, 5) {
                    Ok(mtime) => PendingSidecarWrite::Written(mtime),
                    Err(SidecarWriteError::RawReplaced { .. }) => PendingSidecarWrite::Discard,
                    Err(error) => PendingSidecarWrite::Failed(error),
                }
            })
            .unwrap();

        assert!(matches!(result, PendingSidecarSync::Discarded));
        assert!(!entry.sidecar_path().exists());
        assert!(db.get_image_path(&entry.path).is_none());
    }

    #[test]
    fn failed_flush_remains_dirty_and_retries_after_the_path_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let raw_path = dir.path().join("missing-parent/photo.ARW");
        let raw_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let entry = FolderEntry {
            file_name: "photo.ARW".into(),
            path: normalize_physical_path(&raw_path),
            size: 3,
            mtime_ns: raw_mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
        };
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 3);
        library.flush();

        assert!(library.dirty.load(Ordering::Acquire));
        assert!(!entry.sidecar_path().exists());
        let db = Db::open(&db_path).unwrap();
        assert!(
            db.get_image_path(&entry.path).is_none(),
            "an unresolved owner must not be journaled under a guessed key"
        );
        drop(db);

        std::fs::create_dir_all(entry.path.parent().unwrap()).unwrap();
        std::fs::write(&entry.path, b"raw").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&entry.path)
            .unwrap()
            .set_modified(raw_mtime)
            .unwrap();
        assert_eq!(
            current_raw_identity(&entry.path).unwrap(),
            (entry.size, entry.mtime_ns)
        );
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(3));
        assert!(!library.dirty.load(Ordering::Acquire));
        let db = Db::open(&db_path).unwrap();
        assert!(!db.get_image_path(&entry.path).unwrap().sidecar_dirty);
    }

    #[test]
    fn shutdown_flush_retry_is_bounded_and_stops_after_success() {
        let attempts = std::cell::Cell::new(0);
        let pauses = std::cell::Cell::new(0);
        assert!(retry_shutdown_flush(
            || {
                attempts.set(attempts.get() + 1);
                attempts.get() == SHUTDOWN_FLUSH_ATTEMPTS
            },
            || pauses.set(pauses.get() + 1),
        ));
        assert_eq!(attempts.get(), SHUTDOWN_FLUSH_ATTEMPTS);
        assert_eq!(pauses.get(), SHUTDOWN_FLUSH_ATTEMPTS - 1);

        attempts.set(0);
        pauses.set(0);
        assert!(!retry_shutdown_flush(
            || {
                attempts.set(attempts.get() + 1);
                false
            },
            || pauses.set(pauses.get() + 1),
        ));
        assert_eq!(attempts.get(), SHUTDOWN_FLUSH_ATTEMPTS);
        assert_eq!(pauses.get(), SHUTDOWN_FLUSH_ATTEMPTS - 1);
    }
}
