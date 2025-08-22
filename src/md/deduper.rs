use ahash::{AHashSet, AHasher};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Lightweight LRU deduper with simple metrics
pub struct Deduper {
    capacity: usize,
    set: AHashSet<u64>,
    window: VecDeque<u64>,
    metrics: DeduperMetrics,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct DeduperMetrics {
    pub inserts: u64,
    pub hits: u64,
    pub evictions: u64,
}

impl Deduper {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            set: AHashSet::default(),
            window: VecDeque::new(),
            metrics: DeduperMetrics::default(),
        }
    }

    pub fn check_unique<K: Hash>(&mut self, item: &K) -> bool {
        // Fast hash using ahash
        let hash = self.hash_item(item);

        // Check if already seen
        if self.set.contains(&hash) {
            self.metrics.hits += 1;
            return false;
        }

        // Add to seen set
        self.set.insert(hash);
        self.metrics.inserts += 1;

        // Maintain bounded window
        if self.window.len() >= self.capacity {
            if let Some(old_hash) = self.window.pop_front() {
                self.set.remove(&old_hash);
                self.metrics.evictions += 1;
            }
        }

        self.window.push_back(hash);
        true
    }

    #[inline]
    fn hash_item<K: Hash>(&self, item: &K) -> u64 {
        let mut hasher = AHasher::default();
        item.hash(&mut hasher);
        hasher.finish()
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
    pub fn metrics(&self) -> DeduperMetrics {
        self.metrics
    }
}
