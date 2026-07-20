//! Ring 3: persistent disk cache of developed JPEGs.
//!
//! Lives OUTSIDE any photo folder (never pollutes synced libraries):
//! `~/Library/Caches/viewr/objects/xx/<blake3>.jpg`. Keyed by
//! (path, size, mtime, DEVELOP_VERSION, tier) so edited files and
//! pipeline changes self-invalidate — stale objects simply never hit
//! and are swept by GC later.

use std::path::PathBuf;

use crate::folder::FolderEntry;
use crate::types::Tier;

/// Bump when the develop pipeline's output changes; invalidates every
/// cached render for free.
/// v2: base tone curve added.
pub const DEVELOP_VERSION: u32 = 2;

#[derive(Clone)]
pub struct DiskCache {
    root: PathBuf,
}

impl DiskCache {
    pub fn open_default() -> Option<Self> {
        let root = dirs::cache_dir()?.join("viewr").join("objects");
        std::fs::create_dir_all(&root).ok()?;
        Some(Self { root })
    }

    #[cfg(test)]
    pub fn open_at(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
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
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
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

    fn entry(size: u64, mtime: i64) -> FolderEntry {
        FolderEntry {
            path: "/photos/a.arw".into(),
            file_name: "a.arw".into(),
            size,
            mtime_ns: mtime,
        }
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
        let dir = std::env::temp_dir().join(format!("viewr-gc-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let cache = DiskCache::open_at(dir.clone());
        let keys: Vec<String> = (0..3)
            .map(|i| DiskCache::key(&entry(10 + i, 1), Tier::Browse))
            .collect();
        for key in &keys {
            cache.put(key, &[0u8; 1000]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        // Budget fits two objects: the oldest must go.
        let deleted = cache.gc(2500);
        assert_eq!(deleted, 1000);
        assert!(!cache.has(&keys[0]));
        assert!(cache.has(&keys[1]) && cache.has(&keys[2]));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = std::env::temp_dir().join(format!("viewr-test-{}", std::process::id()));
        let cache = DiskCache::open_at(dir.clone());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        assert!(!cache.has(&key));
        cache.put(&key, b"hello").unwrap();
        assert!(cache.has(&key));
        assert_eq!(cache.get(&key).unwrap(), b"hello");
        std::fs::remove_dir_all(dir).ok();
    }
}
