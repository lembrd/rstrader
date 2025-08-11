#![allow(dead_code)]

#[cfg(feature = "metrics-hdr")]
pub mod hdr;

pub mod snapshot;
pub mod exporters;

#[cfg(feature = "metrics-hdr")]
pub use hdr::{HistogramBounds, LatencyHistograms, MetricsSnapshot};

#[cfg(not(feature = "metrics-hdr"))]
/// Empty types when histograms are disabled to avoid feature‑gating call sites.
pub mod noop {
    #[derive(Clone, Copy)]
    pub struct HistogramBounds;
    #[derive(Default, Debug, Clone, Copy)]
    pub struct MetricsSnapshot;
}

// Feature-agnostic alias for exporter interfaces
#[cfg(feature = "metrics-hdr")]
pub type MaybeMetricsSnapshot = MetricsSnapshot;
#[cfg(not(feature = "metrics-hdr"))]
pub type MaybeMetricsSnapshot = noop::MetricsSnapshot;

use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};

// Global registry for a single process-wide Metrics instance
pub static GLOBAL_METRICS: Lazy<Arc<Mutex<crate::types::Metrics>>> = Lazy::new(|| {
    Arc::new(Mutex::new(crate::types::Metrics::new()))
});

/// Best-effort: copy selected fields into the global metrics snapshot.
/// This performs a single lock per call and overwrites absolute fields.
pub fn update_global(from: &crate::types::Metrics) {
    if let Ok(mut g) = GLOBAL_METRICS.lock() {
        g.messages_received = from.messages_received;
        g.messages_processed = from.messages_processed;
        g.connection_attempts = from.connection_attempts;
        g.reconnections = from.reconnections;
        g.parse_errors = from.parse_errors;
        g.last_code_latency_us = from.last_code_latency_us;
        g.avg_code_latency_us = from.avg_code_latency_us;
        g.last_overall_latency_us = from.last_overall_latency_us;
        g.avg_overall_latency_us = from.avg_overall_latency_us;
        g.throughput_msgs_per_sec = from.throughput_msgs_per_sec;
        g.last_parse_time_us = from.last_parse_time_us;
        g.avg_parse_time_us = from.avg_parse_time_us;
        g.last_transform_time_us = from.last_transform_time_us;
        g.avg_transform_time_us = from.avg_transform_time_us;
        g.last_overhead_time_us = from.last_overhead_time_us;
        g.avg_overhead_time_us = from.avg_overhead_time_us;
    }
}


