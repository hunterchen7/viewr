use std::collections::{HashMap, VecDeque};

/// Byte-bounded least-recently-used storage for UI-owned resources.
///
/// `egui::TextureHandle` releases its GPU allocation when the last handle is
/// dropped, so evicting an entry here directly bounds thumbnail residency.
pub(crate) struct ByteLru<T> {
    entries: HashMap<usize, Entry<T>>,
    oldest_first: VecDeque<usize>,
    used_bytes: usize,
    budget_bytes: usize,
}

struct Entry<T> {
    value: T,
    bytes: usize,
}

impl<T> ByteLru<T> {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            oldest_first: VecDeque::new(),
            used_bytes: 0,
            budget_bytes,
        }
    }

    pub(crate) fn get(&self, key: &usize) -> Option<&T> {
        self.entries.get(key).map(|entry| &entry.value)
    }

    pub(crate) fn touch(&mut self, key: usize) -> bool {
        if !self.entries.contains_key(&key) {
            return false;
        }
        self.remove_from_order(key);
        self.oldest_first.push_back(key);
        true
    }

    /// Insert an entry and evict the least-recently-used values until it fits.
    /// An entry larger than the whole budget is rejected without disturbing an
    /// existing value at the same key.
    pub(crate) fn insert(&mut self, key: usize, value: T, bytes: usize) -> bool {
        if bytes > self.budget_bytes {
            return false;
        }

        if let Some(replaced) = self.entries.remove(&key) {
            self.used_bytes -= replaced.bytes;
            self.remove_from_order(key);
        }

        while self.used_bytes > self.budget_bytes - bytes {
            let Some(oldest) = self.oldest_first.pop_front() else {
                debug_assert_eq!(self.used_bytes, 0);
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.used_bytes -= evicted.bytes;
            }
        }

        self.used_bytes += bytes;
        self.entries.insert(key, Entry { value, bytes });
        self.oldest_first.push_back(key);
        debug_assert!(self.used_bytes <= self.budget_bytes);
        true
    }

    fn remove_from_order(&mut self, key: usize) {
        if let Some(position) = self.oldest_first.iter().position(|queued| *queued == key) {
            self.oldest_first.remove(position);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn used_bytes(&self) -> usize {
        self.used_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_evicts_the_least_recently_used_entry_within_budget() {
        let mut cache = ByteLru::new(10);
        assert!(cache.insert(1, "one", 4));
        assert!(cache.insert(2, "two", 4));
        assert!(cache.touch(1));
        assert!(cache.insert(3, "three", 4));

        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), Some(&"three"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), 8);
    }

    #[test]
    fn replacement_updates_accounting_without_duplicate_lru_entries() {
        let mut cache = ByteLru::new(10);
        assert!(cache.insert(1, "old", 7));
        assert!(cache.insert(1, "new", 2));
        assert!(cache.insert(2, "other", 8));

        assert_eq!(cache.get(&1), Some(&"new"));
        assert_eq!(cache.get(&2), Some(&"other"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), 10);
    }

    #[test]
    fn oversized_insert_is_rejected_without_losing_the_old_value() {
        let mut cache = ByteLru::new(10);
        assert!(cache.insert(1, "old", 4));
        assert!(!cache.insert(1, "too large", 11));

        assert_eq!(cache.get(&1), Some(&"old"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 4);
    }

    #[test]
    fn residency_stays_bounded_over_a_fifty_thousand_item_stream() {
        let mut cache = ByteLru::new(64);
        for key in 0..50_000 {
            assert!(cache.insert(key, key, 1));
        }

        assert_eq!(cache.len(), 64);
        assert_eq!(cache.used_bytes(), 64);
        assert_eq!(cache.get(&49_935), None);
        assert_eq!(cache.get(&49_936), Some(&49_936));
        assert_eq!(cache.get(&49_999), Some(&49_999));
    }
}
