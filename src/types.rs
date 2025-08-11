#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Unified data structure for L2 order book updates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookL2Update {
    // Common fields (as per project specification)
    pub timestamp: i64,       // Exchange timestamp (microseconds UTC)
    pub rcv_timestamp: i64,   // Local receive timestamp (microseconds UTC)
    pub exchange: ExchangeId, // Exchange identifier
    pub ticker: String,       // Instrument symbol (e.g., "BTCUSDT")
    pub seq_id: i64,          // Local monotonic sequence ID
    pub packet_id: i64,       // Network packet grouping ID

    // L2 specific fields
    pub update_id: i64,       // Exchange-specific update ID (Binance lastUpdateId)
    pub first_update_id: i64, // First update ID in event (Binance U field)
    pub action: L2Action,     // Action type (UPDATE for Binance)
    pub side: OrderSide,      // BID/ASK
    pub price: f64,           // Price level
    pub qty: f64,             // Quantity (0 = delete level)
}

/// Builder for OrderBookL2Update to eliminate code duplication
pub struct OrderBookL2UpdateBuilder {
    timestamp: i64,
    rcv_timestamp: i64,
    exchange: ExchangeId,
    ticker: String,
    seq_id: i64,
    packet_id: i64,
    update_id: i64,
    first_update_id: i64,
}

impl OrderBookL2UpdateBuilder {
    /// Create a new builder with required common fields
    pub fn new(
        timestamp: i64,
        rcv_timestamp: i64,
        exchange: ExchangeId,
        ticker: String,
        seq_id: i64,
        packet_id: i64,
        update_id: i64,
        first_update_id: i64,
    ) -> Self {
        Self {
            timestamp,
            rcv_timestamp,
            exchange,
            ticker,
            seq_id,
            packet_id,
            update_id,
            first_update_id,
        }
    }

    /// Build a bid update
    pub fn build_bid(self, price: f64, qty: f64) -> OrderBookL2Update {
        OrderBookL2Update {
            timestamp: self.timestamp,
            rcv_timestamp: self.rcv_timestamp,
            exchange: self.exchange,
            ticker: self.ticker,
            seq_id: self.seq_id,
            packet_id: self.packet_id,
            update_id: self.update_id,
            first_update_id: self.first_update_id,
            action: if qty == 0.0 {
                L2Action::Delete
            } else {
                L2Action::Update
            },
            side: OrderSide::Bid,
            price,
            qty,
        }
    }

    /// Build an ask update
    pub fn build_ask(self, price: f64, qty: f64) -> OrderBookL2Update {
        OrderBookL2Update {
            timestamp: self.timestamp,
            rcv_timestamp: self.rcv_timestamp,
            exchange: self.exchange,
            ticker: self.ticker,
            seq_id: self.seq_id,
            packet_id: self.packet_id,
            update_id: self.update_id,
            first_update_id: self.first_update_id,
            action: if qty == 0.0 {
                L2Action::Delete
            } else {
                L2Action::Update
            },
            side: OrderSide::Ask,
            price,
            qty,
        }
    }
}

/// L2 order book action types
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum L2Action {
    Update = 1,
    Delete = 2,
}
/// Trade update data structure for trade stream types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeUpdate {
    // Common fields (matching L2 structure)
    pub timestamp: i64,       // Exchange timestamp (microseconds UTC)
    pub rcv_timestamp: i64,   // Local receive timestamp (microseconds UTC)
    pub exchange: ExchangeId, // Exchange identifier
    pub ticker: String,       // Instrument symbol (e.g., "BTCUSDT")
    pub seq_id: i64,          // Local monotonic sequence ID
    pub packet_id: i64,       // Network packet grouping ID

    // Trade specific fields
    pub trade_id: String,     // Exchange-specific trade ID
    pub order_id: Option<String>, // Order ID if available
    pub side: TradeSide,      // BUY/SELL/UNKNOWN (direction of taker)
    pub price: f64,           // Trade execution price
    pub qty: f64,             // Trade quantity
}

/// Trade side enumeration (direction of taker)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TradeSide {
    Unknown = 0,
    Buy = 1,   // Taker bought (aggressor was buyer)
    Sell = -1, // Taker sold (aggressor was seller)
}

impl std::fmt::Display for TradeSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TradeSide::Buy => write!(f, "BUY"),
            TradeSide::Sell => write!(f, "SELL"),
            TradeSide::Unknown => write!(f, "UNKNOWN"),
        }
    }
}

/// Builder for TradeUpdate to maintain consistency with L2 pattern
pub struct TradeUpdateBuilder {
    timestamp: i64,
    rcv_timestamp: i64,
    exchange: ExchangeId,
    ticker: String,
    seq_id: i64,
    packet_id: i64,
    trade_id: String,
    order_id: Option<String>,
}

impl TradeUpdateBuilder {
    /// Create a new trade builder with required common fields
    pub fn new(
        timestamp: i64,
        rcv_timestamp: i64,
        exchange: ExchangeId,
        ticker: String,
        seq_id: i64,
        packet_id: i64,
        trade_id: String,
        order_id: Option<String>,
    ) -> Self {
        Self {
            timestamp,
            rcv_timestamp,
            exchange,
            ticker,
            seq_id,
            packet_id,
            trade_id,
            order_id,
        }
    }

    /// Build a trade update
    pub fn build(self, side: TradeSide, price: f64, qty: f64, _is_buyer_maker: bool) -> TradeUpdate {
        TradeUpdate {
            timestamp: self.timestamp,
            rcv_timestamp: self.rcv_timestamp,
            exchange: self.exchange,
            ticker: self.ticker,
            seq_id: self.seq_id,
            packet_id: self.packet_id,
            trade_id: self.trade_id,
            order_id: self.order_id,
            side,
            price,
            qty,
        }
    }
}

/// Enum to identify stream types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    L2,
    Trade,
}
/// Unified enum for different stream data types
#[derive(Debug, Clone)]
pub enum StreamData {
    L2(OrderBookL2Update),
    Trade(TradeUpdate),
}

impl StreamData {
    /// Get the stream type identifier
    pub fn stream_type(&self) -> &'static str {
        match self {
            StreamData::L2(_) => "L2",
            StreamData::Trade(_) => "TRADES",
        }
    }

    /// Get common fields for any stream type
    pub fn common_fields(&self) -> (i64, i64, ExchangeId, &str, i64, i64) {
        match self {
            StreamData::L2(update) => (
                update.timestamp,
                update.rcv_timestamp,
                update.exchange,
                &update.ticker,
                update.seq_id,
                update.packet_id,
            ),
            StreamData::Trade(update) => (
                update.timestamp,
                update.rcv_timestamp,
                update.exchange,
                &update.ticker,
                update.seq_id,
                update.packet_id,
            ),
        }
    }
}

impl std::fmt::Display for L2Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            L2Action::Update => write!(f, "UPDATE"),
            L2Action::Delete => write!(f, "DELETE"),
        }
    }
}

/// Order book side enumeration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum OrderSide {
    Bid = 1,
    Ask = -1,
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderSide::Bid => write!(f, "BID"),
            OrderSide::Ask => write!(f, "ASK"),
        }
    }
}

/// Exchange identifier enumeration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ExchangeId {
    BinanceFutures = 1,
    OkxSwap = 2,
    OkxSpot = 3,
    Deribit = 4,
}

impl std::fmt::Display for ExchangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeId::BinanceFutures => write!(f, "BINANCE_FUTURES"),
            ExchangeId::OkxSwap => write!(f, "OKX_SWAP"),
            ExchangeId::OkxSpot => write!(f, "OKX_SPOT"),
            ExchangeId::Deribit => write!(f, "DERIBIT"),
        }
    }
}

/// Connection status enumeration
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Error,
    Failed,
}

/// Raw message from exchange WebSocket
#[derive(Debug, Clone)]
pub struct RawMessage {
    pub exchange_id: ExchangeId,
    pub data: String,
    pub timestamp: SystemTime,
}

/// Order book snapshot from REST API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub exchange_id: ExchangeId,
    pub symbol: String,
    pub last_update_id: i64,
    pub timestamp: i64,
    pub sequence: i64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

/// Price level in order book
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: f64,
    pub qty: f64,
}

/// Metrics for performance monitoring
#[derive(Debug, Clone)]
pub struct Metrics {
    pub messages_received: u64,
    pub messages_processed: u64,
    pub connection_attempts: u64,
    pub reconnections: u64,
    pub parse_errors: u64,
    pub last_code_latency_us: Option<u64>, // Time from packet arrival to processing complete
    pub avg_code_latency_us: Option<f64>,
    pub last_overall_latency_us: Option<u64>, // Local time vs exchange timestamp difference
    pub avg_overall_latency_us: Option<f64>,
    pub throughput_msgs_per_sec: Option<f64>,

    // Message complexity metrics
    pub total_bids_processed: u64,
    pub total_asks_processed: u64,
    pub last_bid_count: u32,
    pub last_ask_count: u32,
    pub avg_bid_count: f64,
    pub avg_ask_count: f64,

    // Message size metrics
    pub last_message_bytes: u32,
    pub avg_message_bytes: f64,
    pub total_message_bytes: u64,

    // Packet aggregation metrics
    pub messages_per_packet: f64, // Average messages per packet
    pub market_data_entries_per_packet: f64, // Average bid/ask entries per packet
    pub avg_packet_size_bytes: f64, // Average packet size in bytes

    // Timing breakdown metrics
    pub last_parse_time_us: Option<u64>, // JSON parsing only
    pub avg_parse_time_us: Option<f64>,
    pub last_transform_time_us: Option<u64>, // Business logic transformation
    pub avg_transform_time_us: Option<f64>,
    pub last_overhead_time_us: Option<u64>, // Time between parse and transform
    pub avg_overhead_time_us: Option<f64>,

    // Running averages counters
    code_latency_count: u64,
    overall_latency_count: u64,
    parse_time_count: u64,
    transform_time_count: u64,
    overhead_time_count: u64,
    packet_count: u64,

    // Optional low-overhead histograms (disabled by default to avoid allocations)
    #[cfg(feature = "metrics-hdr")]
    #[allow(clippy::type_complexity)]
    pub(crate) latency_histograms: Option<crate::metrics::LatencyHistograms>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            messages_received: 0,
            messages_processed: 0,
            connection_attempts: 0,
            reconnections: 0,
            parse_errors: 0,
            last_code_latency_us: None,
            avg_code_latency_us: None,
            last_overall_latency_us: None,
            avg_overall_latency_us: None,
            throughput_msgs_per_sec: None,
            total_bids_processed: 0,
            total_asks_processed: 0,
            last_bid_count: 0,
            last_ask_count: 0,
            avg_bid_count: 0.0,
            avg_ask_count: 0.0,
            last_message_bytes: 0,
            avg_message_bytes: 0.0,
            total_message_bytes: 0,
            messages_per_packet: 0.0,
            market_data_entries_per_packet: 0.0,
            avg_packet_size_bytes: 0.0,
            last_parse_time_us: None,
            avg_parse_time_us: None,
            last_transform_time_us: None,
            avg_transform_time_us: None,
            last_overhead_time_us: None,
            avg_overhead_time_us: None,
            code_latency_count: 0,
            overall_latency_count: 0,
            parse_time_count: 0,
            transform_time_count: 0,
            overhead_time_count: 0,
            packet_count: 0,
            #[cfg(feature = "metrics-hdr")]
            latency_histograms: None,
        }
    }

    #[cfg(feature = "metrics-hdr")]
    pub fn enable_histograms(&mut self, bounds: crate::metrics::HistogramBounds) {
        use crate::metrics::LatencyHistograms;
        if self.latency_histograms.is_none() {
            self.latency_histograms = Some(LatencyHistograms::new(bounds));
        }
    }

    pub fn increment_received(&mut self) {
        self.messages_received += 1;
    }

    pub fn increment_processed(&mut self) {
        self.messages_processed += 1;
    }

    #[allow(dead_code)]
    pub fn increment_connection_attempts(&mut self) {
        self.connection_attempts += 1;
    }

    #[allow(dead_code)]
    pub fn increment_reconnections(&mut self) {
        self.reconnections += 1;
    }

    pub fn increment_parse_errors(&mut self) {
        self.parse_errors += 1;
    }

    pub fn update_code_latency(&mut self, latency_us: u64) {
        self.last_code_latency_us = Some(latency_us);
        self.code_latency_count += 1;

        // Update rolling average (simple moving average with weight 0.1 for new values)
        match self.avg_code_latency_us {
            Some(avg) => self.avg_code_latency_us = Some(avg * 0.9 + latency_us as f64 * 0.1),
            None => self.avg_code_latency_us = Some(latency_us as f64),
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &mut self.latency_histograms {
            h.record_code_latency(latency_us);
        }
    }

    pub fn update_overall_latency(&mut self, latency_us: u64) {
        self.last_overall_latency_us = Some(latency_us);
        self.overall_latency_count += 1;

        // Update rolling average (simple moving average with weight 0.1 for new values)
        match self.avg_overall_latency_us {
            Some(avg) => self.avg_overall_latency_us = Some(avg * 0.9 + latency_us as f64 * 0.1),
            None => self.avg_overall_latency_us = Some(latency_us as f64),
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &mut self.latency_histograms {
            h.record_overall_latency(latency_us);
        }
    }

    pub fn update_parse_time(&mut self, parse_time_us: u64) {
        self.last_parse_time_us = Some(parse_time_us);
        self.parse_time_count += 1;

        match self.avg_parse_time_us {
            Some(avg) => self.avg_parse_time_us = Some(avg * 0.9 + parse_time_us as f64 * 0.1),
            None => self.avg_parse_time_us = Some(parse_time_us as f64),
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &mut self.latency_histograms {
            h.record_parse_time(parse_time_us);
        }
    }

    pub fn update_transform_time(&mut self, transform_time_us: u64) {
        self.last_transform_time_us = Some(transform_time_us);
        self.transform_time_count += 1;

        match self.avg_transform_time_us {
            Some(avg) => {
                self.avg_transform_time_us = Some(avg * 0.9 + transform_time_us as f64 * 0.1)
            }
            None => self.avg_transform_time_us = Some(transform_time_us as f64),
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &mut self.latency_histograms {
            h.record_transform_time(transform_time_us);
        }
    }

    pub fn update_overhead_time(&mut self, overhead_time_us: u64) {
        self.last_overhead_time_us = Some(overhead_time_us);
        self.overhead_time_count += 1;

        match self.avg_overhead_time_us {
            Some(avg) => {
                self.avg_overhead_time_us = Some(avg * 0.9 + overhead_time_us as f64 * 0.1)
            }
            None => self.avg_overhead_time_us = Some(overhead_time_us as f64),
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &mut self.latency_histograms {
            h.record_overhead_time(overhead_time_us);
        }
    }

    pub fn update_message_complexity(
        &mut self,
        bid_count: u32,
        ask_count: u32,
        message_bytes: u32,
    ) {
        // Update counts
        self.last_bid_count = bid_count;
        self.last_ask_count = ask_count;
        self.total_bids_processed += bid_count as u64;
        self.total_asks_processed += ask_count as u64;

        // Update averages
        let total_messages = self.messages_processed as f64;
        if total_messages > 0.0 {
            self.avg_bid_count =
                (self.avg_bid_count * (total_messages - 1.0) + bid_count as f64) / total_messages;
            self.avg_ask_count =
                (self.avg_ask_count * (total_messages - 1.0) + ask_count as f64) / total_messages;
        } else {
            self.avg_bid_count = bid_count as f64;
            self.avg_ask_count = ask_count as f64;
        }

        // Update message size metrics
        self.last_message_bytes = message_bytes;
        self.total_message_bytes += message_bytes as u64;
        if total_messages > 0.0 {
            self.avg_message_bytes = (self.avg_message_bytes * (total_messages - 1.0)
                + message_bytes as f64)
                / total_messages;
        } else {
            self.avg_message_bytes = message_bytes as f64;
        }
    }

    pub fn update_packet_metrics(
        &mut self,
        messages_in_packet: u32,
        entries_in_packet: u32,
        packet_size_bytes: u32,
    ) {
        self.packet_count += 1;

        // Update messages per packet average
        let packet_count_f64 = self.packet_count as f64;
        self.messages_per_packet = (self.messages_per_packet * (packet_count_f64 - 1.0)
            + messages_in_packet as f64)
            / packet_count_f64;

        // Update market data entries per packet average
        self.market_data_entries_per_packet = (self.market_data_entries_per_packet
            * (packet_count_f64 - 1.0)
            + entries_in_packet as f64)
            / packet_count_f64;

        // Update average packet size
        self.avg_packet_size_bytes = (self.avg_packet_size_bytes * (packet_count_f64 - 1.0)
            + packet_size_bytes as f64)
            / packet_count_f64;
    }

    #[allow(dead_code)]
    pub fn update_message_bytes(&mut self, bytes: u32) {
        self.last_message_bytes = bytes;
        self.total_message_bytes += bytes as u64;
        self.avg_message_bytes = self.total_message_bytes as f64 / self.messages_received as f64;
    }

    pub fn update_throughput(&mut self, msgs_per_sec: f64) {
        self.throughput_msgs_per_sec = Some(msgs_per_sec);
    }

    pub fn increment_messages_processed(&mut self) {
        self.messages_processed += 1;
    }

    pub fn increment_batches_written(&mut self) {
        // This is a helper for tracking batches written
        self.messages_processed += 1; // Use existing counter for now
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "packets_received: {}, orderbook_updates: {}, errors: {}, connections: {}, reconnections: {}",
            self.messages_received,
            self.messages_processed,
            self.parse_errors,
            self.connection_attempts,
            self.reconnections
        )?;

        if let Some(code_latency) = self.last_code_latency_us {
            write!(f, ", code_latency: {}μs", code_latency)?;
        }

        if let Some(overall_latency) = self.last_overall_latency_us {
            write!(f, ", overall_latency: {}μs", overall_latency)?;
        }

        if let Some(avg_code) = self.avg_code_latency_us {
            write!(f, ", avg_code: {:.1}μs", avg_code)?;
        }

        if let Some(avg_overall) = self.avg_overall_latency_us {
            write!(f, ", avg_overall: {:.1}μs", avg_overall)?;
        }

        // Add new packet metrics
        if self.packet_count > 0 {
            write!(f, ", avg_packet_size: {:.0}B", self.avg_packet_size_bytes)?;
            write!(
                f,
                ", avg_md_entries: {:.1}/pkt",
                self.market_data_entries_per_packet
            )?;
            write!(f, ", avg_msgs: {:.1}/pkt", self.messages_per_packet)?;
        }

        if let Some(throughput) = self.throughput_msgs_per_sec {
            write!(f, ", throughput: {:.1}msg/s", throughput)?;
        }

        #[cfg(feature = "metrics-hdr")]
        if let Some(h) = &self.latency_histograms {
            // Non-mutating quantiles read
            let code_p50 = h.code_us.value_at_quantile(0.50);
            let code_p99 = h.code_us.value_at_quantile(0.99);
            let overall_p50 = h.overall_us.value_at_quantile(0.50);
            let overall_p99 = h.overall_us.value_at_quantile(0.99);
            write!(
                f,
                ", p50/p99(us) code {} / {}, overall {} / {}",
                code_p50, code_p99, overall_p50, overall_p99
            )?;
        }

        Ok(())
    }
}

/// Utility functions for timestamp handling
pub mod time {
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Get current timestamp in microseconds since Unix epoch
    pub fn now_micros() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_micros() as i64
    }

    /// Get current timestamp in milliseconds since Unix epoch
    pub fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as i64
    }

    /// Convert milliseconds to microseconds
    pub fn millis_to_micros(millis: i64) -> i64 {
        millis * 1000
    }

    /// Calculate latency in microseconds between two timestamps
    #[allow(dead_code)]
    pub fn latency_micros(start: SystemTime, end: SystemTime) -> u64 {
        end.duration_since(start).unwrap_or_default().as_micros() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_update() {
        let mut metrics = Metrics::new();

        assert_eq!(metrics.messages_received, 0);

        metrics.increment_received();
        assert_eq!(metrics.messages_received, 1);

        metrics.update_code_latency(100);
        assert_eq!(metrics.last_code_latency_us, Some(100));
        assert_eq!(metrics.avg_code_latency_us, Some(100.0));

        metrics.update_code_latency(200);
        assert_eq!(metrics.last_code_latency_us, Some(200));
        // Should be weighted average: 100 * 0.9 + 200 * 0.1 = 110
        assert_eq!(metrics.avg_code_latency_us, Some(110.0));
    }

    #[test]
    fn test_time_functions() {
        let now = time::now_micros();
        assert!(now > 0);

        let millis = time::now_millis();
        assert!(millis > 0);

        assert_eq!(time::millis_to_micros(1000), 1_000_000);
    }
}
