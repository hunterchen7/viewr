//! Ring 3: persistent disk cache of developed JPEGs.
//!
//! Lives OUTSIDE any photo folder (never pollutes synced libraries):
//! `~/Library/Caches/viewr/objects/xx/<blake3>.jpg`. Keyed by
//! (path, size, mtime, DEVELOP_VERSION, tier) so edited files and
//! pipeline changes self-invalidate — stale objects simply never hit
//! and become eligible for budget GC later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::time::Duration;

use crate::atomic_write;
use crate::folder::FolderEntry;
use crate::types::Tier;

/// Bump when the develop pipeline's output changes; invalidates every
/// cached render for free.
/// Version 3 invalidated renders after the crop, gamma, and RGBA-packing
/// pipeline was streamlined.
pub const DEVELOP_VERSION: u32 = 3;

#[derive(Clone)]
/// Persistent, file-identity-keyed cache of developed JPEG renders.
///
/// Clones share an in-process GC lock for the same canonical cache root, so
/// concurrent engines cannot run conflicting snapshot-based sweeps. Normal
/// reads and atomic writes may proceed while GC runs. Serialization does not
/// extend across processes.
pub struct DiskCache {
    root: PathBuf,
    budget_bytes: u64,
    gc_lock: Arc<Mutex<()>>,
}

static GC_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
const ORPHAN_TEMP_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

fn ascii_hex_name(name: &std::ffi::OsStr, expected_len: usize) -> Option<&str> {
    let name = name.to_str()?;
    (name.len() == expected_len && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(name)
}

fn is_cache_object_name(name: &std::ffi::OsStr, shard: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(key) = name.strip_suffix(".jpg") else {
        return false;
    };
    key.starts_with(shard)
        && key.len() == blake3::OUT_LEN * 2
        && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_viewr_temp_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(".viewr-") && name.ends_with(".tmp"))
}

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

fn hash_path_identity(hasher: &mut blake3::Hasher, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        hasher.update(path.as_os_str().as_bytes());
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;

        for code_unit in path.as_os_str().encode_wide() {
            hasher.update(&code_unit.to_le_bytes());
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        hasher.update(path.as_os_str().as_encoded_bytes());
    }
}

impl DiskCache {
    /// Opens the platform-default `viewr/objects` cache directory.
    ///
    /// Returns `None` when the platform has no cache directory or the cache
    /// root cannot be created. The budget is enforced only when
    /// [`gc_to_budget`](Self::gc_to_budget) is called; opening is otherwise
    /// non-destructive.
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

    #[cfg(any(test, feature = "benchmarks"))]
    #[doc(hidden)]
    /// Opens a cache at an explicit root for tests and benchmarks.
    pub fn open_at(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        let gc_lock = shared_gc_lock(&root);
        Self {
            root,
            budget_bytes: DEFAULT_DISK_BUDGET,
            gc_lock,
        }
    }

    /// Derives the persistent object key for an entry and render tier.
    ///
    /// The digest includes the native path representation, file size,
    /// nanosecond modification time, [`DEVELOP_VERSION`], and tier. It avoids
    /// lossy path conversion, but deliberately does not hash file contents.
    pub fn key(entry: &FolderEntry, tier: Tier) -> String {
        let mut hasher = blake3::Hasher::new();
        hash_path_identity(&mut hasher, &entry.path);
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

    /// Returns whether a regular cache object exists for `key`.
    ///
    /// Invalid keys and filesystem errors are reported as `false`.
    pub fn has(&self, key: &str) -> bool {
        self.object_path(key).is_ok_and(|path| path.is_file())
    }

    /// Reads a cache object into memory.
    ///
    /// Returns `None` for invalid keys, missing objects, and read failures.
    /// Callers that decode the bytes should treat corrupt JPEG data as a cache
    /// miss; [`crate::jobs::Engine`] removes corrupt objects before rebuilding.
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

    /// Atomically stores an object using a temporary file in the same shard.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::InvalidInput`] if `key` is not a
    /// 64-character hexadecimal digest. Other errors report directory
    /// creation, temporary-file, write, or rename failures.
    pub fn put(&self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.object_path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic_write::replace(&path, bytes)
    }

    /// Enforces the byte budget supplied to [`open_default`](Self::open_default).
    ///
    /// Returns the number of object bytes successfully deleted. Filesystem
    /// failures are best-effort and do not surface as errors.
    pub fn gc_to_budget(&self) -> u64 {
        self.gc(self.budget_bytes)
    }

    /// Attempts one budget sweep without waiting for another sweep on this
    /// cache root.
    ///
    /// Returns `None` when a sweep is already active. This is intended for
    /// best-effort background maintenance where folder/session teardown must
    /// not queue behind a full directory scan.
    pub fn try_gc_to_budget(&self) -> Option<u64> {
        let _gc_guard = match self.gc_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        Some(self.gc_without_lock(self.budget_bytes))
    }

    /// Enforce the byte budget by deleting the oldest-written objects first.
    /// Cache reads do not refresh file modification times, so this is not LRU.
    /// Stale-keyed objects age out naturally.
    ///
    /// The sweep recognizes only regular `<64 hex>.jpg` objects in matching,
    /// real `<2 hex>` shard directories and abandoned `.viewr-*.tmp` files.
    /// Symlinks and unrelated filesystem entries are ignored. Returns bytes
    /// deleted; recent temporary files may belong to active atomic writers.
    pub fn gc(&self, budget_bytes: u64) -> u64 {
        // Engines opened for the same cache root share this lock. It prevents
        // snapshot-based GC passes from racing and independently deleting the
        // same budget excess.
        let _gc_guard = self
            .gc_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.gc_without_lock(budget_bytes)
    }

    fn gc_without_lock(&self, budget_bytes: u64) -> u64 {
        let mut objects: Vec<(std::path::PathBuf, std::time::SystemTime, u64)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(shards) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        for shard in shards.flatten() {
            let Ok(shard_type) = shard.file_type() else {
                continue;
            };
            if !shard_type.is_dir() {
                continue;
            }
            let shard_name = shard.file_name();
            let Some(shard_name) = ascii_hex_name(&shard_name, 2) else {
                continue;
            };
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(file_type) = file.file_type() else {
                    continue;
                };
                if !file_type.is_file() {
                    continue;
                }
                let path = file.path();
                let Ok(md) = file.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let file_name = file.file_name();
                if is_viewr_temp_name(&file_name) {
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
                if !is_cache_object_name(&file_name, shard_name) {
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

    #[cfg(unix)]
    #[test]
    fn key_distinguishes_non_utf8_paths_with_the_same_lossy_text() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let make_entry = |invalid_byte| {
            let mut path = b"/photos/a_.arw".to_vec();
            path[9] = invalid_byte;
            FolderEntry {
                path: PathBuf::from(OsString::from_vec(path)),
                file_name: "non-utf8.arw".into(),
                size: 10,
                mtime_ns: 1,
            }
        };
        let first = make_entry(0x80);
        let second = make_entry(0x81);
        assert_eq!(first.path.to_string_lossy(), second.path.to_string_lossy());

        assert_ne!(
            DiskCache::key(&first, Tier::Browse),
            DiskCache::key(&second, Tier::Browse)
        );
    }

    #[cfg(windows)]
    #[test]
    fn key_distinguishes_non_unicode_windows_paths_with_the_same_lossy_text() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt as _;

        let make_entry = |surrogate| {
            let mut path = "C:\\photos\\photo-".encode_utf16().collect::<Vec<_>>();
            path.push(surrogate);
            path.extend(".arw".encode_utf16());
            FolderEntry {
                path: PathBuf::from(OsString::from_wide(&path)),
                file_name: "non-unicode.arw".into(),
                size: 10,
                mtime_ns: 1,
            }
        };
        let first = make_entry(0xd800);
        let second = make_entry(0xd801);
        assert_eq!(first.path.to_string_lossy(), second.path.to_string_lossy());

        assert_ne!(
            DiskCache::key(&first, Tier::Browse),
            DiskCache::key(&second, Tier::Browse)
        );
    }

    #[cfg(unix)]
    #[test]
    fn valid_utf8_path_preserves_the_existing_cache_key() {
        let entry = entry(10, 1);
        let mut legacy = blake3::Hasher::new();
        legacy.update(entry.path.to_string_lossy().as_bytes());
        legacy.update(&entry.size.to_le_bytes());
        legacy.update(&entry.mtime_ns.to_le_bytes());
        legacy.update(&DEVELOP_VERSION.to_le_bytes());
        legacy.update(b"b");

        assert_eq!(
            DiskCache::key(&entry, Tier::Browse),
            legacy.finalize().to_hex().to_string()
        );
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
        let unrelated_temp = cache
            .object_path(&key)
            .unwrap()
            .with_file_name("third-party.tmp");
        let unrelated_viewr_file = cache
            .object_path(&key)
            .unwrap()
            .with_file_name(".viewr-interrupted.part");
        std::fs::write(&stale, b"interrupted write").unwrap();
        std::fs::write(&recent, b"active write").unwrap();
        std::fs::write(&unrelated_temp, b"belongs to another tool").unwrap();
        std::fs::write(&unrelated_viewr_file, b"not an atomic-write temp").unwrap();
        let stale_file = std::fs::File::options().write(true).open(&stale).unwrap();
        stale_file
            .set_modified(
                std::time::SystemTime::now() - ORPHAN_TEMP_MIN_AGE - Duration::from_secs(1),
            )
            .unwrap();
        for path in [&unrelated_temp, &unrelated_viewr_file] {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(
                std::time::SystemTime::now() - ORPHAN_TEMP_MIN_AGE - Duration::from_secs(1),
            )
            .unwrap();
        }

        assert_eq!(cache.gc(u64::MAX), 0);
        assert!(!stale.exists());
        assert!(recent.exists());
        assert!(unrelated_temp.exists());
        assert!(unrelated_viewr_file.exists());
        assert_eq!(cache.get(&key).unwrap(), b"cached object");
    }

    #[test]
    fn gc_ignores_entries_outside_the_cache_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());
        let key = DiskCache::key(&entry(10, 1), Tier::Browse);
        cache.put(&key, b"cached object").unwrap();

        let shard = cache
            .object_path(&key)
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let unrelated_jpeg = shard.join("notes.jpg");
        let wrong_prefix = if &key[..2] == "ff" { "ee" } else { "ff" };
        let misplaced_object = shard.join(format!("{wrong_prefix}{}.jpg", "0".repeat(62)));
        let nested_file = shard.join("nested").join("keep");
        let invalid_shard_file =
            dir.path()
                .join("not-a-shard")
                .join(format!("{}{}", "a".repeat(64), ".jpg"));
        std::fs::write(&unrelated_jpeg, b"unrelated").unwrap();
        std::fs::write(&misplaced_object, b"wrong shard").unwrap();
        std::fs::create_dir_all(nested_file.parent().unwrap()).unwrap();
        std::fs::write(&nested_file, b"nested").unwrap();
        std::fs::create_dir_all(invalid_shard_file.parent().unwrap()).unwrap();
        std::fs::write(&invalid_shard_file, b"invalid shard").unwrap();

        assert_eq!(cache.gc(0), b"cached object".len() as u64);
        assert!(!cache.has(&key));
        assert!(unrelated_jpeg.exists());
        assert!(misplaced_object.exists());
        assert!(nested_file.exists());
        assert!(invalid_shard_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn gc_does_not_follow_symlinked_shards_or_objects() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cache = DiskCache::open_at(dir.path().to_owned());

        let outside_shard_object = outside.path().join(format!("{}.jpg", "a".repeat(64)));
        let outside_shard_temp = outside.path().join(".viewr-abandoned.tmp");
        std::fs::write(&outside_shard_object, b"external object").unwrap();
        std::fs::write(&outside_shard_temp, b"external temp").unwrap();
        let stale_temp = std::fs::File::options()
            .write(true)
            .open(&outside_shard_temp)
            .unwrap();
        stale_temp
            .set_modified(
                std::time::SystemTime::now() - ORPHAN_TEMP_MIN_AGE - Duration::from_secs(1),
            )
            .unwrap();
        let shard_link = dir.path().join("aa");
        symlink(outside.path(), &shard_link).unwrap();

        let real_shard = dir.path().join("bb");
        std::fs::create_dir(&real_shard).unwrap();
        let outside_object = outside.path().join("object-sentinel");
        std::fs::write(&outside_object, b"external target").unwrap();
        let object_link = real_shard.join(format!("{}.jpg", "b".repeat(64)));
        symlink(&outside_object, &object_link).unwrap();

        assert_eq!(cache.gc(0), 0);
        assert_eq!(
            std::fs::read(&outside_shard_object).unwrap(),
            b"external object"
        );
        assert_eq!(
            std::fs::read(&outside_shard_temp).unwrap(),
            b"external temp"
        );
        assert_eq!(std::fs::read(&outside_object).unwrap(), b"external target");
        assert!(
            shard_link
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            object_link
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn cache_instances_for_one_root_share_gc_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let first = DiskCache::open_at(dir.path().to_owned());
        let second = DiskCache::open_at(dir.path().to_owned());
        assert!(Arc::ptr_eq(&first.gc_lock, &second.gc_lock));

        let guard = first.gc_lock.lock().unwrap();
        assert_eq!(second.try_gc_to_budget(), None);
        drop(guard);
        assert_eq!(second.try_gc_to_budget(), Some(0));
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
