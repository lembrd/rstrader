//

pub mod hdr;

pub mod snapshot;
pub mod exporters;

pub use hdr::{HistogramBounds, LatencyHistograms, MetricsSnapshot};

pub type MaybeMetricsSnapshot = MetricsSnapshot;

use once_cell::sync::{Lazy, OnceCell};
use std::sync::{Arc, Mutex};
use crate::xcommons::types as types;

// Global registry for a single process-wide Metrics instance
pub static GLOBAL_METRICS: Lazy<Arc<Mutex<types::Metrics>>> = Lazy::new(|| {
    Arc::new(Mutex::new(types::Metrics::new()))
});

// Global Prometheus exporter handle for components to publish labeled metrics
pub static PROM_EXPORTER: OnceCell<Arc<crate::metrics::exporters::PrometheusExporter>> = OnceCell::new();

/// Publish stream-scoped metrics: update global snapshot and export p50/p99 latencies
/// Labels: stream, exchange, symbol
pub fn publish_stream_metrics(
    stream: &str,
    exchange: types::ExchangeId,
    symbol: &str,
    metrics: &types::Metrics,
) {
    // Always keep the global snapshot up to date
    update_global(metrics);

    // Avoid unused-arg warnings when histograms feature is disabled
    let _ = (stream, exchange, symbol); // used below and for label construction

    // Export quantiles when histograms are enabled
    {
        let h = &metrics.latency_histograms;
        if let Some(prom) = PROM_EXPORTER.get() {
            // Compute all present values and single-call update
            let mut code_p50 = None; let mut code_p99 = None;
            if metrics.last_code_latency_us.is_some() || metrics.avg_code_latency_us.is_some() {
                code_p50 = Some(h.code_us.value_at_quantile(0.50));
                code_p99 = Some(h.code_us.value_at_quantile(0.99));
            }
            let mut overall_p50 = None; let mut overall_p99 = None;
            if metrics.last_overall_latency_us.is_some() || metrics.avg_overall_latency_us.is_some() {
                overall_p50 = Some(h.overall_us.value_at_quantile(0.50));
                overall_p99 = Some(h.overall_us.value_at_quantile(0.99));
            }
            let mut parse_p50 = None; let mut parse_p99 = None;
            if metrics.last_parse_time_us.is_some() || metrics.avg_parse_time_us.is_some() {
                parse_p50 = Some(h.parse_us.value_at_quantile(0.50));
                parse_p99 = Some(h.parse_us.value_at_quantile(0.99));
            }
            let mut transform_p50 = None; let mut transform_p99 = None;
            if metrics.last_transform_time_us.is_some() || metrics.avg_transform_time_us.is_some() {
                transform_p50 = Some(h.transform_us.value_at_quantile(0.50));
                transform_p99 = Some(h.transform_us.value_at_quantile(0.99));
            }
            let mut overhead_p50 = None; let mut overhead_p99 = None;
            if metrics.last_overhead_time_us.is_some() || metrics.avg_overhead_time_us.is_some() {
                overhead_p50 = Some(h.overhead_us.value_at_quantile(0.50));
                overhead_p99 = Some(h.overhead_us.value_at_quantile(0.99));
            }
            // Only publish present categories to avoid resetting others to zero
            prom.set_stream_quantiles_opt(
                stream,
                &exchange.to_string(),
                symbol,
                code_p50,
                code_p99,
                overall_p50,
                overall_p99,
                parse_p50,
                parse_p99,
                transform_p50,
                transform_p99,
                overhead_p50,
                overhead_p99,
            );
        }
    }
}

/// Publish only counters/snapshot (no latency quantiles). Use this from sinks that don't record latency.
pub fn publish_stream_counters(
    _stream: &str,
    _exchange: types::ExchangeId,
    _symbol: &str,
    metrics: &types::Metrics,
) {
    update_global(metrics);
}

/// Best-effort: copy selected fields into the global metrics snapshot.
/// This performs a single lock per call and overwrites absolute fields.
pub fn update_global(from: &types::Metrics) {
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


