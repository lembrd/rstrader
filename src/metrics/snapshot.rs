#[derive(Debug, Clone, Default)]
pub struct BasicSnapshot {
    pub messages_received: u64,
    pub messages_processed: u64,
    pub connection_attempts: u64,
    pub reconnections: u64,
    pub parse_errors: u64,

    pub last_code_latency_us: Option<u64>,
    pub avg_code_latency_us: Option<f64>,
    pub last_overall_latency_us: Option<u64>,
    pub avg_overall_latency_us: Option<f64>,
    pub throughput_msgs_per_sec: Option<f64>,

    pub last_parse_time_us: Option<u64>,
    pub avg_parse_time_us: Option<f64>,
    pub last_transform_time_us: Option<u64>,
    pub avg_transform_time_us: Option<f64>,
    pub last_overhead_time_us: Option<u64>,
    pub avg_overhead_time_us: Option<f64>,
}

use crate::xcommons::types as types;
impl From<&types::Metrics> for BasicSnapshot {
    fn from(m: &types::Metrics) -> Self {
        Self {
            messages_received: m.messages_received,
            messages_processed: m.messages_processed,
            connection_attempts: m.connection_attempts,
            reconnections: m.reconnections,
            parse_errors: m.parse_errors,
            last_code_latency_us: m.last_code_latency_us,
            avg_code_latency_us: m.avg_code_latency_us,
            last_overall_latency_us: m.last_overall_latency_us,
            avg_overall_latency_us: m.avg_overall_latency_us,
            throughput_msgs_per_sec: m.throughput_msgs_per_sec,
            last_parse_time_us: m.last_parse_time_us,
            avg_parse_time_us: m.avg_parse_time_us,
            last_transform_time_us: m.last_transform_time_us,
            avg_transform_time_us: m.avg_transform_time_us,
            last_overhead_time_us: m.last_overhead_time_us,
            avg_overhead_time_us: m.avg_overhead_time_us,
        }
    }
}


