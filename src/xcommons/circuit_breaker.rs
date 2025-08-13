use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CircuitState {
    Closed,   // Normal operation
    Open,     // Circuit breaker tripped, rejecting requests
    HalfOpen, // Testing if service recovered
}

/// Simple circuit breaker for exchange connections
pub struct CircuitBreaker {
    failure_count: AtomicU32,
    failure_threshold: u32,
    timeout_duration: Duration,
    last_failure_time: parking_lot::Mutex<Option<Instant>>,
    state: parking_lot::Mutex<CircuitState>,
    is_testing: AtomicBool,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, timeout_duration: Duration) -> Self {
        Self {
            failure_count: AtomicU32::new(0),
            failure_threshold,
            timeout_duration,
            last_failure_time: parking_lot::Mutex::new(None),
            state: parking_lot::Mutex::new(CircuitState::Closed),
            is_testing: AtomicBool::new(false),
        }
    }

    /// Check if a request should be allowed
    pub fn can_execute(&self) -> bool {
        let state = *self.state.lock();
        
        match state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if timeout has passed
                if let Some(last_failure) = *self.last_failure_time.lock() {
                    if last_failure.elapsed() >= self.timeout_duration {
                        // Try to transition to half-open
                        if !self.is_testing.swap(true, Ordering::AcqRel) {
                            *self.state.lock() = CircuitState::HalfOpen;
                            return true;
                        }
                    }
                }
                false
            },
            CircuitState::HalfOpen => {
                // Only allow one test request at a time
                !self.is_testing.load(Ordering::Acquire)
            }
        }
    }

    /// Record a successful execution
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Release);
        self.is_testing.store(false, Ordering::Release);
        *self.state.lock() = CircuitState::Closed;
    }

    /// Record a failed execution
    pub fn record_failure(&self) {
        let new_count = self.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
        *self.last_failure_time.lock() = Some(Instant::now());
        self.is_testing.store(false, Ordering::Release);

        if new_count >= self.failure_threshold {
            *self.state.lock() = CircuitState::Open;
        }
    }

    /// Get current state
    pub fn state(&self) -> CircuitState {
        *self.state.lock()
    }

    /// Get current failure count
    pub fn failure_count(&self) -> u32 {
        self.failure_count.load(Ordering::Acquire)
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        // Default: 5 failures, 30 second timeout
        Self::new(5, Duration::from_secs(30))
    }
}