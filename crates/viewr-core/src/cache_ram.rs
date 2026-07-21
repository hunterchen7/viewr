//! Byte-budgeted RAM cache rings.
//!
//! Ring 1: decoded RGBA (instant display). Ring 2: encoded JPEG bytes of
//! developed images (~10–20× smaller; cheap re-inflate). Both are LRU by
//! bytes — never by image count. Pinned keys (current ±1) are never
//! evicted. Thumbs live in their own small RGBA ring and have no JPEG
//! form.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::types::{PixelBuf, Tier};

pub type Key = (usize, Tier);

struct Entry<V> {
    value: V,
    bytes: u64,
    last_use: u64,
}

struct ByteLru<V> {
    map: HashMap<Key, Entry<V>>,
    budget: u64,
    bytes: u64,
    clock: u64,
}

impl<V: Clone> ByteLru<V> {
    fn new(budget: u64) -> Self {
        Self {
            map: HashMap::new(),
            budget,
            bytes: 0,
            clock: 0,
        }
    }

    fn get(&mut self, key: &Key) -> Option<V> {
        self.clock += 1;
        let clock = self.clock;
        self.map.get_mut(key).map(|e| {
            e.last_use = clock;
            e.value.clone()
        })
    }

    fn contains(&self, key: &Key) -> bool {
        self.map.contains_key(key)
    }

    fn insert(&mut self, key: Key, value: V, bytes: u64, pinned: &HashSet<Key>) {
        self.clock += 1;
        if let Some(old) = self.map.insert(
            key,
            Entry {
                value,
                bytes,
                last_use: self.clock,
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        self.evict_over_budget(pinned);
    }

    fn evict_over_budget(&mut self, pinned: &HashSet<Key>) {
        while self.bytes > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(k, _)| !pinned.contains(k))
                .min_by_key(|(_, e)| e.last_use)
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    if let Some(e) = self.map.remove(&k) {
                        self.bytes -= e.bytes;
                    }
                }
                None => break, // everything pinned
            }
        }
    }

    fn used_bytes(&self) -> u64 {
        self.bytes
    }
}

pub struct RamCacheStats {
    pub rgba_bytes: u64,
    pub jpeg_bytes: u64,
    pub thumb_bytes: u64,
}

/// Shared between UI thread and workers.
pub struct RamCache {
    inner: Mutex<Inner>,
}

struct Inner {
    thumbs: ByteLru<Arc<PixelBuf>>,
    rgba: ByteLru<Arc<PixelBuf>>,
    jpeg: ByteLru<Arc<Vec<u8>>>,
    pinned: HashSet<Key>,
}

impl RamCache {
    pub fn new(thumb_budget: u64, rgba_budget: u64, jpeg_budget: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                thumbs: ByteLru::new(thumb_budget),
                rgba: ByteLru::new(rgba_budget),
                jpeg: ByteLru::new(jpeg_budget),
                pinned: HashSet::new(),
            }),
        }
    }

    /// Pin keys against eviction (the current image and its neighbors).
    /// Replaces the previous pin set.
    pub fn set_pins(&self, keys: impl IntoIterator<Item = Key>) {
        let mut inner = self.inner.lock().unwrap();
        inner.pinned = keys.into_iter().collect();
        let pinned = std::mem::take(&mut inner.pinned);
        inner.thumbs.evict_over_budget(&pinned);
        inner.rgba.evict_over_budget(&pinned);
        inner.jpeg.evict_over_budget(&pinned);
        inner.pinned = pinned;
    }

    pub fn get_rgba(&self, key: Key) -> Option<Arc<PixelBuf>> {
        let mut inner = self.inner.lock().unwrap();
        match key.1 {
            Tier::Thumb => inner.thumbs.get(&key),
            _ => inner.rgba.get(&key),
        }
    }

    pub fn has_rgba(&self, key: Key) -> bool {
        let inner = self.inner.lock().unwrap();
        match key.1 {
            Tier::Thumb => inner.thumbs.contains(&key),
            _ => inner.rgba.contains(&key),
        }
    }

    pub fn get_jpeg(&self, key: Key) -> Option<Arc<Vec<u8>>> {
        self.inner.lock().unwrap().jpeg.get(&key)
    }

    pub fn has_jpeg(&self, key: Key) -> bool {
        self.inner.lock().unwrap().jpeg.contains(&key)
    }

    pub fn insert_rgba(&self, key: Key, buf: Arc<PixelBuf>) {
        let bytes = buf.byte_len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = std::mem::take(&mut inner.pinned);
        match key.1 {
            Tier::Thumb => inner.thumbs.insert(key, buf, bytes, &pinned),
            _ => inner.rgba.insert(key, buf, bytes, &pinned),
        }
        inner.pinned = pinned;
    }

    pub fn insert_jpeg(&self, key: Key, bytes_vec: Arc<Vec<u8>>) {
        let bytes = bytes_vec.len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = std::mem::take(&mut inner.pinned);
        inner.jpeg.insert(key, bytes_vec, bytes, &pinned);
        inner.pinned = pinned;
    }

    pub fn stats(&self) -> RamCacheStats {
        let inner = self.inner.lock().unwrap();
        RamCacheStats {
            rgba_bytes: inner.rgba.used_bytes(),
            jpeg_bytes: inner.jpeg.used_bytes(),
            thumb_bytes: inner.thumbs.used_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
