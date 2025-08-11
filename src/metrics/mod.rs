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


