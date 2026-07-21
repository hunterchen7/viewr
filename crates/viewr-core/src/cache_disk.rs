//! Ring 3: persistent disk cache of developed JPEGs.
//!
//! Lives OUTSIDE any photo folder (never pollutes synced libraries):
//! `~/Library/Caches/viewr/objects/xx/<blake3>.jpg`. Keyed by
//! (path, size, mtime, DEVELOP_VERSION, tier) so edited files and
//! pipeline changes self-invalidate — stale objects simply never hit
//! and are swept by GC later.

use std::path::PathBuf;

use crate::atomic_write;
use crate::folder::FolderEntry;
use crate::types::Tier;

/// Bump when the develop pipeline's output changes; invalidates every
/// cached render for free.
/// v2: base tone curve added.
pub const DEVELOP_VERSION: u32 = 2;

#[derive(Clone)]
pub struct DiskCache {
    root: PathBuf,
    budget_bytes: u64,
}

impl DiskCache {
    pub fn open_default(budget_bytes: u64) -> Option<Self> {
        let root = dirs::cache_dir()?.join("viewr").join("objects");
        std::fs::create_dir_all(&root).ok()?;
        Some(Self { root, budget_bytes })
    }

    #[cfg(test)]
    pub fn open_at(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            budget_bytes: DEFAULT_DISK_BUDGET,
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

    fn object_path(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(format!("{key}.jpg"))
    }

    pub fn has(&self, key: &str) -> bool {
        self.object_path(key).is_file()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.object_path(key)).ok()
    }

    /// Atomic write: tmp in the same directory, then rename.
    pub fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.object_path(key);
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
    /// Also sweeps orphaned .tmp files. Returns bytes deleted.
    pub fn gc(&self, budget_bytes: u64) -> u64 {
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
                    let _ = std::fs::remove_file(&path);
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
            .open(cache.object_path(key))
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
        assert!(!cache.object_path(&key).with_extension("tmp").exists());
    }

    #[test]
    fn gc_sweeps_orphaned_temp_files_even_when_under_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        cache.put(&key, b"cached object").unwrap();
        let tmp = cache.object_path(&key).with_extension("tmp");
        std::fs::write(&tmp, b"interrupted write").unwrap();

        assert_eq!(cache.gc(u64::MAX), 0);
        assert!(!tmp.exists());
        assert_eq!(cache.get(&key).unwrap(), b"cached object");
    }
}
