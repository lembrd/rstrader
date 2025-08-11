use hdrhistogram::Histogram;

#[derive(Clone, Copy)]
pub struct HistogramBounds {
    pub lowest: u64,
    pub highest: u64,
    pub sigfig: u8,
}

impl Default for HistogramBounds {
    fn default() -> Self {
        // microseconds latency from 1us to 10s, 2 significant digits by default
        Self { lowest: 1, highest: 10_000_000, sigfig: 2 }
    }
}

pub struct LatencyHistograms {
    pub code_us: Histogram<u64>,
    pub overall_us: Histogram<u64>,
    pub parse_us: Histogram<u64>,
    pub transform_us: Histogram<u64>,
    pub overhead_us: Histogram<u64>,
}

impl LatencyHistograms {
    pub fn new(bounds: HistogramBounds) -> Self {
        let mk = || Histogram::new_with_bounds(bounds.lowest, bounds.highest, bounds.sigfig)
            .expect("invalid histogram bounds");
        Self {
            code_us: mk(),
            overall_us: mk(),
            parse_us: mk(),
            transform_us: mk(),
            overhead_us: mk(),
        }
    }

    #[inline(always)]
    pub fn record_code_latency(&mut self, v: u64) {
        let _ = self.code_us.record(v);
    }
    #[inline(always)]
    pub fn record_overall_latency(&mut self, v: u64) {
        let _ = self.overall_us.record(v);
    }
    #[inline(always)]
    pub fn record_parse_time(&mut self, v: u64) {
        let _ = self.parse_us.record(v);
    }
    #[inline(always)]
    pub fn record_transform_time(&mut self, v: u64) {
        let _ = self.transform_us.record(v);
    }
    #[inline(always)]
    pub fn record_overhead_time(&mut self, v: u64) {
        let _ = self.overhead_us.record(v);
    }

    pub fn snapshot_and_reset(&mut self) -> MetricsSnapshot {
        let snap = MetricsSnapshot {
            code_p50: self.code_us.value_at_quantile(0.50),
            code_p90: self.code_us.value_at_quantile(0.90),
            code_p99: self.code_us.value_at_quantile(0.99),
            overall_p50: self.overall_us.value_at_quantile(0.50),
            overall_p90: self.overall_us.value_at_quantile(0.90),
            overall_p99: self.overall_us.value_at_quantile(0.99),
            parse_p50: self.parse_us.value_at_quantile(0.50),
            parse_p90: self.parse_us.value_at_quantile(0.90),
            parse_p99: self.parse_us.value_at_quantile(0.99),
            transform_p50: self.transform_us.value_at_quantile(0.50),
            transform_p90: self.transform_us.value_at_quantile(0.90),
            transform_p99: self.transform_us.value_at_quantile(0.99),
            overhead_p50: self.overhead_us.value_at_quantile(0.50),
            overhead_p90: self.overhead_us.value_at_quantile(0.90),
            overhead_p99: self.overhead_us.value_at_quantile(0.99),
        };

        self.code_us.reset();
        self.overall_us.reset();
        self.parse_us.reset();
        self.transform_us.reset();
        self.overhead_us.reset();
        snap
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsSnapshot {
    pub code_p50: u64,
    pub code_p90: u64,
    pub code_p99: u64,
    pub overall_p50: u64,
    pub overall_p90: u64,
    pub overall_p99: u64,
    pub parse_p50: u64,
    pub parse_p90: u64,
    pub parse_p99: u64,
    pub transform_p50: u64,
    pub transform_p90: u64,
    pub transform_p99: u64,
    pub overhead_p50: u64,
    pub overhead_p90: u64,
    pub overhead_p99: u64,
}


