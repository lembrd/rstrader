use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::time::SystemTime;
use tokio_tungstenite::{connect_async, WebSocketStream};
use tokio_tungstenite::tungstenite::protocol::Message;
use reqwest::Client;

use crate::exchanges::ExchangeConnector;
use crate::error::{AppError, Result};
use fast_float;
use crate::types::{
    ConnectionStatus, ExchangeId, OrderBookSnapshot, PriceLevel, RawMessage
};

/// Binance Futures WebSocket depth update message
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceDepthUpdate {
    #[serde(rename = "e")]
    pub event_type: String,         // "depthUpdate"
    
    #[serde(rename = "E")]
    pub event_time: i64,           // Event time in milliseconds
    
    #[serde(rename = "s")]
    pub symbol: String,            // Symbol (e.g., "BTCUSDT")
    
    #[serde(rename = "U")]
    pub first_update_id: i64,      // First update ID in event
    
    #[serde(rename = "u")]
    pub final_update_id: i64,      // Final update ID in event
    
    #[serde(rename = "pu")]
    pub prev_final_update_id: i64, // Final update ID in previous event (for validation)
    
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,    // Bids to be updated [price, qty]
    
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,    // Asks to be updated [price, qty]
}

/// Binance REST API depth response
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceDepthResponse {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: i64,
    
    pub bids: Vec<[String; 2]>,    // [price, qty]
    pub asks: Vec<[String; 2]>,    // [price, qty]
}

/// Connection state for managing synchronization
#[derive(Debug)]
enum SyncState {
    Disconnected,
    Connecting,
    BufferingUpdates,              // Buffering updates while getting snapshot
    Synchronizing { last_update_id: i64 },  // Synchronizing with snapshot
    Synchronized { last_update_id: i64 },   // Fully synchronized
    Resynchronizing,               // Error recovery - need new snapshot
}

/// Binance Futures connector implementation
pub struct BinanceFuturesConnector {
    rest_client: Client,
    ws_stream: Option<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
    connection_status: ConnectionStatus,
    sync_state: SyncState,
    symbol: Option<String>,
    update_buffer: VecDeque<BinanceDepthUpdate>,
    base_url: String,
    ws_url: String,
}

impl BinanceFuturesConnector {
    pub fn new() -> Self {
        Self {
            rest_client: Client::new(),
            ws_stream: None,
            connection_status: ConnectionStatus::Disconnected,
            sync_state: SyncState::Disconnected,
            symbol: None,
            update_buffer: VecDeque::new(),
            base_url: "https://fapi.binance.com".to_string(),
            ws_url: "wss://fstream.binance.com".to_string(),
        }
    }
    
    /// Get REST endpoint URL
    fn rest_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
    
    /// Get WebSocket stream URL
    fn ws_stream_url(&self, symbol: &str) -> String {
        // Use @depth@100ms for fastest updates (every 100ms or when order book changes)
        format!("{}/ws/{}@depth@0ms", self.ws_url, symbol.to_lowercase())
        // format!("{}/ws/{}@depth@100ms", self.ws_url, symbol.to_lowercase())
    }
    
    /// Parse price level from string array
    fn parse_price_level(level: &[String; 2]) -> Result<PriceLevel> {
        let price = fast_float::parse::<f64, _>(&level[0])
            .map_err(|e| AppError::parse(format!("Invalid price '{}': {}", level[0], e)))?;
            
        let qty = fast_float::parse::<f64, _>(&level[1])
            .map_err(|e| AppError::parse(format!("Invalid quantity '{}': {}", level[1], e)))?;
            
        Ok(PriceLevel { price, qty })
    }
    
    /// Parse depth update from WebSocket message
    fn parse_depth_update(&self, text: &str) -> Result<BinanceDepthUpdate> {
        serde_json::from_str(text)
            .map_err(|e| AppError::parse(format!("Failed to parse depth update: {}", e)))
    }
    
    /// Process buffered updates during synchronization
    fn process_buffered_updates(&mut self, snapshot_last_update_id: i64) -> Result<Vec<BinanceDepthUpdate>> {
        let mut valid_updates = Vec::new();
        
        // Remove updates older than snapshot
        self.update_buffer.retain(|update| update.final_update_id > snapshot_last_update_id);
        
        // Find first valid update
        if let Some(first_update) = self.update_buffer.front() {
            if first_update.first_update_id <= snapshot_last_update_id + 1 &&
               first_update.final_update_id >= snapshot_last_update_id + 1 {
                // Found valid starting point
                valid_updates.extend(self.update_buffer.drain(..).collect::<Vec<_>>());
            } else if first_update.first_update_id > snapshot_last_update_id + 1 {
                // Gap detected - need to resynchronize
                log::warn!("Gap detected in order book updates, resynchronizing");
                self.sync_state = SyncState::Resynchronizing;
                return Err(AppError::stream("Gap in order book updates detected"));
            }
        }
        
        Ok(valid_updates)
    }
    
    /// Validate update sequence
    fn validate_update_sequence(&self, update: &BinanceDepthUpdate, last_update_id: i64) -> Result<()> {
        if update.prev_final_update_id != last_update_id {
            return Err(AppError::stream(format!(
                "Update sequence mismatch: expected pu={}, got pu={}", 
                last_update_id, update.prev_final_update_id
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ExchangeConnector for BinanceFuturesConnector {
    type Error = AppError;
    
    async fn connect(&mut self) -> Result<()> {
        self.connection_status = ConnectionStatus::Connecting;
        self.sync_state = SyncState::Connecting;
        
        log::info!("Connecting to Binance Futures");
        self.connection_status = ConnectionStatus::Connected;
        
        Ok(())
    }
    
    async fn get_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot> {
        let url = self.rest_url(&format!("/fapi/v1/depth?symbol={}&limit=1000", symbol.to_uppercase()));
        
        log::debug!("Fetching order book snapshot from: {}", url);
        
        let response = self.rest_client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::connection(format!("Network error: {}", e)))?;
            
        if !response.status().is_success() {
            return Err(AppError::stream(format!("REST API error: {}", response.status())));
        }
        
        let depth_response: BinanceDepthResponse = response
            .json()
            .await
            .map_err(|e| AppError::connection(format!("Network error: {}", e)))?;
            
        // Parse bids and asks
        let mut bids = Vec::new();
        for bid in depth_response.bids {
            bids.push(Self::parse_price_level(&bid)?);
        }
        
        let mut asks = Vec::new();
        for ask in depth_response.asks {
            asks.push(Self::parse_price_level(&ask)?);
        }
        
        Ok(OrderBookSnapshot {
            symbol: symbol.to_string(),
            last_update_id: depth_response.last_update_id,
            exchange_id: ExchangeId::BinanceFutures,
            timestamp: crate::types::time::now_millis(),
            sequence: depth_response.last_update_id,
            bids,
            asks,
        })
    }
    
    async fn subscribe_l2(&mut self, symbol: &str) -> Result<()> {
        let ws_url = self.ws_stream_url(symbol);
        
        log::info!("Connecting to WebSocket: {}", ws_url);
        
        let (ws_stream, _) = connect_async(&ws_url).await
            .map_err(|e| AppError::connection(format!("WebSocket error: {}", e)))?;
            
        self.ws_stream = Some(ws_stream);
        self.symbol = Some(symbol.to_string());
        self.sync_state = SyncState::BufferingUpdates;
        
        log::info!("WebSocket connected, starting order book synchronization");
        
        Ok(())
    }
    
    async fn next_message(&mut self) -> Result<Option<RawMessage>> {
        let ws_stream = self.ws_stream.as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected"))?;
            
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let timestamp = SystemTime::now();
                
                // Try to parse as depth update
                match self.parse_depth_update(&text) {
                    Ok(update) => {
                        // Handle based on current sync state
                        match &self.sync_state {
                            SyncState::BufferingUpdates => {
                                // Buffer updates until we get snapshot
                                self.update_buffer.push_back(update);
                                // Continue buffering, don't return message yet
                                return self.next_message().await;
                            },
                            SyncState::Synchronizing { last_update_id } => {
                                // Check if this update is valid for synchronization
                                if update.final_update_id <= *last_update_id {
                                    // Skip old update
                                    return self.next_message().await;
                                }
                                
                                if update.first_update_id <= last_update_id + 1 &&
                                   update.final_update_id >= last_update_id + 1 {
                                    // Valid update - switch to synchronized state
                                    self.sync_state = SyncState::Synchronized { 
                                        last_update_id: update.final_update_id 
                                    };
                                    log::info!("Order book synchronized");
                                } else {
                                    // Gap detected
                                    self.sync_state = SyncState::Resynchronizing;
                                    return Err(AppError::stream("Gap in update sequence during synchronization"));
                                }
                            },
                            SyncState::Synchronized { last_update_id } => {
                                // Validate sequence
                                self.validate_update_sequence(&update, *last_update_id)?;
                                
                                // Update state
                                self.sync_state = SyncState::Synchronized { 
                                    last_update_id: update.final_update_id 
                                };
                            },
                            _ => {
                                return Err(AppError::stream(format!("Received update in invalid state: {:?}", self.sync_state)));
                            }
                        }
                    },
                    Err(_) => {
                        // Not a depth update, could be other message type
                        log::debug!("Non-depth update message received");
                    }
                }
                
                Ok(Some(RawMessage {
                    exchange_id: ExchangeId::BinanceFutures,
                    data: text,
                    timestamp,
                }))
            },
            Some(Ok(Message::Close(_))) => {
                log::warn!("WebSocket connection closed by server");
                self.connection_status = ConnectionStatus::Disconnected;
                self.sync_state = SyncState::Disconnected;
                Ok(None)
            },
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                // Handle ping/pong, continue reading
                self.next_message().await
            },
            Some(Ok(_)) => {
                // Other message types, continue reading
                self.next_message().await
            },
            Some(Err(e)) => {
                log::error!("WebSocket error: {}", e);
                self.connection_status = ConnectionStatus::Failed;
                Err(AppError::connection(format!("WebSocket error: {}", e)))
            },
            None => {
                log::warn!("WebSocket stream ended");
                self.connection_status = ConnectionStatus::Disconnected;
                Ok(None)
            }
        }
    }
    
    fn exchange_id(&self) -> ExchangeId {
        ExchangeId::BinanceFutures
    }
    
    fn connection_status(&self) -> ConnectionStatus {
        self.connection_status
    }
    
    async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut ws_stream) = self.ws_stream.take() {
            let _ = ws_stream.close(None).await;
        }
        
        self.connection_status = ConnectionStatus::Disconnected;
        self.sync_state = SyncState::Disconnected;
        self.symbol = None;
        self.update_buffer.clear();
        
        log::info!("Disconnected from Binance Futures");
        Ok(())
    }
    
    async fn validate_symbol(&self, symbol: &str) -> Result<()> {
        let url = self.rest_url(&format!("/fapi/v1/exchangeInfo"));
        
        log::debug!("Validating symbol {} with exchange info", symbol);
        
        let response = self.rest_client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::connection(format!("Failed to get exchange info: {}", e)))?;
            
        if !response.status().is_success() {
            return Err(AppError::validation(format!("Exchange info API error: {}", response.status())));
        }
        
        let exchange_info: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::connection(format!("Failed to parse exchange info: {}", e)))?;
            
        // Check if symbol exists in the symbols array
        if let Some(symbols) = exchange_info["symbols"].as_array() {
            for symbol_info in symbols {
                if let Some(s) = symbol_info["symbol"].as_str() {
                    if s.eq_ignore_ascii_case(symbol) {
                        // Found the symbol
                        if let Some(status) = symbol_info["status"].as_str() {
                            if status != "TRADING" {
                                return Err(AppError::validation(format!("Symbol {} is not in TRADING status: {}", symbol, status)));
                            }
                        }
                        log::info!("Symbol {} validated successfully", symbol);
                        return Ok(());
                    }
                }
            }
        }
        
        Err(AppError::validation(format!("Symbol {} not found", symbol)))
    }
    
    async fn start_depth_stream(&mut self, symbol: &str, tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<()> {
        self.validate_symbol(symbol).await?;
        self.subscribe_l2(symbol).await?;
        
        // Wait a moment for updates to start buffering
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        
        // Get snapshot to start synchronization 
        log::info!("Getting order book snapshot for synchronization");
        let snapshot = self.get_snapshot(symbol).await?;
        
        // Transition to synchronizing state
        self.sync_state = SyncState::Synchronizing { 
            last_update_id: snapshot.last_update_id 
        };
        log::info!("Transitioning to synchronizing state with update_id: {}", snapshot.last_update_id);
        
        log::info!("Starting depth stream for {}", symbol);
        
        loop {
            match self.next_message().await? {
                Some(msg) => {
                    if let Err(e) = tx.send(msg).await {
                        log::error!("Failed to send message to channel: {}", e);
                        return Err(AppError::channel(format!("Channel send failed: {}", e)));
                    }
                }
                None => {
                    log::warn!("Stream ended, attempting to reconnect...");
                    // Attempt reconnection
                    self.connect().await?;
                    self.subscribe_l2(symbol).await?;
                    
                    // Re-synchronize after reconnection
                    let snapshot = self.get_snapshot(symbol).await?;
                    self.sync_state = SyncState::Synchronizing { 
                        last_update_id: snapshot.last_update_id 
                    };
                }
            }
        }
    }
    
    async fn start_trade_stream(&mut self, _symbol: &str, _tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<()> {
        Err(AppError::fatal("Trade streams not implemented yet".to_string()))
    }
}

/// Helper function to create configured Binance connector
pub fn create_binance_connector() -> BinanceFuturesConnector {
    BinanceFuturesConnector::new()
}

/// Binance-specific message processor
pub struct BinanceProcessor {
    base: crate::exchanges::BaseProcessor,
}

impl BinanceProcessor {
    pub fn new() -> Self {
        Self {
            base: crate::exchanges::BaseProcessor::new(),
        }
    }
}

impl crate::exchanges::ExchangeProcessor for BinanceProcessor {
    type Error = crate::error::AppError;

    fn base_processor(&mut self) -> &mut crate::exchanges::BaseProcessor {
        &mut self.base
    }

    fn base_processor_ref(&self) -> &crate::exchanges::BaseProcessor {
        &self.base
    }

    fn process_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        _symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> std::result::Result<Vec<crate::types::OrderBookL2Update>, Self::Error> {
        // Track message received
        self.base.metrics.increment_received();
        
        // Calculate packet arrival timestamp (when network packet was received)
        let packet_arrival_us = raw_msg.timestamp.duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;
        
        // Parse Binance message with timing
        let parse_start = crate::types::time::now_micros();
        let mut data_bytes = raw_msg.data.into_bytes();
        let depth_update: BinanceDepthUpdate = serde_json::from_slice(&mut data_bytes).map_err(|e| {
            self.base.metrics.increment_parse_errors();
            crate::error::AppError::pipeline(format!("Failed to parse Binance message: {}", e))
        })?;
        
        // Calculate parse time
        let parse_end = crate::types::time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.base.metrics.update_parse_time(parse_time as u64);
        }

        // Calculate overall latency (local time vs exchange timestamp)
        let exchange_timestamp_us = crate::types::time::millis_to_micros(depth_update.event_time);
        if let Some(overall_latency) = rcv_timestamp.checked_sub(exchange_timestamp_us) {
            if overall_latency >= 0 {
                self.base.metrics.update_overall_latency(overall_latency as u64);
            }
        }
            
        // Start transformation timing
        let transform_start = crate::types::time::now_micros();
        
        // Calculate overhead time (time between parse end and transform start)
        if let Some(overhead_time) = transform_start.checked_sub(parse_end) {
            self.base.metrics.update_overhead_time(overhead_time as u64);
        }
        
        // Count bid/ask levels for complexity metrics
        let bid_count = depth_update.bids.len() as u32;
        let ask_count = depth_update.asks.len() as u32;
        
        // Reuse pre-allocated buffer
        self.base.updates_buffer.clear();
        
        let timestamp_micros = crate::types::time::millis_to_micros(depth_update.event_time);
        
        // Process bid updates using fast float parsing
        for bid in depth_update.bids {
            let price = fast_float::parse::<f64, _>(&bid[0])
                .map_err(|e| crate::error::AppError::pipeline(format!("Invalid bid price '{}': {}", bid[0], e)))?;
            let qty = fast_float::parse::<f64, _>(&bid[1])
                .map_err(|e| crate::error::AppError::pipeline(format!("Invalid bid quantity '{}': {}", bid[1], e)))?;
            
            let seq_id = self.next_sequence_id();
            let builder = crate::types::OrderBookL2UpdateBuilder::new(
                timestamp_micros,
                rcv_timestamp,
                crate::types::ExchangeId::BinanceFutures,
                depth_update.symbol.clone(),
                seq_id,
                packet_id as i64,
                depth_update.final_update_id,
                depth_update.first_update_id,
            );
            
            self.base.updates_buffer.push(builder.build_bid(price, qty));
        }
        
        // Process ask updates using fast float parsing
        for ask in depth_update.asks {
            let price = fast_float::parse::<f64, _>(&ask[0])
                .map_err(|e| crate::error::AppError::pipeline(format!("Invalid ask price '{}': {}", ask[0], e)))?;
            let qty = fast_float::parse::<f64, _>(&ask[1])
                .map_err(|e| crate::error::AppError::pipeline(format!("Invalid ask quantity '{}': {}", ask[1], e)))?;
            
            let seq_id = self.next_sequence_id();
            let builder = crate::types::OrderBookL2UpdateBuilder::new(
                timestamp_micros,
                rcv_timestamp,
                crate::types::ExchangeId::BinanceFutures,
                depth_update.symbol.clone(),
                seq_id,
                packet_id as i64,
                depth_update.final_update_id,
                depth_update.first_update_id,
            );
            
            self.base.updates_buffer.push(builder.build_ask(price, qty));
        }
        
        // Calculate transformation time
        let transform_end = crate::types::time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.base.metrics.update_transform_time(transform_time as u64);
        }
        
        // Update metrics
        self.base.metrics.increment_processed();
        self.base.metrics.update_message_complexity(bid_count, ask_count, message_bytes);
        
        // Update packet metrics (Binance sends 1 message per packet)
        let entries_in_packet = bid_count + ask_count;
        self.base.metrics.update_packet_metrics(1, entries_in_packet, message_bytes);
        
        // Calculate CODE LATENCY: From network packet arrival to business logic ready
        // This measures the FULL time spent in Rust code processing
        let processing_complete = crate::types::time::now_micros();
        if let Some(code_latency) = processing_complete.checked_sub(packet_arrival_us) {
            if code_latency >= 0 {
                self.base.metrics.update_code_latency(code_latency as u64);
            }
        }
        
        // Return moved buffer instead of cloning
        Ok(std::mem::take(&mut self.base.updates_buffer))
    }

    // Default implementations from trait are used
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_price_level() {
        let level = ["123.45".to_string(), "67.89".to_string()];
        let parsed = BinanceFuturesConnector::parse_price_level(&level).unwrap();
        
        assert_eq!(parsed.price, 123.45);
        assert_eq!(parsed.qty, 67.89);
    }
    
    #[test]
    fn test_parse_invalid_price_level() {
        let level = ["invalid".to_string(), "67.89".to_string()];
        let result = BinanceFuturesConnector::parse_price_level(&level);
        
        assert!(result.is_err());
    }
    
    #[test]
    fn test_connector_creation() {
        let connector = create_binance_connector();
        assert_eq!(connector.exchange_id(), ExchangeId::BinanceFutures);
        assert_eq!(connector.connection_status(), ConnectionStatus::Disconnected);
    }
}