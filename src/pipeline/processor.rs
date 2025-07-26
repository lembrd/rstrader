use tokio::sync::mpsc;
use std::time::SystemTime;

use crate::types::{OrderBookL2Update, OrderBookL2UpdateBuilder, RawMessage, Metrics, time};
use crate::error::{AppError, Result};
use crate::exchanges::binance::BinanceDepthUpdate;
use crate::exchanges::okx::{OkxDepthUpdate, OkxMessage};
use crate::types::ExchangeId;

/// Stream processor for transforming raw messages to unified format
pub struct StreamProcessor {
    sequence_counter: i64,
    packet_counter: i64,
    metrics: Metrics,
    updates_buffer: Vec<OrderBookL2Update>,
}

impl StreamProcessor {
    pub fn new() -> Self {
        Self {
            sequence_counter: 0,
            packet_counter: 0,
            metrics: Metrics::new(),
            updates_buffer: Vec::with_capacity(20), // Pre-allocate for typical message size
        }
    }
    
    fn process_binance_update(
        &mut self, 
        depth_update: BinanceDepthUpdate, 
        rcv_timestamp: i64, 
        packet_id: u64,
        message_bytes: u32
    ) -> Result<Vec<OrderBookL2Update>> {
        // Calculate overall latency (local time vs exchange timestamp)
        let exchange_timestamp_us = time::millis_to_micros(depth_update.event_time);
        if let Some(overall_latency) = rcv_timestamp.checked_sub(exchange_timestamp_us) {
            if overall_latency >= 0 {
                self.metrics.update_overall_latency(overall_latency as u64);
            }
        }
            
        // Start transformation timing
        let transform_start = time::now_micros();
        
        // Count bid/ask levels for complexity metrics
        let bid_count = depth_update.bids.len() as u32;
        let ask_count = depth_update.asks.len() as u32;
        
        // Reuse pre-allocated buffer
        self.updates_buffer.clear();
        
        let timestamp_micros = time::millis_to_micros(depth_update.event_time);
        
        // Process bid updates using fast float parsing
        for bid in depth_update.bids {
            let price = fast_float::parse::<f64, _>(&bid[0])
                .map_err(|e| AppError::pipeline(format!("Invalid bid price '{}': {}", bid[0], e)))?;
            let qty = fast_float::parse::<f64, _>(&bid[1])
                .map_err(|e| AppError::pipeline(format!("Invalid bid quantity '{}': {}", bid[1], e)))?;
            
            let seq_id = self.next_sequence_id(); // Extract to avoid borrow conflict
            let builder = OrderBookL2UpdateBuilder::new(
                timestamp_micros,
                rcv_timestamp,
                crate::types::ExchangeId::BinanceFutures,
                depth_update.symbol.clone(), // TODO: Use symbol interning
                seq_id,
                packet_id as i64,
                depth_update.final_update_id,
                depth_update.first_update_id,
            );
            
            self.updates_buffer.push(builder.build_bid(price, qty));
        }
        
        // Process ask updates using fast float parsing
        for ask in depth_update.asks {
            let price = fast_float::parse::<f64, _>(&ask[0])
                .map_err(|e| AppError::pipeline(format!("Invalid ask price '{}': {}", ask[0], e)))?;
            let qty = fast_float::parse::<f64, _>(&ask[1])
                .map_err(|e| AppError::pipeline(format!("Invalid ask quantity '{}': {}", ask[1], e)))?;
            
            let seq_id = self.next_sequence_id(); // Extract to avoid borrow conflict
            let builder = OrderBookL2UpdateBuilder::new(
                timestamp_micros,
                rcv_timestamp,
                crate::types::ExchangeId::BinanceFutures,
                depth_update.symbol.clone(), // TODO: Use symbol interning
                seq_id,
                packet_id as i64,
                depth_update.final_update_id,
                depth_update.first_update_id,
            );
            
            self.updates_buffer.push(builder.build_ask(price, qty));
        }
        
        // Calculate transformation time
        let transform_end = time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.metrics.update_transform_time(transform_time as u64);
        }
        
        // Update metrics
        self.metrics.increment_processed();
        self.metrics.update_message_complexity(bid_count, ask_count, message_bytes);
        
        // Update packet metrics (Binance sends 1 message per packet)
        let entries_in_packet = bid_count + ask_count;
        self.metrics.update_packet_metrics(1, entries_in_packet, message_bytes);
        
        // Return moved buffer instead of cloning
        Ok(std::mem::take(&mut self.updates_buffer))
    }

    fn process_okx_update(
        &mut self, 
        depth_update: &OkxDepthUpdate, 
        symbol: &str,
        rcv_timestamp: i64, 
        packet_id: u64,
        message_bytes: u32,
        exchange_id: ExchangeId
    ) -> Result<Vec<OrderBookL2Update>> {
        // Reuse pre-allocated buffer
        self.updates_buffer.clear();
        
        // Start transformation timing
        let transform_start = time::now_micros();
        
        // Track total bid/ask count across all data entries
        let mut total_bid_count = 0u32;
        let mut total_ask_count = 0u32;
        
        for data in &depth_update.data {
            let timestamp = data.timestamp.parse::<i64>()
                .map_err(|e| AppError::parse(format!("Invalid timestamp '{}': {}", data.timestamp, e)))?;

            let timestamp_micros = time::millis_to_micros(timestamp);
            
            // Calculate overall latency (local time vs exchange timestamp)
            if let Some(overall_latency) = rcv_timestamp.checked_sub(timestamp_micros) {
                if overall_latency >= 0 {
                    self.metrics.update_overall_latency(overall_latency as u64);
                }
            }
            
            let seq_id = data.seq_id.unwrap_or(0);
            let first_update_id = data.prev_seq_id.unwrap_or(seq_id);

            // Count bid/ask levels for complexity metrics
            total_bid_count += data.bids.len() as u32;
            total_ask_count += data.asks.len() as u32;

            // Process bids
            for bid_entry in &data.bids {
                if bid_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&bid_entry[0])
                        .map_err(|e| AppError::pipeline(format!("Invalid bid price '{}': {}", bid_entry[0], e)))?;
                    let size = fast_float::parse::<f64, _>(&bid_entry[1])
                        .map_err(|e| AppError::pipeline(format!("Invalid bid size '{}': {}", bid_entry[1], e)))?;

                    let builder = OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        exchange_id,
                        symbol.to_string(),
                        self.next_sequence_id(),
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );
                    
                    self.updates_buffer.push(builder.build_bid(price, size));
                }
            }

            // Process asks
            for ask_entry in &data.asks {
                if ask_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&ask_entry[0])
                        .map_err(|e| AppError::pipeline(format!("Invalid ask price '{}': {}", ask_entry[0], e)))?;
                    let size = fast_float::parse::<f64, _>(&ask_entry[1])
                        .map_err(|e| AppError::pipeline(format!("Invalid ask size '{}': {}", ask_entry[1], e)))?;

                    let builder = OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        exchange_id,
                        symbol.to_string(),
                        self.next_sequence_id(),
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );
                    
                    self.updates_buffer.push(builder.build_ask(price, size));
                }
            }
        }
        
        // Calculate transformation time
        let transform_end = time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.metrics.update_transform_time(transform_time as u64);
        }
        
        // Update metrics
        self.metrics.increment_processed();
        self.metrics.update_message_complexity(total_bid_count, total_ask_count, message_bytes);
        
        // Update packet metrics (OKX can send multiple messages in one packet)
        let messages_in_packet = depth_update.data.len() as u32;
        let entries_in_packet = total_bid_count + total_ask_count;
        self.metrics.update_packet_metrics(messages_in_packet, entries_in_packet, message_bytes);
        
        // Return moved buffer instead of cloning
        Ok(std::mem::take(&mut self.updates_buffer))
    }



    fn next_packet_id(&mut self) -> u64 {
        self.packet_counter += 1;
        self.packet_counter as u64
    }

    fn next_sequence_id(&mut self) -> i64 {
        self.sequence_counter += 1;
        self.sequence_counter
    }

    pub fn update_throughput(&mut self, msgs_per_sec: f64) {
        self.metrics.update_throughput(msgs_per_sec);
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    pub fn process_raw_message(&mut self, raw_msg: RawMessage, symbol: &str) -> Result<Vec<OrderBookL2Update>> {
        // Single timestamp capture at start
        let rcv_timestamp = time::now_micros();
        self.metrics.increment_received();
        
        let packet_id = self.next_packet_id();
        
        // Track message size
        let message_bytes = raw_msg.data.len() as u32;
        self.metrics.update_message_bytes(message_bytes);
        
        // Parse exchange-specific message format
        let parse_start = time::now_micros();
        let updates = match raw_msg.exchange_id {
            ExchangeId::BinanceFutures => {
                let mut data_bytes = raw_msg.data.into_bytes();
                let depth_update: BinanceDepthUpdate = serde_json::from_slice(&mut data_bytes).map_err(|e| {
                    self.metrics.increment_parse_errors();
                    AppError::pipeline(format!("Failed to parse Binance message: {}", e))
                })?;
                self.process_binance_update(depth_update, rcv_timestamp, packet_id, message_bytes)?
            }
            ExchangeId::OkxSwap | ExchangeId::OkxSpot => {
                // Parse OKX message JSON to get depth update structure
                let okx_message: crate::exchanges::okx::OkxMessage = serde_json::from_str(&raw_msg.data).map_err(|e| {
                    self.metrics.increment_parse_errors();
                    AppError::pipeline(format!("Failed to parse OKX message JSON: {}", e))
                })?;
                
                match okx_message {
                    crate::exchanges::okx::OkxMessage::Data(depth_update) => {
                        self.process_okx_update(&depth_update, symbol, rcv_timestamp, packet_id, message_bytes, raw_msg.exchange_id)?
                    },
                    crate::exchanges::okx::OkxMessage::Subscription { .. } => {
                        // Subscription acknowledgment - no data to process
                        Vec::new()
                    },
                    crate::exchanges::okx::OkxMessage::Error { .. } => {
                        self.metrics.increment_parse_errors();
                        return Err(AppError::pipeline("OKX error message received".to_string()));
                    }
                }
            }
        };

        // Calculate parse time
        let parse_end = time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.metrics.update_parse_time(parse_time as u64);
        }

        // Calculate code latency (full processing time)
        let code_end = time::now_micros();
        if let Some(code_latency) = code_end.checked_sub(rcv_timestamp) {
            self.metrics.update_code_latency(code_latency as u64);
        }

        // Update processed count
        self.metrics.increment_processed();

        Ok(updates)
    }
}

/// Alternative pipeline design (not currently used in main execution path)
pub struct MessagePipeline {
    processor: StreamProcessor,
    raw_sender: mpsc::UnboundedSender<RawMessage>,
    update_receiver: mpsc::UnboundedReceiver<OrderBookL2Update>,
}

impl MessagePipeline {
    pub fn new() -> Self {
        let (raw_sender, mut raw_receiver) = mpsc::unbounded_channel();
        let (update_sender, update_receiver) = mpsc::unbounded_channel();
        
        Self {
            processor: StreamProcessor::new(),
            raw_sender,
            update_receiver,
        }
    }

    pub fn raw_sender(&self) -> mpsc::UnboundedSender<RawMessage> {
        self.raw_sender.clone()
    }

    pub fn update_receiver(&mut self) -> &mut mpsc::UnboundedReceiver<OrderBookL2Update> {
        &mut self.update_receiver
    }

    pub async fn run(&mut self) -> Result<()> {
        // This would be the implementation for the alternative pipeline design
        Ok(())
    }
}

pub struct MetricsReporter {
    symbol: String,
    exchange: Option<ExchangeId>,
    last_report: std::time::Instant,
    report_interval: std::time::Duration,
}

impl MetricsReporter {
    pub fn new(symbol: String, interval_seconds: u64) -> Self {
        Self {
            symbol,
            exchange: None,
            last_report: std::time::Instant::now(),
            report_interval: std::time::Duration::from_secs(interval_seconds),
        }
    }

    pub fn set_exchange(&mut self, exchange_id: ExchangeId) {
        self.exchange = Some(exchange_id);
    }

    pub fn maybe_report(&mut self, metrics: &Metrics) {
        if self.last_report.elapsed() >= self.report_interval {
            let exchange_str = match self.exchange {
                Some(ref exchange) => format!("{:?}", exchange),
                None => "Unknown".to_string(),
            };
            log::info!("📊 Stream Metrics [{}@{}]: {}", self.symbol, exchange_str, metrics);
            self.last_report = std::time::Instant::now();
        }
    }
}

/// Main stream processor task
pub async fn run_stream_processor(
    mut rx: mpsc::Receiver<RawMessage>,
    tx: mpsc::Sender<OrderBookL2Update>,
    symbol: String,
    verbose: bool,
) -> Result<()> {
    let mut processor = StreamProcessor::new();
    let mut reporter = if verbose {
        Some(MetricsReporter::new(symbol.clone(), 10)) // Report every 10 seconds
    } else {
        None
    };

    log::info!("Stream processor started for {}", symbol);

    while let Some(raw_msg) = rx.recv().await {
        // Set exchange info on first message
        if let Some(ref mut reporter) = reporter {
            if reporter.exchange.is_none() {
                reporter.set_exchange(raw_msg.exchange_id.clone());
            }
        }

        match processor.process_raw_message(raw_msg, &symbol) {
            Ok(updates) => {
                for update in updates {
                    if let Err(e) = tx.send(update).await {
                        log::error!("Failed to send processed update: {}", e);
                        return Err(AppError::pipeline("Channel send failed".to_string()));
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to process raw message: {}", e);
                // Continue processing other messages for fail-fast behavior
            }
        }

        // Report metrics if verbose mode
        if let Some(ref mut reporter) = reporter {
            reporter.maybe_report(processor.metrics());
        }
    }

    log::info!("Stream processor for {} completed", symbol);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::time;

    #[test]
    fn test_stream_processor_creation() {
        let processor = StreamProcessor::new();
        assert_eq!(processor.sequence_counter, 0);
        assert_eq!(processor.packet_counter, 0);
    }

    #[test]
    fn test_metrics_reporter() {
        let mut reporter = MetricsReporter::new("TESTBTC".to_string(), 1);
        let metrics = Metrics::default();
        
        // Should not report immediately
        reporter.maybe_report(&metrics);
        
        // Should report after interval
        std::thread::sleep(std::time::Duration::from_secs(2));
        reporter.maybe_report(&metrics);
    }
}