//! Ratings orchestration and best-effort persistence.
//!
//! An unfinished dirty database journal entry remains authoritative regardless
//! of sidecar modification time until it is flushed. Otherwise, a current
//! sidecar wins over the clean database row.
//! Embedded ratings arrive later through the metadata wave and fill only
//! entries still missing a rating. The persistence thread attempts to journal
//! updates before debouncing XMP sidecar writes (~400 ms per image).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::db::{
    Db, DbError, PendingSidecarSync, PendingSidecarWrite, RatingGlobalSnapshot,
    RatingOwnerSnapshot, configured_db_path,
};
use crate::folder::{
    FolderEntry, normalize_physical_path, raw_path_from_sidecar_owner,
    sidecar_owner_collision_token, sidecar_owner_key, sidecar_owner_keys,
};
use crate::xmp;

const SIDECAR_DEBOUNCE: Duration = Duration::from_millis(400);
const SIDECAR_RETRY: Duration = Duration::from_secs(5);
const SHUTDOWN_FLUSH_ATTEMPTS: usize = 3;
const SHUTDOWN_RETRY_DELAY: Duration = Duration::from_millis(50);
const DB_OPEN_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(50);
const DB_OPEN_LOCK_RETRY: Duration = Duration::from_millis(50);
const DB_OPEN_OTHER_RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
enum ConfiguredDbOpenError {
    #[error("cannot create database directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Database(#[from] DbError),
}

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
/// before writing its XMP sidecar. When a platform database path is configured,
/// SQLite is the publication authority: updates wait for a successful journal
/// write instead of bypassing an unavailable database. Systems without a
/// platform configuration directory use explicit database-free XMP
/// persistence. Sidecars remain the interoperable representation. Dropping
/// the service makes a bounded sequence of best-effort flush attempts and
/// joins its worker. A failed sidecar remains recoverable on restart only if
/// its dirty database journal write succeeded.
pub struct Library {
    tx: Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
    /// Avoid a channel allocation and worker round-trip on ordinary
    /// navigation when no rating has changed since the last flush.
    dirty: Arc<AtomicBool>,
    database_configured: bool,
    database_ready: Arc<AtomicBool>,
}

/// Resolved ratings and the physical sidecar owner for each input entry.
#[doc(hidden)]
pub type RatingLoad = (HashMap<usize, u8>, Vec<Option<PathBuf>>);

/// Resolves only physical sidecar owners, without reading XMP or SQLite.
#[doc(hidden)]
pub fn rating_owner_keys(entries: &[FolderEntry]) -> Vec<Option<PathBuf>> {
    sidecar_owner_keys(entries)
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
pub fn load_ratings_with_owners(entries: &[FolderEntry], db: Option<&Db>) -> RatingLoad {
    try_load_ratings_with_owners(entries, db).unwrap_or_else(|_| {
        resolve_rating_snapshot(
            entries,
            rating_owner_keys(entries),
            crate::db::RatingSnapshot::default(),
        )
    })
}

/// Resolves initial ratings without suppressing a database snapshot failure.
///
/// Callers that must retry an unavailable recovery journal can use this
/// variant instead of the best-effort [`load_ratings_with_owners`].
///
/// # Errors
///
/// Returns [`DbError`] when reading a supplied database snapshot fails.
#[doc(hidden)]
pub fn try_load_ratings_with_owners(
    entries: &[FolderEntry],
    db: Option<&Db>,
) -> Result<RatingLoad, DbError> {
    let owners = rating_owner_keys(entries);
    let snapshot = match db {
        Some(db) => {
            db.rating_snapshot(entries.iter().map(|entry| entry.path.as_path()), &owners)?
        }
        None => crate::db::RatingSnapshot::default(),
    };
    Ok(resolve_rating_snapshot(entries, owners, snapshot))
}

/// Runs the pre-optimization global legacy scan for comparative benchmarks.
#[cfg(feature = "benchmarks")]
#[doc(hidden)]
pub fn benchmark_load_ratings_legacy_full_scan(
    entries: &[FolderEntry],
    db: &Db,
) -> Result<RatingLoad, DbError> {
    let owners = rating_owner_keys(entries);
    let snapshot = db.benchmark_full_legacy_rating_snapshot(
        entries.iter().map(|entry| entry.path.as_path()),
        &owners,
    )?;
    Ok(resolve_rating_snapshot(entries, owners, snapshot))
}

fn resolve_rating_snapshot(
    entries: &[FolderEntry],
    owners: Vec<Option<PathBuf>>,
    snapshot: crate::db::RatingSnapshot,
) -> (HashMap<usize, u8>, Vec<Option<PathBuf>>) {
    let mut out = HashMap::new();
    let mut members_by_owner: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    let mut unresolved = Vec::new();
    for (index, owner) in owners.iter().enumerate() {
        match owner {
            Some(owner) => members_by_owner
                .entry(owner.clone())
                .or_default()
                .push(index),
            None => unresolved.push(index),
        }
    }
    let entry_by_path = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.path.as_path(), index))
        .collect::<HashMap<_, _>>();
    let mut fallback_by_owner = HashMap::<PathBuf, Vec<_>>::new();
    let mut rejected_legacy_history_tokens = HashSet::new();
    for row in snapshot.by_path.values() {
        if snapshot.legacy_owners_require_derivation
            && (normalize_physical_path(&row.path) != row.path
                || sidecar_owner_key(&row.path).is_err())
        {
            // Legacy schemas did not retain proof of the physical owner seen
            // when a path-spelled rating was accepted. An alias can since
            // have been removed or retargeted to an equal-identity RAW.
            // Every same-name legacy row is therefore unordered with respect
            // to this history, even when it is now a physical path.
            if let Some(token) = sidecar_owner_collision_token(&row.path) {
                rejected_legacy_history_tokens.insert(token);
            }
            continue;
        }
        let (derived_owner, identity_matches) =
            if let Some(index) = entry_by_path.get(row.path.as_path()) {
                (
                    owners[*index].clone(),
                    entries[*index].size == row.size && entries[*index].mtime_ns == row.mtime_ns,
                )
            } else if row.sidecar_dirty || snapshot.legacy_owners_require_derivation {
                (
                    sidecar_owner_key(&row.path).ok(),
                    current_raw_identity(&row.path).ok() == Some((row.size, row.mtime_ns)),
                )
            } else {
                (None, false)
            };
        if let Some(derived_owner) = derived_owner
            && (row.sidecar_owner.is_none() || snapshot.legacy_owners_require_derivation)
            && row.sidecar_owner.as_ref() != Some(&derived_owner)
        {
            fallback_by_owner
                .entry(derived_owner)
                .or_default()
                .push((row, identity_matches));
        }
    }

    for (owner, members) in members_by_owner {
        if sidecar_owner_collision_token(&owner)
            .is_some_and(|token| rejected_legacy_history_tokens.contains(&token))
        {
            if let Some(rating) = sidecar_mtime_ns(&entries[members[0]].sidecar_path())
                .and_then(|_| xmp::read_rating(&entries[members[0]].sidecar_path()))
            {
                install_group_rating(&mut out, &members, rating);
            }
            continue;
        }
        let owned = snapshot.by_owner.get(&owner).filter(|row| {
            members
                .iter()
                .map(|index| &entries[*index])
                .find(|entry| entry.path == row.path)
                .map_or_else(
                    || stored_owner_identity_matches(&owner, row),
                    |entry| entry.size == row.size && entry.mtime_ns == row.mtime_ns,
                )
        });

        // One XMP target can be shared by multiple RAW containers. A current
        // dirty owner row is authoritative for every member, not just for the
        // exact RAW path that accepted the rating. Once a valid current owner
        // exists, leftover ownerless rows are historical mirrors and cannot
        // suppress or replace it.
        if let Some(owned) = owned {
            if owned.sidecar_dirty {
                if let Some(rating) = owned.rating {
                    install_group_rating(&mut out, &members, rating);
                }
                continue;
            }
            let sidecar = entries[members[0]].sidecar_path();
            let rating = sidecar_mtime_ns(&sidecar)
                .filter(|mtime| *mtime >= owned.sidecar_mtime_ns)
                .and_then(|_| xmp::read_rating(&sidecar))
                .or(owned.rating);
            if let Some(rating) = rating {
                install_group_rating(&mut out, &members, rating);
            }
            continue;
        }

        let fallback = fallback_by_owner.remove(&owner).unwrap_or_default();
        if fallback.iter().any(|(row, _)| row.sidecar_dirty) {
            if fallback.len() == 1
                && fallback[0].0.sidecar_dirty
                && fallback[0].1
                && let Some(rating) = fallback[0].0.rating
            {
                install_group_rating(&mut out, &members, rating);
            }
            continue;
        }

        let newest_db_sidecar = fallback
            .iter()
            .filter(|(_, identity_matches)| *identity_matches)
            .map(|(row, _)| row.sidecar_mtime_ns)
            .max()
            .unwrap_or(0);
        let sidecar = entries[members[0]].sidecar_path();
        let sidecar_rating = sidecar_mtime_ns(&sidecar)
            .filter(|mtime| *mtime >= newest_db_sidecar)
            .and_then(|_| xmp::read_rating(&sidecar));
        if let Some(rating) = sidecar_rating {
            install_group_rating(&mut out, &members, rating);
            continue;
        }

        let mut clean_ratings = fallback
            .iter()
            .filter(|(_, identity_matches)| *identity_matches)
            .map(|(row, _)| row.rating);
        let clean_rating = clean_ratings
            .next()
            .flatten()
            .filter(|rating| clean_ratings.all(|candidate| candidate == Some(*rating)));
        if let Some(rating) = clean_rating {
            install_group_rating(&mut out, &members, rating);
        }
    }

    for index in unresolved {
        let entry = &entries[index];
        let row = snapshot
            .by_path
            .get(&entry.path)
            .filter(|row| row.size == entry.size && row.mtime_ns == entry.mtime_ns)
            .filter(|row| {
                !snapshot.legacy_owners_require_derivation
                    || (normalize_physical_path(&row.path) == row.path
                        && sidecar_owner_key(&row.path).is_ok()
                        && sidecar_owner_collision_token(&row.path)
                            .is_none_or(|token| !rejected_legacy_history_tokens.contains(&token)))
            });
        if let Some(rating) = row
            .filter(|row| row.sidecar_dirty)
            .and_then(|row| row.rating)
        {
            out.insert(index, rating);
            continue;
        }
        let sidecar = entry.sidecar_path();
        let sidecar_rating = sidecar_mtime_ns(&sidecar)
            .filter(|mtime| *mtime >= row.map_or(0, |row| row.sidecar_mtime_ns))
            .and_then(|_| xmp::read_rating(&sidecar));
        if let Some(rating) = sidecar_rating.or_else(|| row.and_then(|row| row.rating)) {
            out.insert(index, rating);
        }
    }
    (out, owners)
}

fn install_group_rating(ratings: &mut HashMap<usize, u8>, members: &[usize], rating: u8) {
    ratings.extend(members.iter().map(|index| (*index, rating)));
}

fn stored_owner_identity_matches(
    owner: &std::path::Path,
    row: &crate::db::StoredRatingRow,
) -> bool {
    let raw = match sidecar_owner_key(&row.path) {
        Ok(current_owner) if current_owner == owner => row.path.clone(),
        Ok(_) => return false,
        Err(_) => match raw_path_from_sidecar_owner(owner, &row.path) {
            Ok(raw) => raw,
            Err(_) => return false,
        },
    };
    current_raw_identity(&raw).ok() == Some((row.size, row.mtime_ns))
}

fn sidecar_mtime_ns(path: &std::path::Path) -> Option<i64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos() as i64)
}

impl Library {
    /// Spawns the persistence thread.
    ///
    /// When a platform database path exists, it remains the publication
    /// authority: open failures retain updates for retry rather than silently
    /// switching to unsynchronized XMP writes. Systems without a platform
    /// configuration directory use the explicit database-free mode. Journaled
    /// sidecar writes left by a prior process resume when the database opens.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the persistence thread.
    pub fn start() -> Self {
        Self::start_with(configured_db_path(), SIDECAR_DEBOUNCE)
    }

    /// Spawns persistence and invokes `notify` once the configured database
    /// has completed any required migration or repair.
    ///
    /// The callback runs on the persistence thread and must return quickly.
    #[doc(hidden)]
    pub fn start_with_database_ready_notify(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::start_with_notify(configured_db_path(), SIDECAR_DEBOUNCE, Some(notify))
    }

    fn start_with(db_path: Option<PathBuf>, debounce: Duration) -> Self {
        Self::start_with_notify(db_path, debounce, None)
    }

    fn start_with_notify(
        db_path: Option<PathBuf>,
        debounce: Duration,
        database_ready_notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let dirty = Arc::new(AtomicBool::new(false));
        let worker_dirty = dirty.clone();
        let database_configured = db_path.is_some();
        let database_ready = Arc::new(AtomicBool::new(db_path.is_none()));
        let worker_database_ready = database_ready.clone();
        let worker = std::thread::spawn(move || {
            if let Some(path) = db_path {
                persist_configured_thread(
                    &rx,
                    &path,
                    debounce,
                    &worker_dirty,
                    &worker_database_ready,
                    database_ready_notify.as_deref(),
                );
            } else {
                persist_thread(
                    &rx,
                    None,
                    debounce,
                    &worker_dirty,
                    VecDeque::new(),
                    HashMap::new(),
                );
            }
        });
        Self {
            tx,
            worker: Some(worker),
            dirty,
            database_configured,
            database_ready,
        }
    }

    /// Returns whether this service has a platform database path to open.
    #[doc(hidden)]
    pub fn database_configured(&self) -> bool {
        self.database_configured
    }

    /// Returns true after the configured database has opened and completed
    /// any required schema migration or repair.
    ///
    /// Applications can use this non-blocking signal to refresh an initial
    /// read-only snapshot that was unavailable during folder open.
    pub fn database_ready(&self) -> bool {
        self.database_ready.load(Ordering::Acquire)
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

#[derive(Clone)]
enum JournalPredecessor {
    Owner(RatingOwnerSnapshot),
    Global(RatingGlobalSnapshot),
}

fn persist_configured_thread(
    rx: &Receiver<Cmd>,
    path: &std::path::Path,
    debounce: Duration,
    dirty: &AtomicBool,
    database_ready: &AtomicBool,
    database_ready_notify: Option<&(dyn Fn() + Send + Sync)>,
) {
    let mut backlog = VecDeque::new();
    let mut retry_at = Instant::now();
    let mut last_error: Option<String> = None;
    loop {
        if Instant::now() >= retry_at {
            match open_configured_db(path) {
                Ok((db, mut pending)) => {
                    // The ready signal is also the application's permission
                    // to take a durable rating snapshot. Resolve every
                    // startup journal row once first so a replaced or
                    // retargeted RAW cannot appear in that snapshot and then
                    // disappear immediately afterward.
                    let mut latest_sequence_by_owner = pending
                        .keys()
                        .cloned()
                        .map(|owner| (owner, 0_u64))
                        .collect::<HashMap<_, _>>();
                    flush_due(&mut pending, &mut latest_sequence_by_owner, Some(&db), true);
                    database_ready.store(true, Ordering::Release);
                    if let Some(notify) = database_ready_notify
                        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(notify)).is_err()
                    {
                        eprintln!(
                            "rating database readiness callback panicked; continuing persistence"
                        );
                    }
                    persist_thread(rx, Some(&db), debounce, dirty, backlog, pending);
                    return;
                }
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_deref() != Some(message.as_str()) {
                        eprintln!(
                            "rating database open failed for {}; retaining updates and retrying: \
                             {error}",
                            path.display()
                        );
                        last_error = Some(message);
                    }
                    retry_at = Instant::now()
                        + if matches!(
                            &error,
                            ConfiguredDbOpenError::Database(error)
                                if Db::is_lock_contention(error)
                        ) {
                            DB_OPEN_LOCK_RETRY
                        } else {
                            DB_OPEN_OTHER_RETRY
                        };
                }
            }
        }

        let wait = retry_at.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Cmd::Flush { done: Some(done) }) => {
                // A configured database is an ownership boundary, not an
                // optional cache. Report this attempt as incomplete and keep
                // an asynchronous marker at the same FIFO position.
                let _ = done.send(false);
                backlog.push_back(Cmd::Flush { done: None });
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                match open_configured_db(path) {
                    Ok((db, pending)) => {
                        backlog.push_back(Cmd::Shutdown);
                        persist_thread(rx, Some(&db), debounce, dirty, backlog, pending);
                    }
                    Err(error) => {
                        let unpersisted = backlog
                            .iter()
                            .filter(|command| matches!(command, Cmd::SetRating { .. }))
                            .count();
                        eprintln!(
                            "rating database remained unavailable for {}; leaving {unpersisted} \
                             queued update(s) unpublished: {error}",
                            path.display()
                        );
                        #[cfg(test)]
                        for command in backlog {
                            if let Cmd::Barrier { done } = command {
                                let _ = done.send(());
                            }
                        }
                    }
                }
                return;
            }
            Ok(command) => backlog.push_back(command),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn open_configured_db(
    path: &std::path::Path,
) -> Result<(Db, HashMap<PathBuf, Pending>), ConfiguredDbOpenError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|source| {
            ConfiguredDbOpenError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            }
        })?;
    }
    let db = Db::open_with_timeout(path, DB_OPEN_ATTEMPT_TIMEOUT)?;
    let pending = db
        .pending_sidecars_with_owners()?
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
        .collect();
    Ok((db, pending))
}

fn persist_thread(
    rx: &Receiver<Cmd>,
    db: Option<&Db>,
    debounce: Duration,
    dirty: &AtomicBool,
    mut backlog: VecDeque<Cmd>,
    mut pending: HashMap<PathBuf, Pending>,
) {
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
        let command = backlog
            .pop_front()
            .map_or_else(|| rx.recv_timeout(timeout), Ok);
        match command {
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
                        db.rating_global_snapshot(&path)
                            .map(JournalPredecessor::Global)
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
            let Some(predecessor) = p.journal_predecessor.clone() else {
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
            match db.synchronize_pending_sidecar(
                &path,
                p.size,
                p.mtime_ns,
                p.rating,
                |publication_path| match write_sidecar_for_identity(
                    publication_path,
                    p.size,
                    p.mtime_ns,
                    p.rating,
                ) {
                    Ok(mtime) => PendingSidecarWrite::Written(mtime),
                    Err(SidecarWriteError::RawReplaced { .. }) => PendingSidecarWrite::Discard,
                    Err(error) => PendingSidecarWrite::Failed(error),
                },
            ) {
                Ok(PendingSidecarSync::Written | PendingSidecarSync::Superseded) => {}
                Ok(PendingSidecarSync::Discarded) => {
                    eprintln!("discarding rating for replaced RAW file {}", path.display());
                }
                Ok(PendingSidecarSync::OwnerChanged) => {
                    eprintln!(
                        "quarantining rating because the RAW path now resolves to a different \
                         sidecar owner: {}; rate the photo again to publish it safely",
                        path.display()
                    );
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
    fn load_ratings_reads_a_legacy_dirty_journal_before_background_migration() {
        let directory = tempfile::tempdir().unwrap();
        let entry = entry(directory.path().join("legacy-dirty.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let database_path = directory.path().join("legacy.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                 VALUES (?1, ?2, ?3, 5, ?4, 1, 0)",
                rusqlite::params![
                    entry.path.to_str().unwrap(),
                    entry.size,
                    entry.mtime_ns,
                    sidecar_mtime(&entry),
                ],
            )
            .unwrap();
        drop(connection);

        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 5)]),
            "an accepted but unpublished legacy rating must beat older XMP"
        );
        assert!(
            db.pending_sidecars().is_err(),
            "legacy reads must not expose unvalidated rows for publication"
        );
        drop(db);

        let connection = rusqlite::Connection::open(&database_path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*)
                       FROM pragma_table_info('images')
                      WHERE name = 'sidecar_owner'",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0,
            "the latency-sensitive read path must not run migrations"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*)
                       FROM sqlite_schema
                      WHERE type = 'table'
                        AND name = 'viewr_schema_migrations'",
                    [],
                    |row| row.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn fallible_rating_load_surfaces_snapshot_conversion_errors() {
        let directory = tempfile::tempdir().unwrap();
        let entry = entry(directory.path().join("invalid-rating.ARW"));
        let database_path = directory.path().join("legacy-invalid.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES (?1, ?2, ?3, 'invalid', 1)",
                rusqlite::params![entry.path.to_str().unwrap(), entry.size, entry.mtime_ns,],
            )
            .unwrap();
        drop(connection);
        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert!(
            try_load_ratings_with_owners(std::slice::from_ref(&entry), Some(&db)).is_err(),
            "callers that coordinate recovery must be able to retry a failed snapshot"
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
    fn clean_owned_database_rating_applies_to_every_raw_sharing_the_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let arw = entry(directory.path().join("photo.ARW"));
        let dng = entry(directory.path().join("photo.DNG"));
        let db = Db::open_in_memory().unwrap();
        db.upsert_rating(
            arw.path.to_str().unwrap(),
            arw.size,
            arw.mtime_ns,
            Some(4),
            0,
        )
        .unwrap();

        assert_eq!(
            load_ratings(&[arw, dng], Some(&db)),
            HashMap::from([(0, 4), (1, 4)])
        );
    }

    #[test]
    fn owned_dirty_rating_overrides_a_leftover_clean_legacy_alias() {
        let directory = tempfile::tempdir().unwrap();
        let arw = entry(directory.path().join("photo.ARW"));
        let dng = entry(directory.path().join("photo.DNG"));
        let db = Db::open_in_memory().unwrap();
        put_db_rating(&db, &dng, Some(2), 0);
        db.record_rating_pending_sidecar_canonical(&arw.path, arw.size, arw.mtime_ns, 5)
            .unwrap();

        assert_eq!(
            load_ratings(&[arw, dng], Some(&db)),
            HashMap::from([(0, 5), (1, 5)])
        );
    }

    #[test]
    fn owned_clean_rating_overrides_a_conflicting_clean_legacy_alias() {
        let directory = tempfile::tempdir().unwrap();
        let arw = entry(directory.path().join("photo.ARW"));
        let dng = entry(directory.path().join("photo.DNG"));
        let db = Db::open_in_memory().unwrap();
        put_db_rating(&db, &dng, Some(2), 0);
        db.upsert_rating(
            arw.path.to_str().unwrap(),
            arw.size,
            arw.mtime_ns,
            Some(5),
            0,
        )
        .unwrap();

        assert_eq!(
            load_ratings(&[arw, dng], Some(&db)),
            HashMap::from([(0, 5), (1, 5)])
        );
    }

    #[test]
    fn singleton_legacy_clean_rating_applies_to_its_sidecar_owner_group() {
        let directory = tempfile::tempdir().unwrap();
        let arw = entry(directory.path().join("photo.ARW"));
        let dng = entry(directory.path().join("photo.DNG"));
        let db = Db::open_in_memory().unwrap();
        put_db_rating(&db, &arw, Some(3), 0);

        assert_eq!(
            load_ratings(&[arw, dng], Some(&db)),
            HashMap::from([(0, 3), (1, 3)])
        );
    }

    #[test]
    fn conflicting_legacy_clean_aliases_fail_closed_without_a_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let arw = entry(directory.path().join("photo.ARW"));
        let dng = entry(directory.path().join("photo.DNG"));
        let db = Db::open_in_memory().unwrap();
        put_db_rating(&db, &arw, Some(2), 0);
        put_db_rating(&db, &dng, Some(5), 0);

        assert!(load_ratings(&[arw, dng], Some(&db)).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unrequested_legacy_clean_alias_conflicts_fail_closed_for_both_schemas() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let arw = entry(physical.join("photo.ARW"));
        let dng = entry(physical.join("photo.DNG"));

        for owner_aware in [false, true] {
            let database_path = directory
                .path()
                .join(format!("legacy-clean-conflict-{owner_aware}.db"));
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            if owner_aware {
                connection
                    .execute_batch(
                        "CREATE TABLE images (
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
                        );
                        CREATE TABLE viewr_schema_migrations (
                            name TEXT PRIMARY KEY
                        ) WITHOUT ROWID;
                        INSERT INTO viewr_schema_migrations (name)
                        VALUES ('rating-generation-and-owner-v6');
                        CREATE UNIQUE INDEX images_sidecar_owners
                            ON images(sidecar_owner)
                         WHERE sidecar_owner IS NOT NULL;",
                    )
                    .unwrap();
                connection
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating, sidecar_owner)
                         VALUES
                            (?1, ?2, ?3, 2, ?4),
                            (?5, ?6, ?7, 5, ?8)",
                        rusqlite::params![
                            arw.path.to_str().unwrap(),
                            arw.size,
                            arw.mtime_ns,
                            arw.sidecar_path().to_str().unwrap(),
                            alias.join("photo.DNG").to_str().unwrap(),
                            dng.size,
                            dng.mtime_ns,
                            alias.join("photo.xmp").to_str().unwrap(),
                        ],
                    )
                    .unwrap();
            } else {
                connection
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
                connection
                    .execute(
                        "INSERT INTO images
                            (path, size, mtime_ns, rating)
                         VALUES
                            (?1, ?2, ?3, 2),
                            (?4, ?5, ?6, 5)",
                        rusqlite::params![
                            arw.path.to_str().unwrap(),
                            arw.size,
                            arw.mtime_ns,
                            alias.join("photo.DNG").to_str().unwrap(),
                            dng.size,
                            dng.mtime_ns,
                        ],
                    )
                    .unwrap();
            }
            drop(connection);
            let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

            assert!(
                load_ratings(std::slice::from_ref(&arw), Some(&db)).is_empty(),
                "schema owner_aware={owner_aware} must include the unrequested alias"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_dirty_symlink_spelling_uses_only_the_current_sidecar_before_migration() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let entry = entry(physical.join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let database_path = directory.path().join("legacy-alias.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES (?1, ?2, ?3, 5, 1)",
                rusqlite::params![
                    alias.join("photo.ARW").to_str().unwrap(),
                    entry.size,
                    entry.mtime_ns,
                ],
            )
            .unwrap();
        drop(connection);
        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 1)]),
            "an ownerless alias spelling must not override the current sidecar"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ambiguous_legacy_alias_history_suppresses_same_name_dirty_fallbacks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let mut first = entry(first_dir.join("photo.ARW"));
        let mut second = entry(second_dir.join("photo.ARW"));
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for candidate in [&mut first, &mut second] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&candidate.path)
                .unwrap()
                .set_modified(fixed)
                .unwrap();
            let metadata = std::fs::metadata(&candidate.path).unwrap();
            candidate.size = metadata.len();
            candidate.mtime_ns = metadata
                .modified()
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
        }
        assert_eq!((first.size, first.mtime_ns), (second.size, second.mtime_ns));
        xmp::write_rating(&first.sidecar_path(), 2).unwrap();
        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let database_path = directory.path().join("ambiguous-history.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES
                    (?1, ?2, ?3, 1, 1),
                    (?4, ?2, ?3, 5, 1)",
                rusqlite::params![
                    first.path.to_str().unwrap(),
                    first.size,
                    first.mtime_ns,
                    aliased_raw.to_str().unwrap(),
                ],
            )
            .unwrap();
        drop(connection);
        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();
        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert_eq!(
            load_ratings(&[first, second], Some(&db)),
            HashMap::from([(0, 2)]),
            "unordered legacy path histories must not override either current sidecar owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn removed_clean_legacy_alias_suppresses_same_name_dirty_fallback() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        let dirty = entry(physical.join("photo.ARW"));
        let clean = entry(physical.join("photo.DNG"));
        xmp::write_rating(&dirty.sidecar_path(), 2).unwrap();
        symlink(&physical, &alias).unwrap();
        let clean_alias = alias.join("photo.DNG");
        let database_path = directory.path().join("removed-clean-alias.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_mtime_ns,
                     sidecar_dirty, last_seen)
                 VALUES
                    (?1, ?2, ?3, 1, 0, 1, 1),
                    (?4, ?5, ?6, 5, 10, 0, 2)",
                rusqlite::params![
                    dirty.path.to_str().unwrap(),
                    dirty.size,
                    dirty.mtime_ns,
                    clean_alias.to_str().unwrap(),
                    clean.size,
                    clean.mtime_ns,
                ],
            )
            .unwrap();
        drop(connection);
        std::fs::remove_file(&alias).unwrap();
        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert_eq!(
            load_ratings(std::slice::from_ref(&dirty), Some(&db)),
            HashMap::from([(0, 2)]),
            "an unresolved clean history must prevent an unordered dirty fallback"
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_dirty_alias_remains_authoritative_after_the_alias_is_removed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        let entry = entry(physical.join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        symlink(&physical, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&aliased_raw, entry.size, entry.mtime_ns, 5)
            .unwrap();

        std::fs::remove_file(&alias).unwrap();

        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 5)]),
            "a validated physical owner must outlive its legacy alias spelling"
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_dirty_alias_retarget_with_equal_identity_does_not_transfer_rating() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let first_dir = directory.path().join("first");
        let second_dir = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let mut first = entry(first_dir.join("photo.ARW"));
        let mut second = entry(second_dir.join("photo.ARW"));
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for candidate in [&mut first, &mut second] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&candidate.path)
                .unwrap()
                .set_modified(fixed)
                .unwrap();
            let metadata = std::fs::metadata(&candidate.path).unwrap();
            candidate.size = metadata.len();
            candidate.mtime_ns = metadata
                .modified()
                .unwrap()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
        }
        assert_eq!((first.size, first.mtime_ns), (second.size, second.mtime_ns));

        symlink(&first_dir, &alias).unwrap();
        let aliased_raw = alias.join("photo.ARW");
        let db = Db::open_in_memory().unwrap();
        db.record_rating_pending_sidecar_path(&aliased_raw, first.size, first.mtime_ns, 5)
            .unwrap();

        std::fs::remove_file(&alias).unwrap();
        symlink(&second_dir, &alias).unwrap();

        assert!(
            load_ratings(&[first, second], Some(&db)).is_empty(),
            "a current owned row cannot become a legacy fallback for a new physical owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_dirty_alias_with_a_clean_sibling_fails_closed_before_migration() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let arw = entry(physical.join("photo.ARW"));
        let dng = entry(physical.join("photo.DNG"));
        xmp::write_rating(&arw.sidecar_path(), 1).unwrap();
        let database_path = directory.path().join("legacy-conflict.db");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
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
        connection
            .execute(
                "INSERT INTO images
                    (path, size, mtime_ns, rating, sidecar_dirty)
                 VALUES
                    (?1, ?2, ?3, 5, 1),
                    (?4, ?5, ?6, 2, 0)",
                rusqlite::params![
                    alias.join("photo.ARW").to_str().unwrap(),
                    arw.size,
                    arw.mtime_ns,
                    alias.join("photo.DNG").to_str().unwrap(),
                    dng.size,
                    dng.mtime_ns,
                ],
            )
            .unwrap();
        drop(connection);
        let db = Db::try_open_for_read(&database_path).unwrap().unwrap();

        assert_eq!(
            load_ratings(&[arw], Some(&db)),
            HashMap::from([(0, 1)]),
            "ambiguous legacy rows must not suppress the current sidecar"
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

        assert!(!library.database_configured());
        assert!(!library.dirty.load(Ordering::Acquire));
        library.flush();
        assert!(!library.dirty.load(Ordering::Acquire));
    }

    #[test]
    fn database_ready_callback_panic_does_not_stop_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let entry = entry(directory.path().join("photo.ARW"));
        let library = Library::start_with_notify(
            Some(database_path.clone()),
            Duration::from_secs(60),
            Some(Arc::new(|| panic!("readiness callback test panic"))),
        );

        worker_barrier(&library);
        assert!(library.database_ready());
        library.set_rating(&entry, 4);
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        assert_eq!(
            Db::open(&database_path)
                .unwrap()
                .get_image_path(&entry.path)
                .unwrap()
                .rating,
            Some(4)
        );
    }

    #[test]
    fn configured_database_lock_never_falls_back_to_unowned_xmp() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let entry = entry(directory.path().join("photo.ARW"));
        let blocker = rusqlite::Connection::open(&database_path).unwrap();
        blocker
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE hold_writer_lock (value INTEGER);",
            )
            .unwrap();
        let library = Library::start_with(Some(database_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 5);
        library.flush();

        assert!(library.dirty.load(Ordering::Acquire));
        assert!(
            !entry.sidecar_path().exists(),
            "a configured but locked database must not become database-free publication"
        );

        blocker.execute_batch("ROLLBACK").unwrap();
        worker_barrier(&library);
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        let row = Db::open(&database_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
    }

    #[test]
    fn transient_database_directory_failure_never_falls_back_to_unowned_xmp() {
        let directory = tempfile::tempdir().unwrap();
        let blocked_parent = directory.path().join("database");
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let database_path = blocked_parent.join("viewr.db");
        let entry = entry(directory.path().join("photo.ARW"));
        let notified = Arc::new(AtomicBool::new(false));
        let notify_flag = notified.clone();
        let library = Library::start_with_notify(
            Some(database_path.clone()),
            Duration::from_secs(60),
            Some(Arc::new(move || {
                notify_flag.store(true, Ordering::Release);
            })),
        );
        assert!(library.database_configured());
        assert!(!library.database_ready());
        assert!(!notified.load(Ordering::Acquire));

        library.set_rating(&entry, 4);
        library.flush();

        assert!(library.dirty.load(Ordering::Acquire));
        assert!(
            !entry.sidecar_path().exists(),
            "a temporary directory error must not become database-free publication"
        );

        std::fs::remove_file(&blocked_parent).unwrap();
        worker_barrier(&library);
        assert!(library.database_ready());
        assert!(notified.load(Ordering::Acquire));
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(4));
        let row = Db::open(&database_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(4));
        assert!(!row.sidecar_dirty);
        assert!(!library.dirty.load(Ordering::Acquire));
    }

    #[test]
    fn configured_database_recovery_precedes_queued_local_commands() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let entry = entry(directory.path().join("photo.ARW"));
        {
            let db = Db::open(&database_path).unwrap();
            db.record_rating_pending_sidecar(
                entry.path.to_str().unwrap(),
                entry.size,
                entry.mtime_ns,
                1,
            )
            .unwrap();
        }
        let blocker = rusqlite::Connection::open(&database_path).unwrap();
        blocker
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let library = Library::start_with(Some(database_path.clone()), Duration::from_secs(60));

        library.set_rating(&entry, 5);
        library.flush();
        assert!(!entry.sidecar_path().exists());

        blocker.execute_batch("ROLLBACK").unwrap();
        worker_barrier(&library);
        library.flush();

        assert_eq!(xmp::read_rating(&entry.sidecar_path()), Some(5));
        let row = Db::open(&database_path)
            .unwrap()
            .get_image_path(&entry.path)
            .unwrap();
        assert_eq!(row.rating, Some(5));
        assert!(!row.sidecar_dirty);
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
    fn obsolete_exact_path_writer_cannot_make_its_xmp_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("viewr.db");
        let entry = entry(directory.path().join("photo.ARW"));
        xmp::write_rating(&entry.sidecar_path(), 5).unwrap();
        let initial_sidecar_mtime = sidecar_mtime(&entry);
        let db = Db::open(&database_path).unwrap();
        db.upsert_rating(
            entry.path.to_str().unwrap(),
            entry.size,
            entry.mtime_ns,
            Some(5),
            initial_sidecar_mtime,
        )
        .unwrap();
        let obsolete = rusqlite::Connection::open(&database_path).unwrap();

        assert_eq!(
            obsolete
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         sidecar_dirty, last_seen)
                     VALUES (?1, ?2, ?3, 1, 0, 1, unixepoch())
                     ON CONFLICT(path) DO UPDATE SET
                        size = excluded.size,
                        mtime_ns = excluded.mtime_ns,
                        rating = excluded.rating,
                        sidecar_dirty = 1,
                        last_seen = excluded.last_seen",
                    rusqlite::params![entry.path.to_str().unwrap(), entry.size, entry.mtime_ns,],
                )
                .unwrap(),
            0
        );
        xmp::write_rating(&entry.sidecar_path(), 1).unwrap();
        let obsolete_sidecar_mtime = sidecar_mtime(&entry);
        assert_eq!(
            obsolete
                .execute(
                    "INSERT INTO images
                        (path, size, mtime_ns, rating, sidecar_mtime_ns,
                         sidecar_dirty, last_seen)
                     VALUES (?1, ?2, ?3, 1, ?4, 0, unixepoch())
                     ON CONFLICT(path) DO UPDATE SET
                        size = excluded.size,
                        mtime_ns = excluded.mtime_ns,
                        rating = excluded.rating,
                        sidecar_mtime_ns = excluded.sidecar_mtime_ns,
                        sidecar_dirty = 0,
                        last_seen = excluded.last_seen",
                    rusqlite::params![
                        entry.path.to_str().unwrap(),
                        entry.size,
                        entry.mtime_ns,
                        obsolete_sidecar_mtime,
                    ],
                )
                .unwrap(),
            0
        );

        assert_eq!(
            load_ratings(std::slice::from_ref(&entry), Some(&db)),
            HashMap::from([(0, 5)]),
            "the protected current row must outrank obsolete exact-path XMP"
        );
        let protected = db.get_image_path(&entry.path).unwrap();
        assert_eq!(protected.rating, Some(5));
        assert!(protected.sidecar_dirty);
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
                .synchronize_pending_sidecar(
                    &entry.path,
                    entry.size,
                    entry.mtime_ns,
                    5,
                    |publication_path| {
                        match write_sidecar_for_identity(
                            publication_path,
                            entry.size,
                            entry.mtime_ns,
                            5,
                        ) {
                            Ok(mtime) => PendingSidecarWrite::Written(mtime),
                            Err(error) => PendingSidecarWrite::Failed(error),
                        }
                    },
                )
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

        let callback_saw_settled_database = Arc::new(AtomicBool::new(false));
        let callback_result = callback_saw_settled_database.clone();
        let callback_db_path = db_path.clone();
        let callback_entry_path = entry.path.clone();
        let library = Library::start_with_notify(
            Some(db_path.clone()),
            Duration::from_secs(60),
            Some(Arc::new(move || {
                let settled = Db::try_open_for_read(&callback_db_path)
                    .ok()
                    .flatten()
                    .is_some_and(|db| db.get_image_path(&callback_entry_path).is_none());
                callback_result.store(settled, Ordering::Release);
            })),
        );
        worker_barrier(&library);

        assert!(library.database_ready());
        assert!(
            callback_saw_settled_database.load(Ordering::Acquire),
            "readiness must follow the initial recovery disposition"
        );
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
            .synchronize_pending_sidecar(
                &entry.path,
                entry.size,
                entry.mtime_ns,
                5,
                |publication_path| {
                    std::fs::write(publication_path, b"a replacement RAW payload").unwrap();
                    match write_sidecar_for_identity(
                        publication_path,
                        entry.size,
                        entry.mtime_ns,
                        5,
                    ) {
                        Ok(mtime) => PendingSidecarWrite::Written(mtime),
                        Err(SidecarWriteError::RawReplaced { .. }) => PendingSidecarWrite::Discard,
                        Err(error) => PendingSidecarWrite::Failed(error),
                    }
                },
            )
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
