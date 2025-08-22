//

use std::time::Duration;
use prometheus::{Encoder, TextEncoder, Registry, IntGauge, Opts, GaugeVec, IntCounterVec, HistogramVec, HistogramOpts};
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
            if let Some(h) = hdr {
                format!(
                    " | p50/p99 us: code {} / {}, overall {} / {}",
                    h.code_p50, h.code_p99, h.overall_p50, h.overall_p99
                )
            } else {
                String::new()
            }
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
    // Order latency histograms (microseconds)
    order_code_latency: HistogramVec,          // code_latency: tc.send_ts - tc.rcv_ts
    order_before_strategy: HistogramVec,       // before_strategy: tc.proc_ts - tc.rcv_ts
    order_req_network_latency: HistogramVec,   // req_network_latency: rcv_timestamp - tc.send_ts
    order_rcv_network_latency: HistogramVec,   // rcv_network_latency: tc.rcv_ts - tc.initial_exchange_ts
    // Strategy-level gauges
    strategy_pnl: GaugeVec,
    strategy_amount: GaugeVec,
    strategy_bps: GaugeVec,
    strategy_mid_price: GaugeVec,
    // Strategy latency (us) gauges
    strat_tick_to_trade_p50: GaugeVec,
    strat_tick_to_trade_p99: GaugeVec,
    strat_tick_to_cancel_p50: GaugeVec,
    strat_tick_to_cancel_p99: GaugeVec,
    strat_code_p50: GaugeVec,
    strat_code_p99: GaugeVec,
    strat_network_p50: GaugeVec,
    strat_network_p99: GaugeVec,
    // Explicit post/cancel latencies (full, incl. network)
    strat_post_p50: GaugeVec,
    strat_post_p99: GaugeVec,
    strat_cancel_p50: GaugeVec,
    strat_cancel_p99: GaugeVec,
    // Request counters
    strat_post_requests_total: IntCounterVec,
    strat_cancel_requests_total: IntCounterVec,
    // WS-API metrics
    ws_api_last_rtt_us: GaugeVec,
    ws_api_requests_total: IntCounterVec, // labels: exchange, method, outcome {ok,error,timeout}
    ws_api_errors_total: IntCounterVec,   // labels: exchange, method, code
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
        // Order latency histograms with buckets optimized for microsecond measurements
        let order_latency_buckets = vec![
            1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0,
            1_000.0, 2_500.0, 5_000.0, 7_500.0, 10_000.0, 25_000.0, 50_000.0, 75_000.0,
            100_000.0, 250_000.0, 500_000.0, 1_000_000.0
        ];
        let order_code_latency = HistogramVec::new(
            HistogramOpts::new("xtrader_order_code_latency_us", "Order code latency (send_ts - rcv_ts) in microseconds")
                .buckets(order_latency_buckets.clone()),
            &["exchange", "symbol"]
        ).unwrap();
        let order_before_strategy = HistogramVec::new(
            HistogramOpts::new("xtrader_order_before_strategy_us", "Order processing time before strategy (proc_ts - rcv_ts) in microseconds")
                .buckets(order_latency_buckets.clone()),
            &["exchange", "symbol"]
        ).unwrap();
        let order_req_network_latency = HistogramVec::new(
            HistogramOpts::new("xtrader_order_req_network_latency_us", "Order request network latency (rcv_timestamp - send_ts) in microseconds")
                .buckets(order_latency_buckets.clone()),
            &["exchange", "symbol"]
        ).unwrap();
        let order_rcv_network_latency = HistogramVec::new(
            HistogramOpts::new("xtrader_order_rcv_network_latency_us", "Order receive network latency (rcv_ts - initial_exchange_ts) in microseconds")
                .buckets(order_latency_buckets),
            &["exchange", "symbol"]
        ).unwrap();
        // Strategy metrics
        let strategy_pnl = GaugeVec::new(Opts::new("strategy_pnl", "Mark-to-market PnL for strategy"), &[
            "strategy", "symbol"
        ]).unwrap();
        let strategy_amount = GaugeVec::new(Opts::new("strategy_position_amount", "Current position amount"), &[
            "strategy", "symbol"
        ]).unwrap();
        let strategy_bps = GaugeVec::new(Opts::new("strategy_bps", "Realized PnL in bps"), &[
            "strategy", "symbol"
        ]).unwrap();
        let strategy_mid_price = GaugeVec::new(Opts::new("strategy_mid_price", "Mid price observed by strategy"), &[
            "strategy", "symbol"
        ]).unwrap();
        // Strategy latency
        let strat_tick_to_trade_p50 = GaugeVec::new(Opts::new("strategy_tick_to_trade_p50_us", "Tick-to-trade p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_tick_to_trade_p99 = GaugeVec::new(Opts::new("strategy_tick_to_trade_p99_us", "Tick-to-trade p99 (us)"), &["strategy","symbol"]).unwrap();
        let strat_tick_to_cancel_p50 = GaugeVec::new(Opts::new("strategy_tick_to_cancel_p50_us", "Tick-to-cancel p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_tick_to_cancel_p99 = GaugeVec::new(Opts::new("strategy_tick_to_cancel_p99_us", "Tick-to-cancel p99 (us)"), &["strategy","symbol"]).unwrap();
        let strat_code_p50 = GaugeVec::new(Opts::new("strategy_code_latency_p50_us", "Strategy code latency p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_code_p99 = GaugeVec::new(Opts::new("strategy_code_latency_p99_us", "Strategy code latency p99 (us)"), &["strategy","symbol"]).unwrap();
        let strat_network_p50 = GaugeVec::new(Opts::new("strategy_network_latency_p50_us", "Strategy network latency p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_network_p99 = GaugeVec::new(Opts::new("strategy_network_latency_p99_us", "Strategy network latency p99 (us)"), &["strategy","symbol"]).unwrap();
        // New explicit post/cancel gauges
        let strat_post_p50 = GaugeVec::new(Opts::new("strategy_post_latency_p50_us", "Strategy post latency p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_post_p99 = GaugeVec::new(Opts::new("strategy_post_latency_p99_us", "Strategy post latency p99 (us)"), &["strategy","symbol"]).unwrap();
        let strat_cancel_p50 = GaugeVec::new(Opts::new("strategy_cancel_latency_p50_us", "Strategy cancel latency p50 (us)"), &["strategy","symbol"]).unwrap();
        let strat_cancel_p99 = GaugeVec::new(Opts::new("strategy_cancel_latency_p99_us", "Strategy cancel latency p99 (us)"), &["strategy","symbol"]).unwrap();
        // Request counters (increment-only)
        let strat_post_requests_total = IntCounterVec::new(Opts::new("strategy_post_requests_total", "Total post order requests sent"), &["strategy","symbol"]).unwrap();
        let strat_cancel_requests_total = IntCounterVec::new(Opts::new("strategy_cancel_requests_total", "Total cancel order requests sent"), &["strategy","symbol"]).unwrap();
        // WS-API metrics
        let ws_api_last_rtt_us = GaugeVec::new(Opts::new("ws_api_last_rtt_us", "Last WS-API round-trip latency (us)"), &["exchange","method"]).unwrap();
        let ws_api_requests_total = IntCounterVec::new(Opts::new("ws_api_requests_total", "WS-API requests by outcome"), &["exchange","method","outcome"]).unwrap();
        let ws_api_errors_total = IntCounterVec::new(Opts::new("ws_api_errors_total", "WS-API error codes"), &["exchange","method","code"]).unwrap();
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
        registry.register(Box::new(order_code_latency.clone())).unwrap();
        registry.register(Box::new(order_before_strategy.clone())).unwrap();
        registry.register(Box::new(order_req_network_latency.clone())).unwrap();
        registry.register(Box::new(order_rcv_network_latency.clone())).unwrap();
        registry.register(Box::new(strategy_pnl.clone())).unwrap();
        registry.register(Box::new(strategy_amount.clone())).unwrap();
        registry.register(Box::new(strategy_bps.clone())).unwrap();
        registry.register(Box::new(strategy_mid_price.clone())).unwrap();
        registry.register(Box::new(strat_tick_to_trade_p50.clone())).unwrap();
        registry.register(Box::new(strat_tick_to_trade_p99.clone())).unwrap();
        registry.register(Box::new(strat_tick_to_cancel_p50.clone())).unwrap();
        registry.register(Box::new(strat_tick_to_cancel_p99.clone())).unwrap();
        registry.register(Box::new(strat_code_p50.clone())).unwrap();
        registry.register(Box::new(strat_code_p99.clone())).unwrap();
        registry.register(Box::new(strat_network_p50.clone())).unwrap();
        registry.register(Box::new(strat_network_p99.clone())).unwrap();
        registry.register(Box::new(strat_post_p50.clone())).unwrap();
        registry.register(Box::new(strat_post_p99.clone())).unwrap();
        registry.register(Box::new(strat_cancel_p50.clone())).unwrap();
        registry.register(Box::new(strat_cancel_p99.clone())).unwrap();
        registry.register(Box::new(strat_post_requests_total.clone())).unwrap();
        registry.register(Box::new(strat_cancel_requests_total.clone())).unwrap();
        registry.register(Box::new(ws_api_last_rtt_us.clone())).unwrap();
        registry.register(Box::new(ws_api_requests_total.clone())).unwrap();
        registry.register(Box::new(ws_api_errors_total.clone())).unwrap();
        Self { registry, recv_total, processed_total, parse_errors_total, throughput, code_p50_us, code_p99_us, overall_p50_us, overall_p99_us, parse_p50_us, parse_p99_us, transform_p50_us, transform_p99_us, overhead_p50_us, overhead_p99_us, order_code_latency, order_before_strategy, order_req_network_latency, order_rcv_network_latency, strategy_pnl, strategy_amount, strategy_bps, strategy_mid_price, strat_tick_to_trade_p50, strat_tick_to_trade_p99, strat_tick_to_cancel_p50, strat_tick_to_cancel_p99, strat_code_p50, strat_code_p99, strat_network_p50, strat_network_p99, strat_post_p50, strat_post_p99, strat_cancel_p50, strat_cancel_p99, strat_post_requests_total, strat_cancel_requests_total, ws_api_last_rtt_us, ws_api_requests_total, ws_api_errors_total }
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

    /// Update strategy-level gauges in one call (fast, no allocations besides label lookup)
    pub fn set_strategy_metrics(&self, strategy: &str, symbol: &str, pnl: f64, amount: f64, bps: f64) {
        let labels = &[strategy, symbol];
        self.strategy_pnl.with_label_values(labels).set(pnl);
        self.strategy_amount.with_label_values(labels).set(amount);
        self.strategy_bps.with_label_values(labels).set(bps);
    }

    pub fn set_strategy_mid_price(&self, strategy: &str, symbol: &str, mid_price: f64) {
        let labels = &[strategy, symbol];
        self.strategy_mid_price.with_label_values(labels).set(mid_price);
    }

    pub fn set_strategy_latency_quantiles(&self, strategy: &str, symbol: &str,
        tick_to_trade_p50: Option<u64>, tick_to_trade_p99: Option<u64>,
        tick_to_cancel_p50: Option<u64>, tick_to_cancel_p99: Option<u64>,
        code_p50: Option<u64>, code_p99: Option<u64>,
        network_p50: Option<u64>, network_p99: Option<u64>) {
        let labels = &[strategy, symbol];
        if let Some(v) = tick_to_trade_p50 { self.strat_tick_to_trade_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = tick_to_trade_p99 { self.strat_tick_to_trade_p99.with_label_values(labels).set(v as f64); }
        if let Some(v) = tick_to_cancel_p50 { self.strat_tick_to_cancel_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = tick_to_cancel_p99 { self.strat_tick_to_cancel_p99.with_label_values(labels).set(v as f64); }
        if let Some(v) = code_p50 { self.strat_code_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = code_p99 { self.strat_code_p99.with_label_values(labels).set(v as f64); }
        if let Some(v) = network_p50 { self.strat_network_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = network_p99 { self.strat_network_p99.with_label_values(labels).set(v as f64); }
    }

    /// Explicitly set post/cancel latency gauges (use last-sample or quantiles if computed elsewhere)
    pub fn set_strategy_post_cancel_latencies(&self, strategy: &str, symbol: &str,
        post_p50: Option<u64>, post_p99: Option<u64>,
        cancel_p50: Option<u64>, cancel_p99: Option<u64>) {
        let labels = &[strategy, symbol];
        if let Some(v) = post_p50 { self.strat_post_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = post_p99 { self.strat_post_p99.with_label_values(labels).set(v as f64); }
        if let Some(v) = cancel_p50 { self.strat_cancel_p50.with_label_values(labels).set(v as f64); }
        if let Some(v) = cancel_p99 { self.strat_cancel_p99.with_label_values(labels).set(v as f64); }
    }

    /// Increment post/cancel request counters
    pub fn inc_strategy_post_request(&self, strategy: &str, symbol: &str) {
        self.strat_post_requests_total.with_label_values(&[strategy, symbol]).inc();
    }
    pub fn inc_strategy_cancel_request(&self, strategy: &str, symbol: &str) {
        self.strat_cancel_requests_total.with_label_values(&[strategy, symbol]).inc();
    }

    // WS-API helpers
    pub fn set_ws_api_last_rtt_us(&self, exchange: &str, method: &str, rtt_us: u64) {
        self.ws_api_last_rtt_us.with_label_values(&[exchange, method]).set(rtt_us as f64);
    }
    pub fn inc_ws_api_request_outcome(&self, exchange: &str, method: &str, outcome: &str) {
        self.ws_api_requests_total.with_label_values(&[exchange, method, outcome]).inc();
    }
    pub fn inc_ws_api_error_code(&self, exchange: &str, method: &str, code: &str) {
        self.ws_api_errors_total.with_label_values(&[exchange, method, code]).inc();
    }

    /// Observe order latency metrics in microseconds
    pub fn observe_order_latencies(&self, exchange: &str, symbol: &str, 
                                 code_latency: u64, before_strategy: u64, 
                                 req_network_latency: u64, rcv_network_latency: u64) {
        self.order_code_latency.with_label_values(&[exchange, symbol])
            .observe(code_latency as f64);
        self.order_before_strategy.with_label_values(&[exchange, symbol])
            .observe(before_strategy as f64);
        self.order_req_network_latency.with_label_values(&[exchange, symbol])
            .observe(req_network_latency as f64);
        self.order_rcv_network_latency.with_label_values(&[exchange, symbol])
            .observe(rcv_network_latency as f64);
    }
}


