//! Ratings orchestration: load precedence (sidecar > DB > embedded),
//! and a persist thread that writes the DB immediately and debounces
//! sidecar writes (~400ms per image, coalescing re-rating churn).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::db::{Db, default_db_path};
use crate::folder::FolderEntry;
use crate::xmp;

const SIDECAR_DEBOUNCE: Duration = Duration::from_millis(400);

enum Cmd {
    SetRating {
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
        rating: u8,
    },
    Flush {
        done: Sender<()>,
    },
    Shutdown,
}

pub struct Library {
    tx: Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
    /// Avoid a channel allocation and worker round-trip on ordinary
    /// navigation when no rating has changed since the last flush.
    dirty: AtomicBool,
}

/// Initial per-index ratings resolved with full precedence. Embedded
/// camera ratings arrive later via thumb metadata; apply them only where
/// this map has no entry.
pub fn load_ratings(entries: &[FolderEntry], db: Option<&Db>) -> HashMap<usize, u8> {
    let mut out = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let sidecar = entry.sidecar_path();
        let sidecar_mtime = std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);
        let row = db.and_then(|db| db.get_image(&entry.path.to_string_lossy()));

        // Sidecar wins when it exists and is at least as new as our record.
        let from_sidecar = sidecar_mtime.and_then(|mt| {
            let db_mt = row.as_ref().map_or(0, |r| r.sidecar_mtime_ns);
            if mt >= db_mt {
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
    /// Spawn the persist thread. Uses the default DB path (best-effort:
    /// rating persistence degrades gracefully without a DB).
    pub fn start() -> Self {
        Self::start_with(default_db_path(), SIDECAR_DEBOUNCE)
    }

    fn start_with(db_path: Option<PathBuf>, debounce: Duration) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let db = db_path.and_then(|path| Db::open(&path).ok());
            persist_thread(&rx, db.as_ref(), debounce);
        });
        Self {
            tx,
            worker: Some(worker),
            dirty: AtomicBool::new(false),
        }
    }

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

    /// Force pending sidecar writes out now (navigate-away, quit), waiting
    /// until both the sidecar and DB update have completed.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let (done, wait) = std::sync::mpsc::channel();
        if self.tx.send(Cmd::Flush { done }).is_ok() {
            let _ = wait.recv();
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

fn persist_thread(rx: &Receiver<Cmd>, db: Option<&Db>, debounce: Duration) {
    let mut pending: HashMap<PathBuf, Pending> = HashMap::new();

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
                // DB immediately (crash-safe), sidecar debounced. Preserve
                // the previously-known sidecar mtime so a crash before the
                // sidecar write doesn't hand precedence to a stale sidecar.
                if let Some(db) = &db {
                    let key = path.to_string_lossy().into_owned();
                    let prior_mtime = db.get_image(&key).map_or(0, |r| r.sidecar_mtime_ns);
                    let _ = db.upsert_rating(&key, size, mtime_ns, Some(rating), prior_mtime);
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
                flush_due(&mut pending, db, true);
                let _ = done.send(());
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                flush_due(&mut pending, db, true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => flush_due(&mut pending, db, false),
        }
    }
}

fn flush_due(pending: &mut HashMap<PathBuf, Pending>, db: Option<&Db>, all: bool) {
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
        let sidecar = path.with_extension("xmp");
        if let Err(e) = xmp::write_rating(&sidecar, p.rating) {
            eprintln!("sidecar write failed for {}: {e}", sidecar.display());
            continue;
        }
        // Record the sidecar mtime we just produced so external edits
        // (which will be newer) win on next load.
        let mtime = std::fs::metadata(&sidecar)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        if let Some(db) = db {
            let _ = db.upsert_rating(
                &path.to_string_lossy(),
                p.size,
                p.mtime_ns,
                Some(p.rating),
                mtime,
            );
        }
    }
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
            &entry.path.to_string_lossy(),
            entry.size,
            entry.mtime_ns,
            rating,
            sidecar_mtime_ns,
        )
        .unwrap();
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
            .get_image(&entry.path.to_string_lossy())
            .expect("flush must make the DB row visible");
        assert_eq!(row.rating, Some(2));
        assert!(row.sidecar_mtime_ns > 0);
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
        assert_eq!(
            db.get_image(&entry.path.to_string_lossy()).unwrap().rating,
            Some(4)
        );
    }
}
