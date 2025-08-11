#![allow(dead_code)]

use std::time::Duration;

/// Trait for snapshot exporters; called on a background task only.
pub trait SnapshotExporter: Send + Sync {
    fn export(&self, basic: &crate::metrics::snapshot::BasicSnapshot, hdr: Option<&crate::metrics::MaybeMetricsSnapshot>);
}

/// Simple logger exporter for debug/ops when Prometheus/StatsD is not configured.
pub struct LogExporter;

impl SnapshotExporter for LogExporter {
    fn export(&self, basic: &crate::metrics::snapshot::BasicSnapshot, hdr: Option<&crate::metrics::MaybeMetricsSnapshot>) {
        #[allow(unused_variables)]
        let hdr_str = {
            #[cfg(feature = "metrics-hdr")]
            {
                if let Some(h) = hdr {
                    format!(
                        " | p50/p99 us: code {} / {}, overall {} / {}",
                        h.code_p50, h.code_p99, h.overall_p50, h.overall_p99
                    )
                } else {
                    String::new()
                }
            }
            #[cfg(not(feature = "metrics-hdr"))]
            let _ = hdr; // suppress unused when feature disabled
            #[cfg(not(feature = "metrics-hdr"))]
            String::new()
        };

        log::info!(
            "metrics: recv={} processed={} err={} thrpt={:?}{}",
            basic.messages_received,
            basic.messages_processed,
            basic.parse_errors,
            basic.throughput_msgs_per_sec,
            hdr_str
        );
    }
}

/// Periodic aggregator that snapshots provided Metrics and uses an exporter.
pub struct Aggregator<E: SnapshotExporter> {
    exporter: E,
    interval: Duration,
}

impl<E: SnapshotExporter> Aggregator<E> {
    pub fn new(exporter: E, interval: Duration) -> Self { Self { exporter, interval } }

    pub async fn run(&self, metrics_ref: &std::sync::Arc<std::sync::Mutex<crate::types::Metrics>>) {
        let mut tick = tokio::time::interval(self.interval);
        loop {
            tick.tick().await;
            let (basic, hdr) = {
                let guard = metrics_ref.lock().expect("metrics lock");
                let basic = crate::metrics::snapshot::BasicSnapshot::from(&*guard);
                #[cfg(feature = "metrics-hdr")]
                let hdr = metrics_ref
                    .lock()
                    .expect("metrics lock")
                    .latency_histograms
                    .as_ref()
                    .map(|h| {
                        // Cloning a snapshot requires &mut; we cannot snapshot & reset without mut.
                        // Instead, return None here and rely on periodic logging from display or other APIs,
                        // or make metrics_ref mutable in the caller if snapshotting is needed.
                        // Keeping zero-cost path here; exporter can be enhanced later.
                        let _ = h; None::<crate::metrics::MaybeMetricsSnapshot>
                    })
                    .flatten();
                #[cfg(not(feature = "metrics-hdr"))]
                let hdr = None::<crate::metrics::MaybeMetricsSnapshot>;
                (basic, hdr)
            };
            self.exporter.export(&basic, hdr.as_ref());
        }
    }
}


