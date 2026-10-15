//! In-memory bounded cache with caller-supplied time for deterministic expiry checks.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::time::{Duration, Instant};

struct Entry<V> {
    value: V,
    weight: usize,
    expires_at: Instant,
}

/// A small in-memory cache bounded by entry count, total weight, and time to live.
///
/// Keys are kept only in memory and are never formatted or logged. Reads update LRU order but
/// leave the insertion-time expiry unchanged.
pub struct BoundedCache<K: Eq + Hash + Clone, V: Clone> {
    capacity: usize,
    ttl: Duration,
    max_weight: usize,
    total_weight: usize,
    entries: HashMap<K, Entry<V>>,
    lru: VecDeque<K>,
}

impl<K: Eq + Hash + Clone, V: Clone> BoundedCache<K, V> {
    pub fn new(capacity: usize, ttl: Duration, max_weight: usize) -> Self {
        Self {
            capacity,
            ttl,
            max_weight,
            total_weight: 0,
            entries: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    /// Return a cloned value when present and not expired, promoting it to most recently used.
    pub fn get(&mut self, key: &K, now: Instant) -> Option<V> {
        if self.is_disabled() {
            return None;
        }
        self.remove_expired(now);

        let value = self.entries.get(key)?.value.clone();
        self.promote(key);
        Some(value)
    }

    /// Insert a value, removing expired entries and evicting least recently used entries as needed.
    /// Values larger than the entire weight budget are not cached.
    pub fn insert(&mut self, key: K, value: V, weight: usize, now: Instant) {
        self.remove_expired(now);
        let _ = self.remove_entry(&key);

        if self.is_disabled() || weight > self.max_weight {
            return;
        }
        let Some(expires_at) = now.checked_add(self.ttl) else {
            // If the requested expiry cannot be represented, do not cache with an ambiguous TTL.
            return;
        };

        while self.entries.len() >= self.capacity
            || self.total_weight > self.max_weight.saturating_sub(weight)
        {
            if !self.evict_lru() {
                return;
            }
        }

        self.total_weight += weight;
        self.lru.push_back(key.clone());
        self.entries.insert(
            key,
            Entry {
                value,
                weight,
                expires_at,
            },
        );
    }

    /// Remove all entries and reset the tracked weight.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.total_weight = 0;
    }

    fn is_disabled(&self) -> bool {
        self.capacity == 0 || self.ttl.is_zero() || self.max_weight == 0
    }

    fn promote(&mut self, key: &K) {
        if let Some(index) = self.lru.iter().position(|entry| entry == key) {
            self.lru.remove(index);
        }
        self.lru.push_back(key.clone());
    }

    fn remove_entry(&mut self, key: &K) -> Option<Entry<V>> {
        let entry = self.entries.remove(key)?;
        self.total_weight -= entry.weight;
        if let Some(index) = self.lru.iter().position(|entry| entry == key) {
            self.lru.remove(index);
        }
        Some(entry)
    }

    fn remove_expired(&mut self, now: Instant) {
        let expired: Vec<K> = self
            .entries
            .iter()
            .filter_map(|(key, entry)| (now >= entry.expires_at).then(|| key.clone()))
            .collect();
        for key in expired {
            let _ = self.remove_entry(&key);
        }
    }

    fn evict_lru(&mut self) -> bool {
        let Some(key) = self.lru.pop_front() else {
            return false;
        };
        self.remove_entry(&key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::BoundedCache;
    use std::time::{Duration, Instant};

    #[test]
    fn ttl_is_fixed_at_insertion_and_expires_at_the_boundary() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(4, Duration::from_secs(10), 100);
        cache.insert("key", "value", 1, start);

        assert_eq!(
            cache.get(&"key", start + Duration::from_secs(5)),
            Some("value")
        );
        assert_eq!(
            cache.get(&"key", start + Duration::from_secs(9)),
            Some("value")
        );
        assert_eq!(cache.get(&"key", start + Duration::from_secs(10)), None);
        assert_eq!(cache.total_weight, 0);
    }

    #[test]
    fn capacity_eviction_uses_lru_order() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(2, Duration::from_secs(60), 100);
        cache.insert("a", 1, 1, start);
        cache.insert("b", 2, 1, start);
        assert_eq!(cache.get(&"a", start + Duration::from_secs(1)), Some(1));
        cache.insert("c", 3, 1, start + Duration::from_secs(2));

        assert_eq!(cache.get(&"b", start + Duration::from_secs(2)), None);
        assert_eq!(cache.get(&"a", start + Duration::from_secs(2)), Some(1));
        assert_eq!(cache.get(&"c", start + Duration::from_secs(2)), Some(3));
    }

    #[test]
    fn weight_budget_evicts_lru_entries_without_exceeding_total() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(10, Duration::from_secs(60), 5);
        cache.insert("a", "first", 3, start);
        cache.insert("b", "second", 3, start + Duration::from_secs(1));

        assert_eq!(cache.get(&"a", start + Duration::from_secs(1)), None);
        assert_eq!(
            cache.get(&"b", start + Duration::from_secs(1)),
            Some("second")
        );
        assert_eq!(cache.total_weight, 3);
        assert!(cache.total_weight <= cache.max_weight);
    }

    #[test]
    fn oversized_item_is_not_cached_or_allowed_to_evict_existing_values() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(4, Duration::from_secs(60), 3);
        cache.insert("kept", "small", 2, start);
        cache.insert("large", "too large", 4, start);

        assert_eq!(cache.get(&"large", start), None);
        assert_eq!(cache.get(&"kept", start), Some("small"));
        assert_eq!(cache.total_weight, 2);
    }

    #[test]
    fn expired_entries_are_removed_before_insert_eviction() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(1, Duration::from_secs(5), 10);
        cache.insert("old", 1, 2, start);
        cache.insert("new", 2, 3, start + Duration::from_secs(5));

        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.total_weight, 3);
        assert_eq!(cache.get(&"old", start + Duration::from_secs(5)), None);
        assert_eq!(cache.get(&"new", start + Duration::from_secs(5)), Some(2));
    }

    #[test]
    fn clear_removes_values_and_resets_weight() {
        let start = Instant::now();
        let mut cache = BoundedCache::new(3, Duration::from_secs(60), 10);
        cache.insert("key", "value", 4, start);
        cache.clear();

        assert_eq!(cache.get(&"key", start), None);
        assert_eq!(cache.total_weight, 0);
        assert!(cache.entries.is_empty());
        cache.insert("key", "new value", 2, start);
        assert_eq!(cache.get(&"key", start), Some("new value"));
    }

    #[test]
    fn zero_capacity_ttl_or_weight_disables_storage() {
        let start = Instant::now();

        let mut no_capacity = BoundedCache::new(0, Duration::from_secs(1), 10);
        no_capacity.insert("key", 1, 1, start);
        assert_eq!(no_capacity.get(&"key", start), None);

        let mut no_ttl = BoundedCache::new(1, Duration::ZERO, 10);
        no_ttl.insert("key", 1, 1, start);
        assert_eq!(no_ttl.get(&"key", start), None);

        let mut no_weight = BoundedCache::new(1, Duration::from_secs(1), 0);
        no_weight.insert("key", 1, 0, start);
        assert_eq!(no_weight.get(&"key", start), None);
    }
}
