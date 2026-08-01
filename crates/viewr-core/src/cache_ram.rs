//! Byte-budgeted RAM cache rings.
//!
//! Independent rings hold thumbnail, Browse RGBA, Full RGBA, and encoded JPEG
//! payloads. Every ring is exact LRU by bytes, never by image count. Pinned
//! keys (current ±1) are never evicted. The Full ring additionally follows an
//! explicit navigation working set, so stale speculative renders are removed
//! immediately and late completions cannot repopulate them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::types::{PixelBuf, Tier};

/// Cache identity as `(folder index, render tier)`.
///
/// An index is meaningful only for the immutable folder entry list associated
/// with the cache's engine.
pub type Key = (usize, Tier);

struct Entry<V> {
    key: Key,
    value: V,
    bytes: u64,
    last_use: u64,
    prev: Option<usize>,
    next: Option<usize>,
    pinned: bool,
}

struct ByteLru<V> {
    map: HashMap<Key, usize>,
    entries: Vec<Option<Entry<V>>>,
    vacant: Vec<usize>,
    budget: u64,
    bytes: u64,
    clock: u64,
    lru: Option<usize>,
    mru: Option<usize>,
    oldest_unpinned: Option<usize>,
    unpinned: usize,
}

impl<V: Clone> ByteLru<V> {
    fn new(budget: u64) -> Self {
        Self {
            map: HashMap::new(),
            entries: Vec::new(),
            vacant: Vec::new(),
            budget,
            bytes: 0,
            clock: 0,
            lru: None,
            mru: None,
            oldest_unpinned: None,
            unpinned: 0,
        }
    }

    fn get(&mut self, key: &Key) -> Option<V> {
        self.clock += 1;
        let clock = self.clock;
        let index = self.map.get(key).copied()?;

        self.move_to_mru(index);
        let entry = self.entry_mut(index);
        entry.last_use = clock;
        Some(entry.value.clone())
    }

    fn contains(&self, key: &Key) -> bool {
        self.map.contains_key(key)
    }

    fn remove(&mut self, key: &Key) -> Option<V> {
        let index = self.map.get(key).copied()?;
        let (prev, next, bytes, pinned) = {
            let entry = self.entry(index);
            (entry.prev, entry.next, entry.bytes, entry.pinned)
        };

        if !pinned {
            if self.oldest_unpinned == Some(index) {
                self.oldest_unpinned = if self.unpinned == 1 {
                    None
                } else {
                    Some(self.find_next_unpinned(next))
                };
            }
            self.unpinned -= 1;
        }

        if let Some(prev) = prev {
            self.entry_mut(prev).next = next;
        } else {
            self.lru = next;
        }
        if let Some(next) = next {
            self.entry_mut(next).prev = prev;
        } else {
            self.mru = prev;
        }

        let removed = self.map.remove(key);
        debug_assert_eq!(removed, Some(index));
        let entry = self.entries[index]
            .take()
            .expect("LRU index must point to a resident entry");
        self.vacant.push(index);
        self.bytes -= bytes;
        Some(entry.value)
    }

    fn insert(&mut self, key: Key, value: V, bytes: u64, pinned: bool) {
        self.clock += 1;
        let clock = self.clock;

        if let Some(index) = self.map.get(&key).copied() {
            self.move_to_mru(index);
            let old_bytes = self.entry(index).bytes;
            self.bytes -= old_bytes;
            let entry = self.entry_mut(index);
            debug_assert_eq!(entry.pinned, pinned);
            entry.value = value;
            entry.bytes = bytes;
            entry.last_use = clock;
        } else {
            let previous_mru = self.mru;
            let entry = Entry {
                key,
                value,
                bytes,
                last_use: clock,
                prev: previous_mru,
                next: None,
                pinned,
            };
            let index = if let Some(index) = self.vacant.pop() {
                debug_assert!(self.entries[index].is_none());
                self.entries[index] = Some(entry);
                index
            } else {
                let index = self.entries.len();
                self.entries.push(Some(entry));
                index
            };
            self.map.insert(key, index);
            if let Some(previous_mru) = previous_mru {
                self.entry_mut(previous_mru).next = Some(index);
            } else {
                self.lru = Some(index);
            }
            self.mru = Some(index);

            if !pinned {
                self.unpinned += 1;
                if self.oldest_unpinned.is_none() {
                    self.oldest_unpinned = Some(index);
                }
            }
        }
        self.bytes += bytes;
        self.evict_over_budget();
    }

    /// Marks an existing entry as (un)pinned without changing its recency.
    ///
    /// `oldest_unpinned` makes the eviction hot path constant-time. Finding its
    /// successor can walk over pinned nodes, but those nodes are never searched
    /// from the start of the map and the walk is amortized across LRU progress.
    fn set_pinned(&mut self, key: &Key, pinned: bool) {
        let Some(index) = self.map.get(key).copied() else {
            return;
        };
        let entry = self.entry(index);
        if entry.pinned == pinned {
            return;
        }

        let next = entry.next;
        let last_use = entry.last_use;
        self.entry_mut(index).pinned = pinned;

        if pinned {
            self.unpinned -= 1;
            if self.oldest_unpinned == Some(index) {
                self.oldest_unpinned = if self.unpinned == 0 {
                    None
                } else {
                    Some(self.find_next_unpinned(next))
                };
            }
        } else {
            self.unpinned += 1;
            let becomes_oldest = self
                .oldest_unpinned
                .is_none_or(|oldest| last_use < self.entry(oldest).last_use);
            if becomes_oldest {
                self.oldest_unpinned = Some(index);
            }
        }
    }

    fn evict_over_budget(&mut self) {
        while self.bytes > self.budget {
            let Some(victim) = self.oldest_unpinned else {
                break; // everything pinned
            };
            self.remove_unpinned(victim);
        }
    }

    fn move_to_mru(&mut self, index: usize) {
        if self.mru == Some(index) {
            return;
        }

        let (prev, next, pinned) = {
            let entry = self.entry(index);
            (entry.prev, entry.next, entry.pinned)
        };

        if !pinned && self.oldest_unpinned == Some(index) && self.unpinned > 1 {
            self.oldest_unpinned = Some(self.find_next_unpinned(next));
        }

        if let Some(prev) = prev {
            self.entry_mut(prev).next = next;
        } else {
            self.lru = next;
        }
        if let Some(next) = next {
            self.entry_mut(next).prev = prev;
        }

        let previous_mru = self.mru;
        let entry = self.entry_mut(index);
        entry.prev = previous_mru;
        entry.next = None;
        if let Some(previous_mru) = previous_mru {
            self.entry_mut(previous_mru).next = Some(index);
        }
        self.mru = Some(index);
    }

    fn remove_unpinned(&mut self, index: usize) {
        debug_assert_eq!(self.oldest_unpinned, Some(index));
        let (key, prev, next, bytes, pinned) = {
            let entry = self.entry(index);
            (entry.key, entry.prev, entry.next, entry.bytes, entry.pinned)
        };
        debug_assert!(!pinned);

        self.unpinned -= 1;
        self.oldest_unpinned = if self.unpinned == 0 {
            None
        } else {
            Some(self.find_next_unpinned(next))
        };

        if let Some(prev) = prev {
            self.entry_mut(prev).next = next;
        } else {
            self.lru = next;
        }
        if let Some(next) = next {
            self.entry_mut(next).prev = prev;
        } else {
            self.mru = prev;
        }

        let removed = self.map.remove(&key);
        debug_assert_eq!(removed, Some(index));
        self.entries[index] = None;
        self.vacant.push(index);
        self.bytes -= bytes;
    }

    fn find_next_unpinned(&self, mut index: Option<usize>) -> usize {
        while let Some(candidate) = index {
            let entry = self.entry(candidate);
            if !entry.pinned {
                return candidate;
            }
            index = entry.next;
        }
        unreachable!("unpinned count guarantees a successor")
    }

    fn entry(&self, index: usize) -> &Entry<V> {
        self.entries[index]
            .as_ref()
            .expect("LRU index must point to a resident entry")
    }

    fn entry_mut(&mut self, index: usize) -> &mut Entry<V> {
        self.entries[index]
            .as_mut()
            .expect("LRU index must point to a resident entry")
    }

    fn used_bytes(&self) -> u64 {
        self.bytes
    }

    fn budget_bytes(&self) -> u64 {
        self.budget
    }

    fn retain_keys(&mut self, mut keep: impl FnMut(&Key) -> bool) {
        let removed: Vec<_> = self.map.keys().filter(|key| !keep(key)).copied().collect();
        for key in removed {
            self.remove(&key);
        }
    }
}

/// Conservative reservation before Viewr has observed a rendered size in the
/// current folder. It covers a typical 61 MP RGBA8 frame.
const DEFAULT_FULL_RESERVATION_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Independent byte budgets for every in-memory payload ring.
pub struct RamCacheBudgets {
    /// Embedded-preview thumbnail RGBA bytes.
    pub thumb_rgba_bytes: u64,
    /// Browse-quality developed RGBA bytes.
    pub browse_rgba_bytes: u64,
    /// Full-resolution developed RGBA bytes.
    pub full_rgba_bytes: u64,
    /// Encoded Browse and Full JPEG bytes.
    pub jpeg_bytes: u64,
}

impl RamCacheBudgets {
    /// Creates explicit independent ring budgets.
    pub const fn new(
        thumb_rgba_bytes: u64,
        browse_rgba_bytes: u64,
        full_rgba_bytes: u64,
        jpeg_bytes: u64,
    ) -> Self {
        Self {
            thumb_rgba_bytes,
            browse_rgba_bytes,
            full_rgba_bytes,
            jpeg_bytes,
        }
    }
}

#[derive(Debug, Clone)]
/// Atomic size-estimate snapshot used by adaptive Full prefetch planning.
pub struct FullPrefetchSnapshot {
    /// Target payload bytes for the dedicated Full RGBA ring.
    pub budget_bytes: u64,
    /// Estimate for an image without a per-index observation.
    pub fallback_bytes: u64,
    /// Exact Full sizes or conservative estimates derived from Browse pixels.
    pub known_bytes: HashMap<usize, u64>,
}

/// Current payload-byte usage for each in-memory cache ring.
///
/// Values exclude allocator, hash-table, `Arc`, and entry metadata overhead.
pub struct RamCacheStats {
    /// Decoded Browse and Full RGBA payload bytes.
    pub rgba_bytes: u64,
    /// Browse-quality portion of [`Self::rgba_bytes`].
    pub browse_rgba_bytes: u64,
    /// Full-resolution portion of [`Self::rgba_bytes`].
    pub full_rgba_bytes: u64,
    /// Encoded JPEG payload bytes.
    pub jpeg_bytes: u64,
    /// Decoded thumbnail RGBA payload bytes.
    pub thumb_bytes: u64,
}

/// Thread-safe, byte-budgeted cache shared by UI and worker threads.
///
/// Thumbnails, Browse RGBA buffers, Full RGBA buffers, and developed JPEGs
/// occupy independent exact-LRU rings. Reads through `get_*` promote an entry;
/// `has_*` probes do not. Pinned entries cannot be evicted, so a ring may
/// temporarily exceed its configured budget when all possible victims are
/// pinned.
///
/// All operations serialize through one mutex. A panic while that mutex is
/// held poisons the cache and causes later operations to panic.
pub struct RamCache {
    inner: Mutex<Inner>,
}

struct Inner {
    thumbs: ByteLru<Arc<PixelBuf>>,
    browse_rgba: ByteLru<Arc<PixelBuf>>,
    full_rgba: ByteLru<Arc<PixelBuf>>,
    jpeg: ByteLru<Arc<Vec<u8>>>,
    pinned: HashSet<Key>,
    full_working_set: HashSet<Key>,
    observed_browse_bytes: HashMap<usize, u64>,
    observed_full_bytes: HashMap<usize, u64>,
}

impl RamCache {
    /// Creates empty thumbnail, developed-RGBA, and JPEG rings with independent
    /// byte budgets.
    ///
    /// A zero budget is valid: unpinned inserts are immediately evicted.
    pub fn new(thumb_budget: u64, rgba_budget: u64, jpeg_budget: u64) -> Self {
        Self::with_budgets(RamCacheBudgets::new(
            thumb_budget,
            rgba_budget,
            rgba_budget,
            jpeg_budget,
        ))
    }

    /// Creates empty rings from explicit independent byte budgets.
    pub fn with_budgets(budgets: RamCacheBudgets) -> Self {
        Self {
            inner: Mutex::new(Inner {
                thumbs: ByteLru::new(budgets.thumb_rgba_bytes),
                browse_rgba: ByteLru::new(budgets.browse_rgba_bytes),
                full_rgba: ByteLru::new(budgets.full_rgba_bytes),
                jpeg: ByteLru::new(budgets.jpeg_bytes),
                pinned: HashSet::new(),
                full_working_set: HashSet::new(),
                observed_browse_bytes: HashMap::new(),
                observed_full_bytes: HashMap::new(),
            }),
        }
    }

    /// Replaces the set of keys protected against eviction.
    ///
    /// Pins apply to all rings and also affect entries inserted later. Removing
    /// pins immediately evicts the oldest newly eligible entries if a ring is
    /// over budget.
    pub fn set_pins(&self, keys: impl IntoIterator<Item = Key>) {
        let mut inner = self.inner.lock().unwrap();
        let new_pins: HashSet<_> = keys.into_iter().collect();
        let removed: Vec<_> = inner.pinned.difference(&new_pins).copied().collect();
        let added: Vec<_> = new_pins.difference(&inner.pinned).copied().collect();

        for key in removed {
            inner.thumbs.set_pinned(&key, false);
            inner.browse_rgba.set_pinned(&key, false);
            inner.full_rgba.set_pinned(&key, false);
            inner.jpeg.set_pinned(&key, false);
        }
        for key in added {
            inner.thumbs.set_pinned(&key, true);
            inner.browse_rgba.set_pinned(&key, true);
            inner.full_rgba.set_pinned(&key, true);
            inner.jpeg.set_pinned(&key, true);
        }
        inner.pinned = new_pins;
        inner.thumbs.evict_over_budget();
        inner.browse_rgba.evict_over_budget();
        inner.full_rgba.evict_over_budget();
        inner.jpeg.evict_over_budget();
    }

    /// Atomically installs navigation pins and the desired Full working set.
    ///
    /// Full entries outside `full_keys` are removed immediately even when the
    /// ring is below budget. Later worker completions for those stale keys are
    /// rejected by [`insert_rgba`](Self::insert_rgba) under the same mutex.
    pub fn set_navigation_policy(
        &self,
        pins: impl IntoIterator<Item = Key>,
        full_keys: impl IntoIterator<Item = Key>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let new_pins: HashSet<_> = pins.into_iter().collect();
        let removed: Vec<_> = inner.pinned.difference(&new_pins).copied().collect();
        let added: Vec<_> = new_pins.difference(&inner.pinned).copied().collect();
        for key in removed {
            inner.thumbs.set_pinned(&key, false);
            inner.browse_rgba.set_pinned(&key, false);
            inner.full_rgba.set_pinned(&key, false);
            inner.jpeg.set_pinned(&key, false);
        }
        for key in added {
            inner.thumbs.set_pinned(&key, true);
            inner.browse_rgba.set_pinned(&key, true);
            inner.full_rgba.set_pinned(&key, true);
            inner.jpeg.set_pinned(&key, true);
        }
        inner.pinned = new_pins;
        let full_working_set: HashSet<_> = full_keys
            .into_iter()
            .filter(|(_, tier)| *tier == Tier::Full)
            .collect();
        inner
            .full_rgba
            .retain_keys(|key| full_working_set.contains(key));
        inner.full_working_set = full_working_set;
        inner.thumbs.evict_over_budget();
        inner.browse_rgba.evict_over_budget();
        inner.full_rgba.evict_over_budget();
        inner.jpeg.evict_over_budget();
    }

    /// Returns and promotes the decoded RGBA entry for `key`.
    ///
    /// [`Tier::Thumb`] addresses the thumbnail ring; other tiers address the
    /// developed-RGBA ring. The returned `Arc` keeps the payload alive even if
    /// it is subsequently evicted.
    pub fn get_rgba(&self, key: Key) -> Option<Arc<PixelBuf>> {
        let mut inner = self.inner.lock().unwrap();
        match key.1 {
            Tier::Thumb => inner.thumbs.get(&key),
            Tier::Browse => inner.browse_rgba.get(&key),
            Tier::Full => inner.full_rgba.get(&key),
        }
    }

    /// Tests for a decoded RGBA entry without changing LRU recency.
    pub fn has_rgba(&self, key: Key) -> bool {
        let inner = self.inner.lock().unwrap();
        match key.1 {
            Tier::Thumb => inner.thumbs.contains(&key),
            Tier::Browse => inner.browse_rgba.contains(&key),
            Tier::Full => inner.full_rgba.contains(&key),
        }
    }

    /// Returns and promotes an encoded JPEG entry.
    pub fn get_jpeg(&self, key: Key) -> Option<Arc<Vec<u8>>> {
        self.inner.lock().unwrap().jpeg.get(&key)
    }

    /// Tests for an encoded JPEG entry without changing LRU recency.
    pub fn has_jpeg(&self, key: Key) -> bool {
        self.inner.lock().unwrap().jpeg.contains(&key)
    }

    /// Inserts or replaces a decoded RGBA entry and enforces its ring budget.
    ///
    /// Payload size is the buffer's actual [`PixelBuf::byte_len`], even if its
    /// dimensions and storage length are inconsistent.
    pub fn insert_rgba(&self, key: Key, buf: Arc<PixelBuf>) -> bool {
        let bytes = buf.byte_len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = inner.pinned.contains(&key);
        match key.1 {
            Tier::Thumb => {
                inner.thumbs.insert(key, buf, bytes, pinned);
                inner.thumbs.contains(&key)
            }
            Tier::Browse => {
                inner.observed_browse_bytes.insert(key.0, bytes);
                inner.browse_rgba.insert(key, buf, bytes, pinned);
                inner.browse_rgba.contains(&key)
            }
            Tier::Full => {
                inner.observed_full_bytes.insert(key.0, bytes);
                if !inner.full_working_set.contains(&key) {
                    return false;
                }
                inner.full_rgba.insert(key, buf, bytes, pinned);
                inner.full_rgba.contains(&key)
            }
        }
    }

    /// Inserts or replaces an encoded JPEG entry and enforces the JPEG budget.
    pub fn insert_jpeg(&self, key: Key, bytes_vec: Arc<Vec<u8>>) {
        let bytes = bytes_vec.len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = inner.pinned.contains(&key);
        inner.jpeg.insert(key, bytes_vec, bytes, pinned);
    }

    pub(crate) fn remove_jpeg(&self, key: Key) -> Option<Arc<Vec<u8>>> {
        self.inner.lock().unwrap().jpeg.remove(&key)
    }

    /// Returns a payload-byte snapshot for all four rings.
    pub fn stats(&self) -> RamCacheStats {
        let inner = self.inner.lock().unwrap();
        let browse_rgba_bytes = inner.browse_rgba.used_bytes();
        let full_rgba_bytes = inner.full_rgba.used_bytes();
        RamCacheStats {
            rgba_bytes: browse_rgba_bytes.saturating_add(full_rgba_bytes),
            browse_rgba_bytes,
            full_rgba_bytes,
            jpeg_bytes: inner.jpeg.used_bytes(),
            thumb_bytes: inner.thumbs.used_bytes(),
        }
    }

    /// Returns one lock-consistent Full budget and size-estimate snapshot.
    pub fn full_prefetch_snapshot(&self) -> FullPrefetchSnapshot {
        let inner = self.inner.lock().unwrap();
        let mut known_bytes = inner.observed_full_bytes.clone();
        for (&index, &bytes) in &inner.observed_browse_bytes {
            known_bytes
                .entry(index)
                .or_insert_with(|| bytes.saturating_mul(5));
        }
        let fallback_bytes = known_bytes
            .values()
            .copied()
            .max()
            .unwrap_or(DEFAULT_FULL_RESERVATION_BYTES)
            .max(1);
        FullPrefetchSnapshot {
            budget_bytes: inner.full_rgba.budget_bytes(),
            fallback_bytes,
            known_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[derive(Clone, Copy, Debug)]
    struct ModelEntry {
        value: u64,
        bytes: u64,
        last_use: u64,
        pinned: bool,
    }

    struct ModelLru {
        map: HashMap<Key, ModelEntry>,
        budget: u64,
        bytes: u64,
        clock: u64,
    }

    impl ModelLru {
        fn new(budget: u64) -> Self {
            Self {
                map: HashMap::new(),
                budget,
                bytes: 0,
                clock: 0,
            }
        }

        fn get(&mut self, key: &Key) -> Option<u64> {
            self.clock += 1;
            self.map.get_mut(key).map(|entry| {
                entry.last_use = self.clock;
                entry.value
            })
        }

        fn insert(&mut self, key: Key, value: u64, bytes: u64, pinned: bool) {
            self.clock += 1;
            if let Some(old) = self.map.insert(
                key,
                ModelEntry {
                    value,
                    bytes,
                    last_use: self.clock,
                    pinned,
                },
            ) {
                self.bytes -= old.bytes;
            }
            self.bytes += bytes;
            self.evict_over_budget();
        }

        fn set_pins(&mut self, pins: &HashSet<Key>) {
            for (key, entry) in &mut self.map {
                entry.pinned = pins.contains(key);
            }
            self.evict_over_budget();
        }

        fn evict_over_budget(&mut self) {
            while self.bytes > self.budget {
                let victim = self
                    .map
                    .iter()
                    .filter(|(_, entry)| !entry.pinned)
                    .min_by_key(|(_, entry)| entry.last_use)
                    .map(|(key, _)| *key);
                let Some(victim) = victim else {
                    break;
                };
                self.bytes -= self.map.remove(&victim).unwrap().bytes;
            }
        }
    }

    fn buf(bytes: usize) -> Arc<PixelBuf> {
        Arc::new(PixelBuf {
            width: 1,
            height: 1,
            rgba: vec![0; bytes],
        })
    }

    #[test]
    fn evicts_lru_when_over_byte_budget() {
        let cache = RamCache::new(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60)); // 120 > 100 → evict LRU (0)
        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn get_refreshes_recency() {
        let cache = RamCache::new(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(40));
        cache.insert_rgba((1, Tier::Browse), buf(40));
        cache.get_rgba((0, Tier::Browse)); // 0 now most-recent
        cache.insert_rgba((2, Tier::Browse), buf(40)); // evicts 1, not 0
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn pinned_keys_survive_eviction() {
        let cache = RamCache::new(0, 100, 0);
        cache.set_pins([(0, Tier::Browse)]);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60));
        // 0 is pinned → 1 must be the victim even though it's newer.
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn replacing_a_key_accounts_bytes_once() {
        let cache = RamCache::new(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(80));
        cache.insert_rgba((0, Tier::Browse), buf(90)); // replace, not add
        assert_eq!(cache.stats().rgba_bytes, 90);
    }

    #[test]
    fn unpinning_evicts_entries_that_exceed_the_budget() {
        let cache = RamCache::new(0, 100, 0);
        cache.set_pins([(0, Tier::Browse), (1, Tier::Browse)]);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60));
        assert_eq!(cache.stats().rgba_bytes, 120);

        cache.set_pins([(1, Tier::Browse)]);

        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((1, Tier::Browse)));
        assert_eq!(cache.stats().rgba_bytes, 60);
    }

    #[test]
    fn stale_pinned_recency_is_preserved_for_later_eviction() {
        let cache = RamCache::new(0, 20, 0);
        cache.insert_rgba((0, Tier::Browse), buf(10));
        cache.insert_rgba((1, Tier::Browse), buf(10));
        cache.set_pins([(0, Tier::Browse)]);

        cache.get_rgba((1, Tier::Browse));
        cache.insert_rgba((2, Tier::Browse), buf(10));
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));

        cache.set_pins([]);
        cache.insert_rgba((3, Tier::Browse), buf(10));
        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((2, Tier::Browse)));
        assert!(cache.has_rgba((3, Tier::Browse)));
    }

    #[test]
    fn access_while_pinned_still_refreshes_recency() {
        let cache = RamCache::new(0, 20, 0);
        cache.insert_rgba((0, Tier::Browse), buf(10));
        cache.insert_rgba((1, Tier::Browse), buf(10));
        cache.set_pins([(0, Tier::Browse)]);

        cache.get_rgba((0, Tier::Browse));
        cache.set_pins([]);
        cache.insert_rgba((2, Tier::Browse), buf(10));

        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));
        assert!(cache.has_rgba((2, Tier::Browse)));
    }

    #[test]
    fn optimized_lru_matches_full_scan_reference_model() {
        let mut actual = ByteLru::new(512);
        let mut model = ModelLru::new(512);
        let mut pins = HashSet::new();
        let mut random = 0x4d59_5df4_d0f3_3173_u64;

        for step in 0..20_000_u64 {
            // Fixed-seed xorshift64 makes failures exactly reproducible without
            // adding a random-number dependency to the crate.
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;

            let key = model_key((random as usize) % 257);
            match random % 7 {
                0..=2 => {
                    let bytes = (random.rotate_left(19) % 47) + 1;
                    let value = step ^ random;
                    let pinned = pins.contains(&key);
                    actual.insert(key, value, bytes, pinned);
                    model.insert(key, value, bytes, pinned);
                }
                3..=4 => assert_eq!(actual.get(&key), model.get(&key)),
                5 => {
                    let mut new_pins = HashSet::new();
                    for offset in 0_u32..8 {
                        let candidate = model_key(
                            ((random.rotate_left(offset * 7) as usize) + offset as usize) % 257,
                        );
                        new_pins.insert(candidate);
                    }

                    for removed in pins.difference(&new_pins) {
                        actual.set_pinned(removed, false);
                    }
                    for added in new_pins.difference(&pins) {
                        actual.set_pinned(added, true);
                    }
                    actual.evict_over_budget();
                    pins = new_pins;
                    model.set_pins(&pins);
                }
                _ => assert_eq!(actual.contains(&key), model.map.contains_key(&key)),
            }

            assert_lru_matches_model(&actual, &model, step);
        }
    }

    #[test]
    fn cache_rings_have_independent_budgets_and_stats() {
        let cache = RamCache::new(8, 12, 6);
        cache.insert_rgba((0, Tier::Thumb), buf(8));
        cache.insert_rgba((0, Tier::Browse), buf(12));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(vec![1; 6]));

        let stats = cache.stats();
        assert_eq!(stats.thumb_bytes, 8);
        assert_eq!(stats.rgba_bytes, 12);
        assert_eq!(stats.jpeg_bytes, 6);
        assert!(cache.has_rgba((0, Tier::Thumb)));
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_jpeg((0, Tier::Browse)));
    }

    #[test]
    fn removing_pinned_jpeg_repairs_lru_accounting() {
        let cache = RamCache::new(0, 0, 10);
        let pinned = (0, Tier::Browse);
        let other = (1, Tier::Browse);
        cache.set_pins([pinned]);
        cache.insert_jpeg(pinned, Arc::new(vec![1; 6]));
        cache.insert_jpeg(other, Arc::new(vec![2; 4]));

        assert_eq!(*cache.remove_jpeg(pinned).unwrap(), vec![1; 6]);
        assert!(!cache.has_jpeg(pinned));
        assert_eq!(cache.stats().jpeg_bytes, 4);

        cache.insert_jpeg((2, Tier::Browse), Arc::new(vec![3; 7]));
        assert!(!cache.has_jpeg(other));
        assert!(cache.has_jpeg((2, Tier::Browse)));
        assert_eq!(cache.stats().jpeg_bytes, 7);
    }

    #[test]
    fn oversized_unpinned_entry_is_immediately_evicted() {
        let cache = RamCache::new(0, 10, 0);
        cache.insert_rgba((0, Tier::Full), buf(11));
        assert!(!cache.has_rgba((0, Tier::Full)));
        assert_eq!(cache.stats().rgba_bytes, 0);
    }

    #[test]
    fn concurrent_cache_access_preserves_budget_accounting() {
        let cache = Arc::new(RamCache::new(0, 256, 128));
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let cache = cache.clone();
                thread::spawn(move || {
                    for step in 0..500 {
                        let index = (worker * 17 + step) % 32;
                        let key = (index, Tier::Browse);
                        cache.insert_rgba(key, buf(16));
                        let _ = cache.get_rgba(key);
                        cache.insert_jpeg(key, Arc::new(vec![step as u8; 8]));
                        let _ = cache.get_jpeg(key);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let stats = cache.stats();
        assert!(stats.rgba_bytes <= 256);
        assert!(stats.jpeg_bytes <= 128);
    }

    #[test]
    fn full_working_set_evicts_stale_full_without_touching_other_rings() {
        let cache = RamCache::with_budgets(RamCacheBudgets::new(16, 16, 64, 16));
        let old_full = (0, Tier::Full);
        let kept_full = (1, Tier::Full);
        cache.set_navigation_policy(
            [(0, Tier::Thumb), (0, Tier::Browse), kept_full],
            [old_full, kept_full],
        );
        assert!(cache.insert_rgba((0, Tier::Thumb), buf(8)));
        assert!(cache.insert_rgba((0, Tier::Browse), buf(8)));
        assert!(cache.insert_rgba(old_full, buf(8)));
        assert!(cache.insert_rgba(kept_full, buf(8)));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(vec![1; 8]));

        cache.set_navigation_policy(
            [(0, Tier::Thumb), (0, Tier::Browse), kept_full],
            [kept_full],
        );

        assert!(!cache.has_rgba(old_full));
        assert!(cache.has_rgba(kept_full));
        assert!(cache.has_rgba((0, Tier::Thumb)));
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_jpeg((0, Tier::Browse)));
    }

    #[test]
    fn late_full_completion_outside_the_working_set_is_rejected() {
        let cache = RamCache::with_budgets(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(1, Tier::Full)]);

        assert!(!cache.insert_rgba((0, Tier::Full), buf(8)));
        assert!(cache.insert_rgba((1, Tier::Full), buf(8)));
        assert!(!cache.has_rgba((0, Tier::Full)));
        assert!(cache.has_rgba((1, Tier::Full)));
    }

    #[test]
    fn full_eviction_releases_the_cache_arc_owner() {
        let cache = RamCache::with_budgets(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        let pixels = buf(8);
        let weak = Arc::downgrade(&pixels);
        assert!(cache.insert_rgba((0, Tier::Full), pixels));

        cache.set_navigation_policy([], []);

        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn full_prefetch_snapshot_uses_exact_full_and_conservative_browse_estimates() {
        let cache = RamCache::with_budgets(RamCacheBudgets::new(0, 64, 1_000, 0));
        cache.set_navigation_policy([], [(2, Tier::Full)]);
        assert!(cache.insert_rgba((1, Tier::Browse), buf(10)));
        assert!(cache.insert_rgba((2, Tier::Full), buf(24)));

        let snapshot = cache.full_prefetch_snapshot();
        assert_eq!(snapshot.budget_bytes, 1_000);
        assert_eq!(snapshot.known_bytes.get(&1), Some(&50));
        assert_eq!(snapshot.known_bytes.get(&2), Some(&24));
        assert_eq!(snapshot.fallback_bytes, 50);
    }

    fn model_key(value: usize) -> Key {
        let tier = match value % 3 {
            0 => Tier::Thumb,
            1 => Tier::Browse,
            _ => Tier::Full,
        };
        (value / 3, tier)
    }

    fn assert_lru_matches_model(actual: &ByteLru<u64>, model: &ModelLru, step: u64) {
        assert_eq!(actual.bytes, model.bytes, "byte count at step {step}");
        assert_eq!(actual.clock, model.clock, "clock at step {step}");
        assert_eq!(actual.map.len(), model.map.len(), "length at step {step}");

        let summed_bytes = actual
            .entries
            .iter()
            .flatten()
            .map(|entry| entry.bytes)
            .sum::<u64>();
        assert_eq!(actual.bytes, summed_bytes, "byte sum at step {step}");
        for (key, expected) in &model.map {
            let index = *actual
                .map
                .get(key)
                .unwrap_or_else(|| panic!("missing key {key:?} at step {step}"));
            let found = actual.entry(index);
            assert_eq!(found.key, *key, "stored key for {key:?} at {step}");
            assert_eq!(found.value, expected.value, "value for {key:?} at {step}");
            assert_eq!(found.bytes, expected.bytes, "bytes for {key:?} at {step}");
            assert_eq!(
                found.last_use, expected.last_use,
                "recency for {key:?} at {step}"
            );
            assert_eq!(
                found.pinned, expected.pinned,
                "pin state for {key:?} at {step}"
            );
        }

        let expected_oldest = model
            .map
            .iter()
            .filter(|(_, entry)| !entry.pinned)
            .min_by_key(|(_, entry)| entry.last_use)
            .map(|(key, _)| *key);
        let actual_oldest = actual.oldest_unpinned.map(|index| actual.entry(index).key);
        assert_eq!(
            actual_oldest, expected_oldest,
            "oldest unpinned key at step {step}"
        );
        assert_eq!(
            actual.unpinned,
            actual
                .entries
                .iter()
                .flatten()
                .filter(|entry| !entry.pinned)
                .count(),
            "unpinned count at step {step}"
        );

        let mut seen = HashSet::new();
        let mut previous = None;
        let mut previous_use = None;
        let mut cursor = actual.lru;
        while let Some(index) = cursor {
            let entry = actual.entry(index);
            assert!(
                seen.insert(index),
                "LRU cycle through {:?} at step {step}",
                entry.key
            );
            assert_eq!(
                actual.map.get(&entry.key),
                Some(&index),
                "map index for {:?} at {step}",
                entry.key
            );
            assert_eq!(
                entry.prev, previous,
                "back-link for {:?} at {step}",
                entry.key
            );
            if let Some(previous_use) = previous_use {
                assert!(
                    previous_use < entry.last_use,
                    "recency order for {:?} at step {step}",
                    entry.key
                );
            }
            previous = Some(index);
            previous_use = Some(entry.last_use);
            cursor = entry.next;
        }
        assert_eq!(previous, actual.mru, "MRU at step {step}");
        assert_eq!(seen.len(), actual.map.len(), "linked keys at step {step}");
        assert_eq!(
            actual.entries.iter().flatten().count(),
            actual.map.len(),
            "resident arena slots at step {step}"
        );
        let vacant: HashSet<_> = actual.vacant.iter().copied().collect();
        assert_eq!(
            vacant.len(),
            actual.vacant.len(),
            "duplicate vacant slot at step {step}"
        );
        assert!(
            vacant.iter().all(|&index| actual.entries[index].is_none()),
            "occupied vacant slot at step {step}"
        );
        assert_eq!(actual.lru.is_none(), actual.map.is_empty());
        assert_eq!(actual.mru.is_none(), actual.map.is_empty());
    }
}
