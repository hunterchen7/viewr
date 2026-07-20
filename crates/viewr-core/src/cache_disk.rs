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
pub const DEVELOP_VERSION: u32 = 1;

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
}

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
