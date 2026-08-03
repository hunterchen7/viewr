//! Byte-budgeted RAM cache rings.
//!
//! Independent rings hold thumbnail, Browse RGBA, Full RGBA, and encoded JPEG
//! payloads. Every ring is exact LRU by bytes, never by image count. Pinned
//! keys (current ±1) are never evicted. The Full ring additionally follows an
//! explicit navigation working set that gates admission: late completions for
//! keys outside it cannot enter the ring. Entries already resident when they
//! fall out of the working set are retained until byte pressure evicts them,
//! so direction flips and jumps revisit them as RAM hits instead of paying a
//! redevelop.

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

/// Defers the common single replacement or eviction without allocating. A
/// rare insert that removes more than one value spills later owners to `rest`.
struct Removed<V> {
    first: Option<V>,
    rest: Vec<V>,
}

impl<V> Removed<V> {
    fn new() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }

    fn push(&mut self, value: V) {
        if self.first.is_none() {
            self.first = Some(value);
        } else {
            self.rest.push(value);
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }
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

    fn insert(&mut self, key: Key, value: V, bytes: u64, pinned: bool) -> Removed<V> {
        let mut removed = Removed::new();
        self.insert_and_get_index(key, value, bytes, pinned, &mut removed);
        removed
    }

    fn insert_retained(
        &mut self,
        key: Key,
        value: V,
        bytes: u64,
        pinned: bool,
    ) -> (bool, Removed<V>) {
        let mut removed = Removed::new();
        let index = self.insert_and_get_index(key, value, bytes, pinned, &mut removed);
        (self.entries[index].is_some(), removed)
    }

    fn insert_and_get_index(
        &mut self,
        key: Key,
        value: V,
        bytes: u64,
        pinned: bool,
        removed: &mut Removed<V>,
    ) -> usize {
        self.clock += 1;
        let clock = self.clock;

        let index = if let Some(index) = self.map.get(&key).copied() {
            self.move_to_mru(index);
            let old_bytes = self.entry(index).bytes;
            self.bytes -= old_bytes;
            let entry = self.entry_mut(index);
            debug_assert_eq!(entry.pinned, pinned);
            removed.push(std::mem::replace(&mut entry.value, value));
            entry.bytes = bytes;
            entry.last_use = clock;
            index
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
            index
        };
        self.bytes += bytes;
        while self.bytes > self.budget {
            let Some(victim) = self.oldest_unpinned else {
                break;
            };
            removed.push(self.remove_unpinned(victim));
        }
        index
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

    #[cfg(test)]
    fn evict_over_budget(&mut self) {
        self.evict_over_budget_into(&mut Vec::new());
    }

    fn evict_over_budget_into(&mut self, removed: &mut Vec<V>) {
        while self.bytes > self.budget {
            let Some(victim) = self.oldest_unpinned else {
                break; // everything pinned
            };
            removed.push(self.remove_unpinned(victim));
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

    fn remove_unpinned(&mut self, index: usize) -> V {
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
        let removed = self.entries[index]
            .take()
            .expect("LRU index must point to a resident entry");
        self.vacant.push(index);
        self.bytes -= bytes;
        removed.value
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
}

/// Conservative reservation before Viewr has observed a rendered size in the
/// current folder. It covers a typical 61 MP RGBA8 frame.
const DEFAULT_FULL_RESERVATION_BYTES: u64 = 256 * 1024 * 1024;
/// Conservative Browse reservation before this folder supplies a rendered
/// size. Browse uses roughly one quarter of a Full RGBA frame.
const DEFAULT_BROWSE_RESERVATION_BYTES: u64 = DEFAULT_FULL_RESERVATION_BYTES / 4;

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
    /// Per-image Full payload sizes or Browse-derived estimates, shared without
    /// a per-navigation clone.
    pub per_index_bytes: Arc<HashMap<usize, u64>>,
}

#[derive(Debug, Clone)]
/// Atomic size-estimate snapshot used to bound Browse navigation planning.
pub struct BrowsePrefetchSnapshot {
    /// Target payload bytes for the dedicated Browse RGBA ring.
    pub budget_bytes: u64,
    /// Estimate for an image without a per-index Browse observation.
    pub fallback_bytes: u64,
    /// Exact Browse payload observations, shared without a per-navigation
    /// clone.
    pub per_index_bytes: Arc<HashMap<usize, u64>>,
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

/// Provisional visible rows of one in-progress Full-tier decode.
///
/// A rehydrate publishes the band after its first decode phase so the UI can
/// upload sharp visible tiles before the rest of the frame finishes. The
/// rows are copies of the exact bytes the finished frame will contain, so a
/// tile built from a band equals the tile built later from the full buffer.
pub struct FullBand {
    /// Width of the full image the band was cut from.
    pub full_width: u32,
    /// Height of the full image the band was cut from.
    pub full_height: u32,
    /// First image row covered by [`Self::buf`].
    pub y0: u32,
    /// Tightly packed RGBA rows `y0..y0 + buf.height` at full image width.
    pub buf: PixelBuf,
}

/// Thread-safe, byte-budgeted cache shared by UI and worker threads.
///
/// Thumbnails, Browse RGBA buffers, Full RGBA buffers, and developed JPEGs
/// occupy independent exact-LRU rings. Reads through `get_*` promote an entry;
/// `has_*` probes do not. Pinned entries cannot be evicted, so a ring may
/// temporarily exceed its configured budget when all possible victims are
/// pinned.
///
/// A separate single-slot side channel holds at most one [`FullBand`] — the
/// visible rows of the Full decode currently in flight. The slot is outside
/// every ring budget (bounded to one band, lifetime ≈ the tail of one
/// decode): it is replaced by the next publication, cleared when the matching
/// Full RGBA entry is installed, and cleared when its image leaves the
/// desired Full working set.
///
/// All ring operations serialize through one mutex; the band slot has its
/// own. When both are taken the order is always ring mutex → band mutex. A
/// panic while a mutex is held poisons the cache and causes later operations
/// to panic.
pub struct RamCache {
    inner: Mutex<Inner>,
    band: Mutex<Option<(usize, Arc<FullBand>)>>,
}

struct Inner {
    thumbs: ByteLru<Arc<PixelBuf>>,
    browse_rgba: ByteLru<Arc<PixelBuf>>,
    full_rgba: ByteLru<Arc<PixelBuf>>,
    jpeg: ByteLru<Arc<Vec<u8>>>,
    pinned: HashSet<Key>,
    full_working_set: HashSet<Key>,
    per_index_full_bytes: Arc<HashMap<usize, u64>>,
    exact_full_indices: HashSet<usize>,
    observed_browse_bytes: Arc<HashMap<usize, u64>>,
    largest_observed_full_estimate: Option<u64>,
    largest_observed_browse_bytes: Option<u64>,
}

impl RamCache {
    /// Creates empty rings from explicit independent byte budgets.
    ///
    /// A zero budget is valid: unpinned inserts are immediately evicted.
    pub fn new(budgets: RamCacheBudgets) -> Self {
        Self {
            inner: Mutex::new(Inner {
                thumbs: ByteLru::new(budgets.thumb_rgba_bytes),
                browse_rgba: ByteLru::new(budgets.browse_rgba_bytes),
                full_rgba: ByteLru::new(budgets.full_rgba_bytes),
                jpeg: ByteLru::new(budgets.jpeg_bytes),
                pinned: HashSet::new(),
                full_working_set: HashSet::new(),
                per_index_full_bytes: Arc::new(HashMap::new()),
                exact_full_indices: HashSet::new(),
                observed_browse_bytes: Arc::new(HashMap::new()),
                largest_observed_full_estimate: None,
                largest_observed_browse_bytes: None,
            }),
            band: Mutex::new(None),
        }
    }

    /// Atomically installs navigation pins and the desired Full working set.
    ///
    /// The working set gates admission only: later worker completions for keys
    /// outside `full_keys` are rejected by [`insert_rgba`](Self::insert_rgba)
    /// under the same mutex. Resident Full entries that fall out of the
    /// working set are deliberately retained while the ring is under budget —
    /// a direction flip or jump revisiting them costs a RAM hit rather than a
    /// redevelop. Losing their pin makes them ordinary LRU victims, so byte
    /// pressure reclaims them before any in-set entry inserted afterwards.
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
        {
            // Band hygiene: the provisional band follows the desired Full
            // working set exactly like resident Full pixels do. Lock order is
            // ring mutex → band mutex.
            let mut band = self.band.lock().unwrap();
            if band
                .as_ref()
                .is_some_and(|(index, _)| !full_working_set.contains(&(*index, Tier::Full)))
            {
                *band = None;
            }
        }
        // Full pixels leaving the working set stay resident as unpinned LRU
        // victims (lazy eviction); only the byte budget reclaims them.
        let mut removed_pixels = Vec::new();
        inner.full_working_set = full_working_set;
        let mut removed_jpegs = Vec::new();
        inner.thumbs.evict_over_budget_into(&mut removed_pixels);
        inner
            .browse_rgba
            .evict_over_budget_into(&mut removed_pixels);
        inner.full_rgba.evict_over_budget_into(&mut removed_pixels);
        inner.jpeg.evict_over_budget_into(&mut removed_jpegs);
        drop(inner);
        // Releasing the final owner of several large Full buffers can return
        // substantial memory to the allocator. Do that after unlocking so
        // cache readers and worker admission do not wait on deallocation.
        drop(removed_pixels);
        drop(removed_jpegs);
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

    /// Reports `(rgba resident, jpeg resident)` for every key under one lock
    /// acquisition, without changing LRU recency.
    ///
    /// Equivalent to calling [`has_rgba`](Self::has_rgba) and
    /// [`has_jpeg`](Self::has_jpeg) per key, but a navigation replan over
    /// hundreds of plan targets takes the cache mutex once instead of twice
    /// per target while decode workers compete for the same lock.
    pub fn probe_residency<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k Key>,
    ) -> Vec<(bool, bool)> {
        let inner = self.inner.lock().unwrap();
        keys.into_iter()
            .map(|key| {
                let rgba = match key.1 {
                    Tier::Thumb => inner.thumbs.contains(key),
                    Tier::Browse => inner.browse_rgba.contains(key),
                    Tier::Full => inner.full_rgba.contains(key),
                };
                (rgba, inner.jpeg.contains(key))
            })
            .collect()
    }

    /// Reports which display payloads exist for one image index under one
    /// lock acquisition, without changing LRU recency:
    /// `(full RGBA, browse RGBA, any JPEG)`.
    ///
    /// Equivalent to four `has_*` probes; per-frame tier indicators use this
    /// to take the cache mutex once per cell instead of up to four times.
    pub fn image_residency(&self, index: usize) -> (bool, bool, bool) {
        let inner = self.inner.lock().unwrap();
        (
            inner.full_rgba.contains(&(index, Tier::Full)),
            inner.browse_rgba.contains(&(index, Tier::Browse)),
            inner.jpeg.contains(&(index, Tier::Browse))
                || inner.jpeg.contains(&(index, Tier::Full)),
        )
    }

    /// Inserts or replaces a decoded RGBA entry and enforces its ring budget.
    ///
    /// Payload size is the buffer's actual [`PixelBuf::byte_len`], even if its
    /// dimensions and storage length are inconsistent. A Full payload outside
    /// the current navigation working set is discarded.
    pub fn insert_rgba(&self, key: Key, buf: Arc<PixelBuf>) {
        let _ = self.insert_rgba_impl::<false>(key, buf);
    }

    /// Inserts pixels and reports whether they remain resident. Full pixels
    /// outside the current navigation working set are rejected atomically.
    pub(crate) fn insert_rgba_if_desired(&self, key: Key, buf: Arc<PixelBuf>) -> bool {
        self.insert_rgba_impl::<true>(key, buf)
    }

    fn insert_rgba_impl<const REPORT_RESIDENCY: bool>(&self, key: Key, buf: Arc<PixelBuf>) -> bool {
        let bytes = buf.byte_len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = inner.pinned.contains(&key);
        let (retained, removed_pixels) = match key.1 {
            Tier::Thumb => {
                if REPORT_RESIDENCY {
                    inner.thumbs.insert_retained(key, buf, bytes, pinned)
                } else {
                    (true, inner.thumbs.insert(key, buf, bytes, pinned))
                }
            }
            Tier::Browse => {
                let estimate = bytes.saturating_mul(5);
                if inner.observed_browse_bytes.get(&key.0) != Some(&bytes) {
                    Arc::make_mut(&mut inner.observed_browse_bytes).insert(key.0, bytes);
                }
                if !inner.exact_full_indices.contains(&key.0)
                    && inner.per_index_full_bytes.get(&key.0) != Some(&estimate)
                {
                    Arc::make_mut(&mut inner.per_index_full_bytes).insert(key.0, estimate);
                }
                inner.largest_observed_browse_bytes = Some(
                    inner
                        .largest_observed_browse_bytes
                        .map_or(bytes, |largest| largest.max(bytes)),
                );
                inner.largest_observed_full_estimate = Some(
                    inner
                        .largest_observed_full_estimate
                        .map_or(estimate, |largest| largest.max(estimate)),
                );
                if REPORT_RESIDENCY {
                    inner.browse_rgba.insert_retained(key, buf, bytes, pinned)
                } else {
                    (true, inner.browse_rgba.insert(key, buf, bytes, pinned))
                }
            }
            Tier::Full => {
                if inner.per_index_full_bytes.get(&key.0) != Some(&bytes) {
                    Arc::make_mut(&mut inner.per_index_full_bytes).insert(key.0, bytes);
                }
                inner.exact_full_indices.insert(key.0);
                inner.largest_observed_full_estimate = Some(
                    inner
                        .largest_observed_full_estimate
                        .map_or(bytes, |largest| largest.max(bytes)),
                );
                if !inner.full_working_set.contains(&key) {
                    return false;
                }
                if REPORT_RESIDENCY {
                    inner.full_rgba.insert_retained(key, buf, bytes, pinned)
                } else {
                    (true, inner.full_rgba.insert(key, buf, bytes, pinned))
                }
            }
        };
        drop(inner);
        drop(removed_pixels);
        if retained && key.1 == Tier::Full {
            // The resident full frame supersedes its provisional band.
            self.clear_full_band(key.0);
        }
        retained
    }

    /// Publishes the provisional visible band of an in-progress Full decode,
    /// replacing whatever band the single slot held before.
    ///
    /// The slot is deliberately outside the ring byte budgets: it is bounded
    /// to one band whose lifetime spans only the tail of one decode. See
    /// [`get_full_band`](Self::get_full_band) for the read side.
    pub fn publish_full_band(&self, index: usize, band: FullBand) {
        let replaced = self.band.lock().unwrap().replace((index, Arc::new(band)));
        drop(replaced);
    }

    /// Returns the published band when it belongs to `index`.
    pub fn get_full_band(&self, index: usize) -> Option<Arc<FullBand>> {
        self.band
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(owner, _)| *owner == index)
            .map(|(_, band)| Arc::clone(band))
    }

    /// Clears the band slot when it belongs to `index`.
    pub(crate) fn clear_full_band(&self, index: usize) {
        let mut band = self.band.lock().unwrap();
        if band.as_ref().is_some_and(|(owner, _)| *owner == index) {
            *band = None;
        }
    }

    /// Inserts or replaces an encoded JPEG entry and enforces the JPEG budget.
    pub fn insert_jpeg(&self, key: Key, bytes_vec: Arc<Vec<u8>>) {
        let bytes = bytes_vec.len() as u64;
        let mut inner = self.inner.lock().unwrap();
        let pinned = inner.pinned.contains(&key);
        let removed = inner.jpeg.insert(key, bytes_vec, bytes, pinned);
        drop(inner);
        drop(removed);
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
        self.prefetch_snapshots().0
    }

    /// Returns one lock-consistent Browse budget and size-estimate snapshot.
    pub fn browse_prefetch_snapshot(&self) -> BrowsePrefetchSnapshot {
        self.prefetch_snapshots().1
    }

    /// Returns atomic Full and Browse planning snapshots under one cache lock.
    pub fn prefetch_snapshots(&self) -> (FullPrefetchSnapshot, BrowsePrefetchSnapshot) {
        let inner = self.inner.lock().unwrap();
        let fallback_bytes = inner
            .largest_observed_full_estimate
            .unwrap_or(DEFAULT_FULL_RESERVATION_BYTES)
            .max(1);
        (
            FullPrefetchSnapshot {
                budget_bytes: inner.full_rgba.budget_bytes(),
                fallback_bytes,
                per_index_bytes: Arc::clone(&inner.per_index_full_bytes),
            },
            BrowsePrefetchSnapshot {
                budget_bytes: inner.browse_rgba.budget_bytes(),
                fallback_bytes: inner
                    .largest_observed_browse_bytes
                    .unwrap_or(DEFAULT_BROWSE_RESERVATION_BYTES)
                    .max(1),
                per_index_bytes: Arc::clone(&inner.observed_browse_bytes),
            },
        )
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

    fn browse_cache(thumb_bytes: u64, browse_bytes: u64, jpeg_bytes: u64) -> RamCache {
        RamCache::new(RamCacheBudgets::new(
            thumb_bytes,
            browse_bytes,
            browse_bytes,
            jpeg_bytes,
        ))
    }

    #[test]
    fn batched_residency_probes_match_individual_probes() {
        let cache = RamCache::new(RamCacheBudgets::new(1_000, 1_000, 1_000, 1_000));
        cache.set_navigation_policy([], [(2, Tier::Full)]);
        cache.insert_rgba((0, Tier::Browse), buf(40));
        cache.insert_rgba((1, Tier::Thumb), buf(40));
        cache.insert_rgba((2, Tier::Full), buf(40));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(vec![0; 40]));
        cache.insert_jpeg((3, Tier::Full), Arc::new(vec![0; 40]));

        let keys = [
            (0, Tier::Browse),
            (1, Tier::Thumb),
            (2, Tier::Full),
            (3, Tier::Full),
            (4, Tier::Browse),
        ];
        let batched = cache.probe_residency(keys.iter());
        for (key, (rgba, jpeg)) in keys.iter().zip(batched) {
            assert_eq!(rgba, cache.has_rgba(*key), "{key:?} rgba");
            assert_eq!(jpeg, cache.has_jpeg(*key), "{key:?} jpeg");
        }

        for index in 0..5 {
            let (full_rgba, browse_rgba, any_jpeg) = cache.image_residency(index);
            assert_eq!(
                full_rgba,
                cache.has_rgba((index, Tier::Full)),
                "{index} full"
            );
            assert_eq!(
                browse_rgba,
                cache.has_rgba((index, Tier::Browse)),
                "{index} browse"
            );
            assert_eq!(
                any_jpeg,
                cache.has_jpeg((index, Tier::Browse)) || cache.has_jpeg((index, Tier::Full)),
                "{index} jpeg"
            );
        }
    }

    #[test]
    fn evicts_lru_when_over_byte_budget() {
        let cache = browse_cache(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60)); // 120 > 100 → evict LRU (0)
        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn get_refreshes_recency() {
        let cache = browse_cache(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(40));
        cache.insert_rgba((1, Tier::Browse), buf(40));
        cache.get_rgba((0, Tier::Browse)); // 0 now most-recent
        cache.insert_rgba((2, Tier::Browse), buf(40)); // evicts 1, not 0
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn pinned_keys_survive_eviction() {
        let cache = browse_cache(0, 100, 0);
        cache.set_navigation_policy([(0, Tier::Browse)], []);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60));
        // 0 is pinned → 1 must be the victim even though it's newer.
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));
    }

    #[test]
    fn replacing_a_key_accounts_bytes_once() {
        let cache = browse_cache(0, 100, 0);
        cache.insert_rgba((0, Tier::Browse), buf(80));
        cache.insert_rgba((0, Tier::Browse), buf(90)); // replace, not add
        assert_eq!(cache.stats().rgba_bytes, 90);
    }

    #[test]
    fn unpinning_evicts_entries_that_exceed_the_budget() {
        let cache = browse_cache(0, 100, 0);
        cache.set_navigation_policy([(0, Tier::Browse), (1, Tier::Browse)], []);
        cache.insert_rgba((0, Tier::Browse), buf(60));
        cache.insert_rgba((1, Tier::Browse), buf(60));
        assert_eq!(cache.stats().rgba_bytes, 120);

        cache.set_navigation_policy([(1, Tier::Browse)], []);

        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((1, Tier::Browse)));
        assert_eq!(cache.stats().rgba_bytes, 60);
    }

    #[test]
    fn stale_pinned_recency_is_preserved_for_later_eviction() {
        let cache = browse_cache(0, 20, 0);
        cache.insert_rgba((0, Tier::Browse), buf(10));
        cache.insert_rgba((1, Tier::Browse), buf(10));
        cache.set_navigation_policy([(0, Tier::Browse)], []);

        cache.get_rgba((1, Tier::Browse));
        cache.insert_rgba((2, Tier::Browse), buf(10));
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(!cache.has_rgba((1, Tier::Browse)));

        cache.set_navigation_policy([], []);
        cache.insert_rgba((3, Tier::Browse), buf(10));
        assert!(!cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_rgba((2, Tier::Browse)));
        assert!(cache.has_rgba((3, Tier::Browse)));
    }

    #[test]
    fn access_while_pinned_still_refreshes_recency() {
        let cache = browse_cache(0, 20, 0);
        cache.insert_rgba((0, Tier::Browse), buf(10));
        cache.insert_rgba((1, Tier::Browse), buf(10));
        cache.set_navigation_policy([(0, Tier::Browse)], []);

        cache.get_rgba((0, Tier::Browse));
        cache.set_navigation_policy([], []);
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
    fn lru_insert_returns_evicted_owner_for_caller_scoped_drop() {
        let mut cache = ByteLru::new(1);
        let first = Arc::new(17_u8);
        let first_weak = Arc::downgrade(&first);
        assert!(
            cache
                .insert((0, Tier::Full), Arc::clone(&first), 1, false)
                .is_empty()
        );
        drop(first);

        let removed = cache.insert((1, Tier::Full), Arc::new(23), 1, false);
        assert_eq!(removed.len(), 1);
        assert!(first_weak.upgrade().is_some());

        drop(removed);
        assert!(first_weak.upgrade().is_none());
    }

    #[test]
    fn cache_rings_have_independent_budgets_and_stats() {
        let cache = browse_cache(8, 12, 6);
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
        let cache = browse_cache(0, 0, 10);
        let pinned = (0, Tier::Browse);
        let other = (1, Tier::Browse);
        cache.set_navigation_policy([pinned], []);
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
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 10, 0));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Full), buf(11));
        assert!(!cache.has_rgba((0, Tier::Full)));
        assert_eq!(cache.stats().rgba_bytes, 0);
    }

    #[test]
    fn concurrent_cache_access_preserves_budget_accounting() {
        let cache = Arc::new(browse_cache(0, 256, 128));
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
    fn out_of_working_set_full_is_retained_until_budget_pressure() {
        let cache = RamCache::new(RamCacheBudgets::new(16, 16, 24, 16));
        let old_full = (0, Tier::Full);
        let kept_full = (1, Tier::Full);
        cache.set_navigation_policy(
            [(0, Tier::Thumb), (0, Tier::Browse), kept_full],
            [old_full, kept_full],
        );
        assert!(cache.insert_rgba_if_desired((0, Tier::Thumb), buf(8)));
        assert!(cache.insert_rgba_if_desired((0, Tier::Browse), buf(8)));
        assert!(cache.insert_rgba_if_desired(old_full, buf(8)));
        assert!(cache.insert_rgba_if_desired(kept_full, buf(8)));
        cache.insert_jpeg((0, Tier::Browse), Arc::new(vec![1; 8]));

        cache.set_navigation_policy(
            [(0, Tier::Thumb), (0, Tier::Browse), kept_full],
            [kept_full, (2, Tier::Full)],
        );

        // Falling out of the working set no longer evicts a resident entry
        // while the ring is under budget: a revisit stays a RAM hit.
        assert!(cache.has_rgba(old_full));
        assert!(cache.has_rgba(kept_full));

        // Byte pressure reclaims the stale unpinned entry before any in-set
        // entry inserted after it, without touching the other rings.
        assert!(cache.insert_rgba_if_desired((2, Tier::Full), buf(16)));
        assert!(!cache.has_rgba(old_full));
        assert!(cache.has_rgba(kept_full));
        assert!(cache.has_rgba((2, Tier::Full)));
        assert!(cache.has_rgba((0, Tier::Thumb)));
        assert!(cache.has_rgba((0, Tier::Browse)));
        assert!(cache.has_jpeg((0, Tier::Browse)));
    }

    #[test]
    fn late_full_completion_outside_the_working_set_is_rejected() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(1, Tier::Full)]);

        assert!(!cache.insert_rgba_if_desired((0, Tier::Full), buf(8)));
        assert!(cache.insert_rgba_if_desired((1, Tier::Full), buf(8)));
        assert!(!cache.has_rgba((0, Tier::Full)));
        assert!(cache.has_rgba((1, Tier::Full)));
    }

    #[test]
    fn full_eviction_releases_the_cache_arc_owner() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        let pixels = buf(8);
        let weak = Arc::downgrade(&pixels);
        assert!(cache.insert_rgba_if_desired((0, Tier::Full), pixels));

        // Leaving the working set keeps the entry resident (lazy retention);
        // only byte pressure may evict it, and eviction must release the
        // cache's owner so the allocation can actually return to the system.
        cache.set_navigation_policy([], [(1, Tier::Full)]);
        assert!(weak.upgrade().is_some());

        assert!(cache.insert_rgba_if_desired((1, Tier::Full), buf(60)));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn full_prefetch_snapshot_uses_exact_full_and_conservative_browse_estimates() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 64, 1_000, 0));
        cache.set_navigation_policy([], [(2, Tier::Full)]);
        assert!(cache.insert_rgba_if_desired((1, Tier::Browse), buf(10)));
        assert!(cache.insert_rgba_if_desired((2, Tier::Full), buf(24)));

        let snapshot = cache.full_prefetch_snapshot();
        assert_eq!(snapshot.budget_bytes, 1_000);
        assert_eq!(snapshot.per_index_bytes.get(&1), Some(&50));
        assert_eq!(snapshot.per_index_bytes.get(&2), Some(&24));
        assert_eq!(snapshot.fallback_bytes, 50);

        let browse = cache.browse_prefetch_snapshot();
        assert_eq!(browse.budget_bytes, 64);
        assert_eq!(browse.fallback_bytes, 10);
        assert_eq!(browse.per_index_bytes.get(&1), Some(&10));
    }

    #[test]
    fn full_prefetch_snapshots_are_stable_across_copy_on_write_updates() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full), (1, Tier::Full)]);
        assert!(cache.insert_rgba_if_desired((0, Tier::Full), buf(8)));
        let before = cache.full_prefetch_snapshot();

        assert!(cache.insert_rgba_if_desired((1, Tier::Full), buf(12)));
        let after = cache.full_prefetch_snapshot();

        assert_eq!(before.per_index_bytes.get(&0), Some(&8));
        assert_eq!(before.per_index_bytes.get(&1), None);
        assert_eq!(after.per_index_bytes.get(&0), Some(&8));
        assert_eq!(after.per_index_bytes.get(&1), Some(&12));
    }

    #[test]
    fn repeated_size_observations_do_not_clone_live_snapshots() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 64, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Browse), buf(8));
        cache.insert_rgba((0, Tier::Full), buf(24));
        let (full_before, browse_before) = cache.prefetch_snapshots();

        cache.insert_rgba((0, Tier::Browse), buf(8));
        cache.insert_rgba((0, Tier::Full), buf(24));
        let (full_after, browse_after) = cache.prefetch_snapshots();

        assert!(Arc::ptr_eq(
            &full_before.per_index_bytes,
            &full_after.per_index_bytes
        ));
        assert!(Arc::ptr_eq(
            &browse_before.per_index_bytes,
            &browse_after.per_index_bytes
        ));
    }

    #[test]
    fn browse_estimate_never_overwrites_an_exact_full_observation() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 64, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full)]);
        cache.insert_rgba((0, Tier::Full), buf(24));
        cache.insert_rgba((0, Tier::Browse), buf(10));

        assert_eq!(
            cache.full_prefetch_snapshot().per_index_bytes.get(&0),
            Some(&24)
        );
    }

    fn band(index: u32) -> FullBand {
        FullBand {
            full_width: 4,
            full_height: 8,
            y0: index,
            buf: PixelBuf {
                width: 4,
                height: 2,
                rgba: vec![index as u8; 4 * 2 * 4],
            },
        }
    }

    #[test]
    fn full_band_slot_is_single_and_index_checked() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 64, 0));
        assert!(cache.get_full_band(3).is_none());

        cache.publish_full_band(3, band(3));
        assert_eq!(cache.get_full_band(3).map(|band| band.y0), Some(3));
        assert!(cache.get_full_band(4).is_none(), "index mismatch");

        // One slot only: a newer publication replaces the previous band.
        cache.publish_full_band(4, band(4));
        assert!(cache.get_full_band(3).is_none());
        assert_eq!(cache.get_full_band(4).map(|band| band.y0), Some(4));

        cache.clear_full_band(3);
        assert!(cache.get_full_band(4).is_some(), "mismatched clear is a no-op");
        cache.clear_full_band(4);
        assert!(cache.get_full_band(4).is_none());
    }

    #[test]
    fn full_band_is_cleared_by_the_matching_full_install_only() {
        let cache = RamCache::new(RamCacheBudgets::new(64, 64, 64, 0));
        cache.set_navigation_policy([], [(0, Tier::Full), (1, Tier::Full)]);
        cache.publish_full_band(0, band(0));

        // Unrelated installs leave the band alone.
        cache.insert_rgba((0, Tier::Browse), buf(8));
        cache.insert_rgba((0, Tier::Thumb), buf(8));
        assert!(cache.insert_rgba_if_desired((1, Tier::Full), buf(8)));
        assert!(cache.get_full_band(0).is_some());

        // The matching Full install supersedes the provisional band.
        assert!(cache.insert_rgba_if_desired((0, Tier::Full), buf(8)));
        assert!(cache.get_full_band(0).is_none());
    }

    #[test]
    fn full_band_follows_the_navigation_working_set() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 64, 0));
        cache.set_navigation_policy([], [(5, Tier::Full)]);
        cache.publish_full_band(5, band(5));

        cache.set_navigation_policy([], [(5, Tier::Full), (6, Tier::Full)]);
        assert!(
            cache.get_full_band(5).is_some(),
            "a band inside the working set survives a replan"
        );

        cache.set_navigation_policy([], [(6, Tier::Full)]);
        assert!(
            cache.get_full_band(5).is_none(),
            "a band outside the working set is dropped"
        );
    }

    #[test]
    fn full_band_stays_outside_ring_budgets_and_stats() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 0, 0));
        cache.publish_full_band(0, band(0));
        let stats = cache.stats();
        assert_eq!(stats.rgba_bytes, 0);
        assert_eq!(stats.full_rgba_bytes, 0);
        assert!(
            cache.get_full_band(0).is_some(),
            "a zero-budget cache still holds the single band slot"
        );
    }

    #[test]
    fn full_working_set_handles_jumps_and_direction_reversals_without_stale_reentry() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 32, 0));
        let first: Vec<_> = (10..14).map(|index| (index, Tier::Full)).collect();
        cache.set_navigation_policy([], first.iter().copied());
        for key in &first {
            cache.insert_rgba(*key, buf(8));
        }

        // A jump keeps resident entries (revisits stay RAM hits) but a late
        // completion for a key outside the new working set is still rejected.
        let jumped: Vec<_> = (80..84).map(|index| (index, Tier::Full)).collect();
        cache.set_navigation_policy([], jumped.iter().copied());
        assert!(first.iter().all(|key| cache.has_rgba(*key)));
        assert!(!cache.insert_rgba_if_desired((14, Tier::Full), buf(8)));

        // Filling the new working set exceeds the budget; the stale pre-jump
        // entries are the LRU victims, in insertion order.
        for key in &jumped {
            cache.insert_rgba(*key, buf(8));
        }
        assert!(first.iter().all(|key| !cache.has_rgba(*key)));
        assert!(jumped.iter().all(|key| cache.has_rgba(*key)));

        let reversed: Vec<_> = (79..83).map(|index| (index, Tier::Full)).collect();
        cache.set_navigation_policy([], reversed.iter().copied());
        assert!(
            (80..84).all(|index| cache.has_rgba((index, Tier::Full))),
            "a direction reversal must not discard the resident prefix"
        );
    }

    #[test]
    fn direction_flip_and_return_revisit_the_full_prefix_as_ram_hits() {
        let cache = RamCache::new(RamCacheBudgets::new(0, 0, 1_000, 0));
        let forward: Vec<_> = (10..20).map(|index| (index, Tier::Full)).collect();
        cache.set_navigation_policy([], forward.iter().copied());
        for key in &forward {
            assert!(cache.insert_rgba_if_desired(*key, buf(10)));
        }

        // One back-arrow flips the desired working set behind the cursor. The
        // forward prefix must survive: it is under budget and rebuilding it
        // would cost a full RAW develop per image.
        let backward: Vec<_> = (5..=10).map(|index| (index, Tier::Full)).collect();
        cache.set_navigation_policy([], backward.iter().copied());
        assert!(
            forward.iter().all(|key| cache.has_rgba(*key)),
            "an under-budget direction flip must retain the prefetched prefix"
        );
        // The admission guard still tracks the *new* working set.
        assert!(!cache.insert_rgba_if_desired((25, Tier::Full), buf(10)));
        assert!(cache.insert_rgba_if_desired((5, Tier::Full), buf(10)));

        // Flipping forward again finds every prefix entry still resident:
        // navigation needs zero redevelops.
        cache.set_navigation_policy([], forward.iter().copied());
        assert!(forward.iter().all(|key| cache.has_rgba(*key)));
        assert!(cache.has_rgba((5, Tier::Full)));
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
