//! Ring 3: persistent disk cache of developed JPEGs.
//!
//! Lives OUTSIDE any photo folder (never pollutes synced libraries):
//! `~/Library/Caches/viewr/objects/xx/<blake3>.jpg`. Keyed by
//! (path, size, mtime, DEVELOP_VERSION, tier) so edited files and
//! pipeline changes self-invalidate — stale objects simply never hit
//! and are swept by GC later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::atomic_write;
use crate::folder::FolderEntry;
use crate::types::Tier;

/// Bump when the develop pipeline's output changes; invalidates every
/// cached render for free.
/// v2: base tone curve added.
pub const DEVELOP_VERSION: u32 = 3;

#[derive(Clone)]
pub struct DiskCache {
    root: PathBuf,
    budget_bytes: u64,
    gc_lock: Arc<Mutex<()>>,
}

static GC_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
const ORPHAN_TEMP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn shared_gc_lock(root: &Path) -> Arc<Mutex<()>> {
    let identity = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let locks = GC_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(&identity).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    locks.insert(identity, Arc::downgrade(&lock));
    lock
}

impl DiskCache {
    pub fn open_default(budget_bytes: u64) -> Option<Self> {
        let root = dirs::cache_dir()?.join("viewr").join("objects");
        std::fs::create_dir_all(&root).ok()?;
        let gc_lock = shared_gc_lock(&root);
        Some(Self {
            root,
            budget_bytes,
            gc_lock,
        })
    }

    #[cfg(test)]
    pub fn open_at(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        let gc_lock = shared_gc_lock(&root);
        Self {
            root,
            budget_bytes: DEFAULT_DISK_BUDGET,
            gc_lock,
        }
    }

    pub fn key(entry: &FolderEntry, tier: Tier) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(entry.path.to_string_lossy().as_bytes());
        hasher.update(&entry.size.to_le_bytes());
        hasher.update(&entry.mtime_ns.to_le_bytes());
        hasher.update(&DEVELOP_VERSION.to_le_bytes());
        hasher.update(match tier {
            Tier::Thumb => b"t",
            Tier::Browse => b"b",
            Tier::Full => b"f",
        });
        hasher.finalize().to_hex().to_string()
    }

    fn object_path(&self, key: &str) -> std::io::Result<PathBuf> {
        if key.len() != blake3::OUT_LEN * 2 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "disk cache key must be a 64-character hexadecimal digest",
            ));
        }
        Ok(self.root.join(&key[..2]).join(format!("{key}.jpg")))
    }

    pub fn has(&self, key: &str) -> bool {
        self.object_path(key).is_ok_and(|path| path.is_file())
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.object_path(key).ok()?).ok()
    }

    pub(crate) fn remove(&self, key: &str) -> std::io::Result<bool> {
        match std::fs::remove_file(self.object_path(key)?) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Atomic write: tmp in the same directory, then rename.
    pub fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.object_path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write::replace(&path, bytes)
    }

    /// Enforce the configured byte budget.
    pub fn gc_to_budget(&self) -> u64 {
        self.gc(self.budget_bytes)
    }

    /// Enforce the byte budget by deleting oldest objects first
    /// (age-approximated LRU; stale-keyed objects age out naturally).
    /// Also sweeps `.tmp` files old enough to be abandoned. Returns bytes
    /// deleted; recent temporary files may belong to active atomic writers.
    pub fn gc(&self, budget_bytes: u64) -> u64 {
        // Engines opened for the same cache root share this lock. It prevents
        // snapshot-based GC passes from racing and independently deleting the
        // same budget excess.
        let _gc_guard = self
            .gc_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut objects: Vec<(std::path::PathBuf, std::time::SystemTime, u64)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(shards) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        for shard in shards.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let Ok(md) = file.metadata() else { continue };
                if path.extension().is_some_and(|e| e == "tmp") {
                    let is_stale = md
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= ORPHAN_TEMP_MIN_AGE);
                    if is_stale {
                        let _ = std::fs::remove_file(&path);
                    }
                    continue;
                }
                let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
                total += md.len();
                objects.push((path, mtime, md.len()));
            }
        }
        if total <= budget_bytes {
            return 0;
        }
        objects.sort_by_key(|(_, mtime, _)| *mtime);
        let mut deleted = 0u64;
        for (path, _, len) in objects {
            if total - deleted <= budget_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                deleted += len;
            }
        }
        deleted
    }
}

/// Default disk budget: 20GB.
pub const DEFAULT_DISK_BUDGET: u64 = 20 * 1024 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn entry(size: u64, mtime: i64) -> FolderEntry {
        FolderEntry {
            path: "/photos/a.arw".into(),
            file_name: "a.arw".into(),
            size,
            mtime_ns: mtime,
        }
    }

    fn set_object_mtime(cache: &DiskCache, key: &str, seconds_since_epoch: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(cache.object_path(key).unwrap())
            .unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds_since_epoch))
            .unwrap();
    }

    #[test]
    fn key_changes_with_file_identity_and_tier() {
        let a = DiskCache::key(&entry(10, 1), Tier::Browse);
        assert_ne!(a, DiskCache::key(&entry(11, 1), Tier::Browse));
        assert_ne!(a, DiskCache::key(&entry(10, 2), Tier::Browse));
        assert_ne!(a, DiskCache::key(&entry(10, 1), Tier::Full));
        assert_eq!(a, DiskCache::key(&entry(10, 1), Tier::Browse));
    }

    #[test]
    fn gc_deletes_oldest_until_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let keys: Vec<String> = (0..3)
            .map(|i| DiskCache::key(&entry(10 + i, 1), Tier::Browse))
            .collect();
        for (index, key) in keys.iter().enumerate() {
            cache.put(key, &[0u8; 1000]).unwrap();
            set_object_mtime(&cache, key, 10 + index as u64);
        }
        // Budget fits two objects: the oldest must go.
        let deleted = cache.gc(2500);
        assert_eq!(deleted, 1000);
        assert!(!cache.has(&keys[0]));
        assert!(cache.has(&keys[1]) && cache.has(&keys[2]));
    }

    #[test]
    fn put_replaces_an_existing_object_without_leaving_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        assert!(!cache.has(&key));

        cache.put(&key, b"first").unwrap();
        cache.put(&key, b"replacement").unwrap();

        assert!(cache.has(&key));
        assert_eq!(cache.get(&key).unwrap(), b"replacement");
        assert!(
            !cache
                .object_path(&key)
                .unwrap()
                .with_extension("tmp")
                .exists()
        );
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        cache.put(&key, b"cached object").unwrap();

        assert!(cache.remove(&key).unwrap());
        assert!(!cache.remove(&key).unwrap());
        assert!(!cache.has(&key));
    }

    #[test]
    fn gc_sweeps_only_stale_temp_files_even_when_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        cache.put(&key, b"cached object").unwrap();
        let stale = cache
            .object_path(&key)
            .unwrap()
            .with_file_name(".viewr-stale.tmp");
        let recent = cache
            .object_path(&key)
            .unwrap()
            .with_file_name(".viewr-active.tmp");
        std::fs::write(&stale, b"interrupted write").unwrap();
        std::fs::write(&recent, b"active write").unwrap();
        let stale_file = std::fs::File::options().write(true).open(&stale).unwrap();
        stale_file
            .set_modified(
                std::time::SystemTime::now() - ORPHAN_TEMP_MIN_AGE - Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(cache.gc(u64::MAX), 0);
        assert!(!stale.exists());
        assert!(recent.exists());
        assert_eq!(cache.get(&key).unwrap(), b"cached object");
    }

    #[test]
    fn cache_instances_for_one_root_share_gc_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let first = DiskCache::open_at(dir.path().to_owned());
        let second = DiskCache::open_at(dir.path().to_owned());
        assert!(Arc::ptr_eq(&first.gc_lock, &second.gc_lock));
    }

    #[test]
    fn public_operations_reject_non_digest_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        for key in ["", "a", "../outside", &"g".repeat(64)] {
            assert!(!cache.has(key));
            assert!(cache.get(key).is_none());
            assert_eq!(
                cache.put(key, b"data").unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        assert!(!dir.path().join("outside.jpg").exists());
    }
}
