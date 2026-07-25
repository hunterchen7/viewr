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

use crate::db::{Db, default_db_path};
use crate::folder::FolderEntry;
use crate::xmp;

const SIDECAR_DEBOUNCE: Duration = Duration::from_millis(400);
const SIDECAR_RETRY: Duration = Duration::from_secs(5);

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
    Shutdown,
}

/// Asynchronous rating persistence service.
///
/// [`set_rating`](Self::set_rating) queues the update on a dedicated thread,
/// which attempts to journal it and coalesces repeated changes to the same RAW
/// before writing its XMP sidecar. The optional SQLite database is an
/// accelerator and crash-recovery journal; sidecars remain the interoperable
/// source of truth. Dropping the service makes one best-effort flush attempt
/// and joins its worker. A failed sidecar remains recoverable on restart only
/// if its dirty database journal write succeeded.
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
    let mut out = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let sidecar = entry.sidecar_path();
        let sidecar_mtime = std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        let row = db
            .and_then(|db| db.get_image(&entry.path))
            .filter(|row| row.size == entry.size && row.mtime_ns == entry.mtime_ns);

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
    out
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
            let db = db_path.and_then(|path| Db::open(&path).ok());
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
    /// coalesce. Values are not range-checked; the UI convention is `0..=5`,
    /// where zero means unrated. If the worker has already stopped, the update
    /// is silently ignored.
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
    /// remains the blocking durability boundary.
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
    size: u64,
    mtime_ns: i64,
    rating: u8,
    due: Instant,
}

fn persist_thread(rx: &Receiver<Cmd>, db: Option<&Db>, debounce: Duration, dirty: &AtomicBool) {
    let mut pending: HashMap<PathBuf, Pending> = db
        .map(|db| match db.pending_sidecars() {
            Ok(rows) => rows
                .into_iter()
                .map(|row| {
                    (
                        row.path,
                        Pending {
                            size: row.size,
                            mtime_ns: row.mtime_ns,
                            rating: row.rating,
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
                // Commit the rating and dirty marker together before the
                // debounced sidecar write. A restart can then recover both the
                // rating precedence and the unfinished sidecar operation.
                if let Some(db) = &db
                    && let Err(error) =
                        db.record_rating_pending_sidecar(&path, size, mtime_ns, rating)
                {
                    eprintln!(
                        "rating database write failed for {}: {error}",
                        path.display()
                    );
                }
                pending.insert(
                    path,
                    Pending {
                        size,
                        mtime_ns,
                        rating,
                        due: Instant::now() + debounce,
                    },
                );
            }
            Ok(Cmd::Flush { done }) => {
                let clean = flush_due(&mut pending, db, true);
                if !clean {
                    dirty.store(true, Ordering::Release);
                }
                if let Some(done) = done {
                    let _ = done.send(clean);
                }
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                flush_due(&mut pending, db, true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_due(&mut pending, db, false);
            }
        }
    }
}

fn flush_due(pending: &mut HashMap<PathBuf, Pending>, db: Option<&Db>, all: bool) -> bool {
    let now = Instant::now();
    let due: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, p)| all || p.due <= now)
        .map(|(k, _)| k.clone())
        .collect();
    for path in due {
        let Some(p) = pending.remove(&path) else {
            continue;
        };
        match current_raw_identity(&path) {
            Ok(identity) if identity == (p.size, p.mtime_ns) => {}
            Ok(_) => {
                if let Some(db) = db
                    && let Err(error) =
                        db.discard_pending_sidecar(&path, p.size, p.mtime_ns, p.rating)
                {
                    eprintln!(
                        "stale rating database cleanup failed for {}: {error}",
                        path.display()
                    );
                    retry_later(pending, path, p);
                    continue;
                }
                eprintln!("discarding rating for replaced RAW file {}", path.display());
                continue;
            }
            Err(error) => {
                eprintln!("RAW identity read failed for {}: {error}", path.display());
                retry_later(pending, path, p);
                continue;
            }
        }
        let sidecar = path.with_extension("xmp");
        if let Err(e) = xmp::write_rating(&sidecar, p.rating) {
            eprintln!("sidecar write failed for {}: {e}", sidecar.display());
            retry_later(pending, path, p);
            continue;
        }
        // Record the sidecar mtime we just produced so external edits
        // (which will be newer) win on next load.
        let mtime = std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        let Some(mtime) = mtime else {
            eprintln!("sidecar metadata read failed for {}", sidecar.display());
            retry_later(pending, path, p);
            continue;
        };
        if let Some(db) = db {
            match db.complete_pending_sidecar(&path, p.size, p.mtime_ns, p.rating, mtime) {
                Ok(true) => {}
                Ok(false) => {
                    // A newer journal write won the path. It remains dirty and
                    // must not be cleared by this older completion.
                }
                Err(error) => {
                    eprintln!(
                        "rating database sync failed for {}: {error}",
                        path.display()
                    );
                    retry_later(pending, path, p);
                }
            }
        }
    }
    pending.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entry(path: PathBuf) -> FolderEntry {
        std::fs::write(&path, b"raw").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
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
        db.upsert_rating(
            &entry.path,
            entry.size,
            entry.mtime_ns,
            rating,
            sidecar_mtime_ns,
        )
        .unwrap();
    }

    fn flush_barrier(library: &Library) -> bool {
        let (done, wait) = std::sync::mpsc::channel();
        library.tx.send(Cmd::Flush { done: Some(done) }).unwrap();
        wait.recv_timeout(Duration::from_secs(2))
            .expect("persistence worker must answer a FIFO flush barrier")
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
        db.record_rating_pending_sidecar(
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
            .get_image(&entry.path)
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
    fn requested_flush_preserves_fifo_order_without_a_caller_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let entry = entry(dir.path().join("photo.ARW"));
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 4);
        library.request_flush();
        assert!(!library.dirty.load(Ordering::Acquire));
        assert!(flush_barrier(&library));

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        let row = db.get_image(&entry.path).unwrap();
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
            path: raw_path,
            size: 3,
            mtime_ns: raw_mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64,
        };
        let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 3);
        library.request_flush();
        assert!(!flush_barrier(&library));
        assert!(library.dirty.load(Ordering::Acquire));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image(&entry.path).unwrap().sidecar_dirty);
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
        assert_eq!(db.get_image(&entry.path).unwrap().rating, Some(4));
        assert!(!db.get_image(&entry.path).unwrap().sidecar_dirty);
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
            db.record_rating_pending_sidecar(&entry.path, entry.size, entry.mtime_ns, 5)
                .unwrap();
        }

        let db = Db::open(&db_path).unwrap();
        let row = db.get_image(&entry.path).unwrap();
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
            db.record_rating_pending_sidecar(&entry.path, entry.size, entry.mtime_ns, 4)
                .unwrap();
        }

        {
            let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
            drop(library);
        }

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let db = Db::open(&db_path).unwrap();
        let row = db.get_image(&entry.path).unwrap();
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
            db.record_rating_pending_sidecar(&entry.path, entry.size, entry.mtime_ns, 5)
                .unwrap();
        }
        std::fs::write(&entry.path, b"a different raw payload").unwrap();

        {
            let library = Library::start_with(Some(db_path.clone()), Duration::from_secs(60));
            drop(library);
        }

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(1));
        let db = Db::open(&db_path).unwrap();
        assert!(db.get_image(&entry.path).is_none());
        assert!(db.pending_sidecars().unwrap().is_empty());
    }

    #[test]
    fn failed_flush_remains_dirty_and_retries_after_the_path_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("viewr.db");
        let raw_path = dir.path().join("missing-parent/photo.ARW");
        let raw_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let entry = FolderEntry {
            file_name: "photo.ARW".into(),
            path: raw_path,
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
        let row = db.get_image(&entry.path).unwrap();
        assert_eq!(row.rating, Some(3));
        assert!(row.sidecar_dirty);
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
        assert!(!db.get_image(&entry.path).unwrap().sidecar_dirty);
    }
}
