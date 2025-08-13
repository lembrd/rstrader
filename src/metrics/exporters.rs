//

use std::time::Duration;
use prometheus::{Encoder, TextEncoder, Registry, IntGauge, Opts, GaugeVec};
use cadence::{StatsdClient, NopMetricSink, UdpMetricSink, QueuingMetricSink, Gauged, Counted};
use std::net::UdpSocket;

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

    pub async fn run(&self, metrics_ref: &std::sync::Arc<std::sync::Mutex<crate::xcommons::types::Metrics>>) {
        let mut tick = tokio::time::interval(self.interval);
        loop {
            tick.tick().await;
            let (basic, hdr) = {
                let guard = metrics_ref.lock().expect("metrics lock");
                let basic = crate::metrics::snapshot::BasicSnapshot::from(&*guard);
                #[cfg(feature = "metrics-hdr")]
                let hdr = None::<crate::metrics::MaybeMetricsSnapshot>;
                #[cfg(not(feature = "metrics-hdr"))]
                let hdr = None::<crate::metrics::MaybeMetricsSnapshot>;
                (basic, hdr)
            };
            self.exporter.export(&basic, hdr.as_ref());
        }
    }
}

/// Prometheus exporter that keeps a registry and updates gauges/counters from snapshots.
pub struct PrometheusExporter {
    registry: Registry,
    recv_total: IntGauge,
    processed_total: IntGauge,
    parse_errors_total: IntGauge,
    throughput: IntGauge,
    // Per-stream quantiles (microseconds)
    code_p50_us: GaugeVec,
    code_p99_us: GaugeVec,
    overall_p50_us: GaugeVec,
    overall_p99_us: GaugeVec,
    parse_p50_us: GaugeVec,
    parse_p99_us: GaugeVec,
    transform_p50_us: GaugeVec,
    transform_p99_us: GaugeVec,
    overhead_p50_us: GaugeVec,
    overhead_p99_us: GaugeVec,
}

impl PrometheusExporter {
    pub fn new() -> Self {
        let registry = Registry::new();
        let recv_total = IntGauge::with_opts(Opts::new("messages_received_total", "Total messages received")).unwrap();
        let processed_total = IntGauge::with_opts(Opts::new("messages_processed_total", "Total messages processed")).unwrap();
        let parse_errors_total = IntGauge::with_opts(Opts::new("parse_errors_total", "Total parse errors")).unwrap();
        let throughput = IntGauge::with_opts(Opts::new("throughput_msgs_per_sec", "Throughput in messages/sec")).unwrap();
        let code_p50_us = GaugeVec::new(Opts::new("code_latency_p50_us", "Code execution latency p50 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let code_p99_us = GaugeVec::new(Opts::new("code_latency_p99_us", "Code execution latency p99 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let overall_p50_us = GaugeVec::new(Opts::new("overall_latency_p50_us", "Overall latency p50 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let overall_p99_us = GaugeVec::new(Opts::new("overall_latency_p99_us", "Overall latency p99 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let parse_p50_us = GaugeVec::new(Opts::new("parse_latency_p50_us", "Parse latency p50 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let parse_p99_us = GaugeVec::new(Opts::new("parse_latency_p99_us", "Parse latency p99 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let transform_p50_us = GaugeVec::new(Opts::new("transform_latency_p50_us", "Transform latency p50 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let transform_p99_us = GaugeVec::new(Opts::new("transform_latency_p99_us", "Transform latency p99 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let overhead_p50_us = GaugeVec::new(Opts::new("overhead_latency_p50_us", "Overhead latency p50 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        let overhead_p99_us = GaugeVec::new(Opts::new("overhead_latency_p99_us", "Overhead latency p99 (us)"), &[
            "stream", "exchange", "symbol"
        ]).unwrap();
        registry.register(Box::new(recv_total.clone())).unwrap();
        registry.register(Box::new(processed_total.clone())).unwrap();
        registry.register(Box::new(parse_errors_total.clone())).unwrap();
        registry.register(Box::new(throughput.clone())).unwrap();
        registry.register(Box::new(code_p50_us.clone())).unwrap();
        registry.register(Box::new(code_p99_us.clone())).unwrap();
        registry.register(Box::new(overall_p50_us.clone())).unwrap();
        registry.register(Box::new(overall_p99_us.clone())).unwrap();
        registry.register(Box::new(parse_p50_us.clone())).unwrap();
        registry.register(Box::new(parse_p99_us.clone())).unwrap();
        registry.register(Box::new(transform_p50_us.clone())).unwrap();
        registry.register(Box::new(transform_p99_us.clone())).unwrap();
        registry.register(Box::new(overhead_p50_us.clone())).unwrap();
        registry.register(Box::new(overhead_p99_us.clone())).unwrap();
        Self { registry, recv_total, processed_total, parse_errors_total, throughput, code_p50_us, code_p99_us, overall_p50_us, overall_p99_us, parse_p50_us, parse_p99_us, transform_p50_us, transform_p99_us, overhead_p50_us, overhead_p99_us }
    }

    pub fn gather(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        TextEncoder::new().encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap_or_default()
    }
}

impl SnapshotExporter for PrometheusExporter {
    fn export(&self, basic: &crate::metrics::snapshot::BasicSnapshot, _hdr: Option<&crate::metrics::MaybeMetricsSnapshot>) {
        // Convert deltas to counters by setting to absolute via inc_by of delta; here we set to absolute by reading previous is not trivial.
        // For simplicity in this first pass, we just set gauges based on current snapshot where applicable and inc counters by snapshot totals.
        self.recv_total.set(basic.messages_received as i64);
        self.processed_total.set(basic.messages_processed as i64);
        self.parse_errors_total.set(basic.parse_errors as i64);
        if let Some(t) = basic.throughput_msgs_per_sec { self.throughput.set(t as i64); }
    }
}

/// StatsD exporter using cadence with UDP sink.
pub struct StatsdExporter {
    client: StatsdClient,
}

impl StatsdExporter {
    pub fn new(prefix: &str, host: &str, port: u16) -> Self {
        let socket = UdpSocket::bind("0.0.0.0:0").ok();
        let sink = if let Some(sock) = socket {
            UdpMetricSink::from((host, port), sock).map(QueuingMetricSink::from).unwrap_or_else(|_| QueuingMetricSink::from(NopMetricSink))
        } else {
            QueuingMetricSink::from(NopMetricSink)
        };
        let client = StatsdClient::builder(prefix, sink).with_error_handler(|_| {}).build();
        Self { client }
    }
}

impl SnapshotExporter for StatsdExporter {
    fn export(&self, basic: &crate::metrics::snapshot::BasicSnapshot, _hdr: Option<&crate::metrics::MaybeMetricsSnapshot>) {
        let _ = self.client.count("messages_received", basic.messages_received as i64);
        let _ = self.client.count("messages_processed", basic.messages_processed as i64);
        let _ = self.client.count("parse_errors", basic.parse_errors as i64);
        if let Some(t) = basic.throughput_msgs_per_sec { let _ = self.client.gauge("throughput_msgs_per_sec", t as f64); }
    }
}

// Allow sharing PrometheusExporter via Arc between HTTP server and aggregator
impl SnapshotExporter for std::sync::Arc<PrometheusExporter> {
    fn export(&self, basic: &crate::metrics::snapshot::BasicSnapshot, hdr: Option<&crate::metrics::MaybeMetricsSnapshot>) {
        (**self).export(basic, hdr)
    }
}

impl PrometheusExporter {
    pub fn set_stream_quantiles(&self, stream: &str, exchange: &str, symbol: &str,
                                code_p50: u64, code_p99: u64,
                                overall_p50: u64, overall_p99: u64,
                                parse_p50: u64, parse_p99: u64,
                                transform_p50: u64, transform_p99: u64,
                                overhead_p50: u64, overhead_p99: u64) {
        let labels = &[stream, exchange, symbol];
        self.code_p50_us.with_label_values(labels).set(code_p50 as f64);
        self.code_p99_us.with_label_values(labels).set(code_p99 as f64);
        self.overall_p50_us.with_label_values(labels).set(overall_p50 as f64);
        self.overall_p99_us.with_label_values(labels).set(overall_p99 as f64);
        self.parse_p50_us.with_label_values(labels).set(parse_p50 as f64);
        self.parse_p99_us.with_label_values(labels).set(parse_p99 as f64);
        self.transform_p50_us.with_label_values(labels).set(transform_p50 as f64);
        self.transform_p99_us.with_label_values(labels).set(transform_p99 as f64);
        self.overhead_p50_us.with_label_values(labels).set(overhead_p50 as f64);
        self.overhead_p99_us.with_label_values(labels).set(overhead_p99 as f64);
    }

    pub fn set_stream_quantiles_opt(&self, stream: &str, exchange: &str, symbol: &str,
                                    code_p50: Option<u64>, code_p99: Option<u64>,
                                    overall_p50: Option<u64>, overall_p99: Option<u64>,
                                    parse_p50: Option<u64>, parse_p99: Option<u64>,
                                    transform_p50: Option<u64>, transform_p99: Option<u64>,
                                    overhead_p50: Option<u64>, overhead_p99: Option<u64>) {
        let labels = &[stream, exchange, symbol];
        if let Some(v) = code_p50 { self.code_p50_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = code_p99 { self.code_p99_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = overall_p50 { self.overall_p50_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = overall_p99 { self.overall_p99_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = parse_p50 { self.parse_p50_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = parse_p99 { self.parse_p99_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = transform_p50 { self.transform_p50_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = transform_p99 { self.transform_p99_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = overhead_p50 { self.overhead_p50_us.with_label_values(labels).set(v as f64); }
        if let Some(v) = overhead_p99 { self.overhead_p99_us.with_label_values(labels).set(v as f64); }
    }
}


