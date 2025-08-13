use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Monotonic, globally unique, low-latency ID generator based on a Snowflake-like scheme.
///
/// Bit layout (most-significant to least-significant):
/// - 41 bits: timestamp in milliseconds since custom epoch
/// - 10 bits: machine identifier (0..=1023)
/// - 12 bits: per-millisecond sequence (0..=4095)
///
/// Guarantees:
/// - Monotonic non-decreasing IDs within a process, across threads
/// - If the system clock moves backwards (e.g., due to NTP), generation waits until
///   the previously observed millisecond is reached to preserve monotonicity
/// - If more than 4096 IDs are requested within the same millisecond, the generator waits
///   for the next millisecond tick
pub struct MonoSeq {
    machine_id: u16,            // 10 bits
    last_state: AtomicU64,      // packed (last_timestamp_ms << SEQ_BITS) | seq
}

// Bit constants
const TIME_BITS: u64 = 41;
const MACHINE_BITS: u64 = 10;
const SEQ_BITS: u64 = 12;

const MAX_MACHINE_ID: u16 = (1 << MACHINE_BITS) as u16 - 1; // 1023
const MAX_SEQ: u64 = (1u64 << SEQ_BITS) - 1; // 4095

const SEQ_MASK: u64 = MAX_SEQ;

// Custom epoch: 2024-01-01T00:00:00Z
const CUSTOM_EPOCH_UNIX_MS: u64 = 1704067200000;

static GLOBAL: OnceLock<MonoSeq> = OnceLock::new();

impl MonoSeq {
    /// Create a new generator with the specified machine identifier (0..=1023)
    pub fn new(machine_id: u16) -> Self {
        assert!(machine_id <= MAX_MACHINE_ID, "machine_id must be <= {}", MAX_MACHINE_ID);
        Self {
            machine_id,
            last_state: AtomicU64::new(0),
        }
    }

    /// Returns the next monotonic, globally unique int64 ID
    #[inline]
    pub fn next(&self) -> i64 {
        let machine_bits = (self.machine_id as u64) << SEQ_BITS;
        loop {
            let prev = self.last_state.load(Ordering::Relaxed);
            let prev_ms = prev >> SEQ_BITS;
            let prev_seq = prev & SEQ_MASK;

            // Current milliseconds since custom epoch (saturating, never panics on backward time)
            let mut now_ms = now_millis_since_custom_epoch();

            // Handle NTP or manual time adjustments moving clock backwards
            if now_ms < prev_ms {
                now_ms = wait_until_at_least(prev_ms);
            }

            let (new_ms, new_seq) = if now_ms == prev_ms {
                if prev_seq < MAX_SEQ {
                    (prev_ms, prev_seq + 1)
                } else {
                    // Sequence exhausted for this millisecond, wait for next tick
                    let next_ms = wait_until_at_least(prev_ms + 1);
                    (next_ms, 0)
                }
            } else {
                // New millisecond, reset sequence
                (now_ms, 0)
            };

            let new_state = (new_ms << SEQ_BITS) | new_seq;
            match self.last_state.compare_exchange_weak(
                prev,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Compose final ID: [time 41][machine 10][seq 12]
                    let id = (new_ms << (MACHINE_BITS + SEQ_BITS)) | machine_bits | new_seq;
                    // Safe to cast: highest bit (sign) remains 0 due to 63-bit usage
                    return id as i64;
                }
                Err(_) => {
                    // Contended update; retry
                    std::hint::spin_loop();
                    continue;
                }
            }
        }
    }

    /// Initialize and return the global generator. If already initialized, returns existing.
    ///
    /// If `XTRADER_MACHINE_ID` env var is provided, it's used (0..=1023). Otherwise a best-effort
    /// stable hash of hostname and PID is used, masked into 10 bits.
    pub fn global() -> &'static MonoSeq {
        GLOBAL.get_or_init(|| {
            let machine_id = resolve_machine_id();
            MonoSeq::new(machine_id)
        })
    }
}

#[inline(always)]
fn now_millis_since_custom_epoch() -> u64 {
    // Use saturating duration to avoid panics when the system clock is set backwards
    let now = SystemTime::now();
    let dur = now.duration_since(UNIX_EPOCH).unwrap_or_else(|_| Duration::from_secs(0));
    let unix_ms = dur.as_millis() as u64;
    unix_ms.saturating_sub(CUSTOM_EPOCH_UNIX_MS)
}

#[inline]
fn wait_until_at_least(target_ms: u64) -> u64 {
    // Hybrid wait: spin for a short time to keep latency extremely low for sub-ms skews;
    // then yield/sleep in small increments if needed.
    let mut spins: u32 = 0;
    loop {
        let now = now_millis_since_custom_epoch();
        if now >= target_ms {
            return now;
        }
        if spins < 2000 {
            // ~2k spin iterations; tuned to be cheap yet effective for <1ms waits
            spins += 1;
            std::hint::spin_loop();
        } else {
            // Back off minimally to reduce CPU burn if the skew is larger
            std::thread::sleep(Duration::from_micros(50));
        }
    }
}

fn resolve_machine_id() -> u16 {
    if let Ok(val) = std::env::var("XTRADER_MACHINE_ID") {
        if let Ok(parsed) = val.parse::<u16>() {
            return parsed & MAX_MACHINE_ID as u16;
        }
    }

    // Best-effort stable hash based on hostname and PID
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| String::from("unknown"));
    host.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    (hasher.finish() as u16) & MAX_MACHINE_ID
}

/// Returns the next global ID using the lazily initialized global generator
#[inline]
pub fn next_id() -> i64 {
    MonoSeq::global().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_monotonic_across_calls() {
        let gen = MonoSeq::new(1);
        let a = gen.next();
        let b = gen.next();
        assert!(b > a);
    }

    #[test]
    fn test_concurrent_uniqueness_and_order() {
        let gen = Arc::new(MonoSeq::new(2));
        let threads = 8;
        let per_thread = 10_000;

        let mut handles = Vec::new();
        for _ in 0..threads {
            let g = gen.clone();
            handles.push(thread::spawn(move || {
                let mut v = Vec::with_capacity(per_thread);
                for _ in 0..per_thread {
                    v.push(g.next());
                }
                v
            }));
        }

        let mut all = Vec::with_capacity(threads * per_thread);
        for h in handles {
            let mut v = h.join().unwrap();
            all.append(&mut v);
        }

        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), threads * per_thread);
    }

    #[test]
    fn test_handles_clock_backward() {
        let gen = MonoSeq::new(3);
        // Simulate that last observed ms is far in the future relative to current time
        let now = now_millis_since_custom_epoch();
        let future_ms = now + 5; // 5ms ahead
        let prev_state = (future_ms << SEQ_BITS) | 10;
        gen.last_state.store(prev_state, Ordering::Release);

        let id = gen.next();

        // Extract timestamp component from generated id
        let ts_ms = (id as u64) >> (MACHINE_BITS + SEQ_BITS);
        assert!(ts_ms >= future_ms);
    }

    #[test]
    fn test_sequence_rollover_waits_next_ms() {
        let gen = MonoSeq::new(4);
        // Force sequence to MAX_SEQ on current millisecond so next() must wait to next ms and reset seq
        let now = now_millis_since_custom_epoch();
        let prev_state = (now << SEQ_BITS) | MAX_SEQ;
        gen.last_state.store(prev_state, Ordering::Release);

        let id = gen.next();
        let ts_ms = (id as u64) >> (MACHINE_BITS + SEQ_BITS);
        let seq = (id as u64) & ((1 << SEQ_BITS) - 1);

        assert!(ts_ms >= now + 1);
        assert_eq!(seq, 0);
    }
}


