use std::collections::VecDeque;
use std::time::{Duration, Instant};
use rustc_hash::FxHashSet as HashSet;

/// Lightweight LRU + TTL deduper with simple metrics
///
/// - insert(key, now) returns true if the key was not present (first time seen)
/// - Maintains a size-bounded set with eviction from the oldest side
/// - TTL-based pruning on insert/prune calls
pub struct Deduper<K>
where
    K: std::hash::Hash + Eq + Clone,
{
    capacity: usize,
    ttl: Duration,
    set: HashSet<K>,
    order: VecDeque<(K, Instant)>,
    metrics: DeduperMetrics,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct DeduperMetrics {
    pub inserts: u64,
    pub hits: u64,
    pub evictions: u64,
    pub expired: u64,
}

impl<K> Deduper<K>
where
    K: std::hash::Hash + Eq + Clone,
{
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            set: HashSet::default(),
            order: VecDeque::new(),
            metrics: DeduperMetrics::default(),
        }
    }

    /// Insert a key, returning true if this is the first time it's seen
    pub fn insert(&mut self, key: K, now: Instant) -> bool {
        self.prune(now);
        if self.set.insert(key.clone()) {
            self.order.push_back((key, now));
            self.metrics.inserts += 1;
            self.enforce_capacity();
            true
        } else {
            self.metrics.hits += 1;
            false
        }
    }

    /// Remove expired entries based on TTL
    pub fn prune(&mut self, now: Instant) {
        while let Some((front_key, ts)) = self.order.front().cloned() {
            if now.duration_since(ts) > self.ttl {
                self.order.pop_front();
                if self.set.remove(&front_key) {
                    self.metrics.expired += 1;
                }
            } else {
                break;
            }
        }
    }

    fn enforce_capacity(&mut self) {
        while self.set.len() > self.capacity {
            if let Some((oldest, _)) = self.order.pop_front() {
                if self.set.remove(&oldest) {
                    self.metrics.evictions += 1;
                }
            } else {
                break;
            }
        }
    }

    pub fn len(&self) -> usize { self.set.len() }
    pub fn is_empty(&self) -> bool { self.set.is_empty() }
    pub fn metrics(&self) -> DeduperMetrics { self.metrics }
}


