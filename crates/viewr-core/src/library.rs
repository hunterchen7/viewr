//! Ratings orchestration: load precedence (sidecar > DB > embedded),
//! and a persist thread that writes the DB immediately and debounces
//! sidecar writes (~400ms per image, coalescing re-rating churn).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
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
    Flush,
    Shutdown,
}

pub struct Library {
    tx: Sender<Cmd>,
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
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || persist_thread(&rx));
        Self { tx }
    }

    pub fn set_rating(&self, entry: &FolderEntry, rating: u8) {
        let _ = self.tx.send(Cmd::SetRating {
            path: entry.path.clone(),
            size: entry.size,
            mtime_ns: entry.mtime_ns,
            rating,
        });
    }

    /// Force pending sidecar writes out now (navigate-away, quit).
    pub fn flush(&self) {
        let _ = self.tx.send(Cmd::Flush);
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

struct Pending {
    size: u64,
    mtime_ns: i64,
    rating: u8,
    due: Instant,
}

fn persist_thread(rx: &Receiver<Cmd>) {
    let db = default_db_path().and_then(|p| Db::open(&p).ok());
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
                        due: Instant::now() + SIDECAR_DEBOUNCE,
                    },
                );
            }
            Ok(Cmd::Flush) => flush_due(&mut pending, db.as_ref(), true),
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                flush_due(&mut pending, db.as_ref(), true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => flush_due(&mut pending, db.as_ref(), false),
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
