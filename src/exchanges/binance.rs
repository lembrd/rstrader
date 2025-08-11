#![allow(dead_code)]
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use std::collections::VecDeque;
use std::time::SystemTime;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{WebSocketStream, connect_async, MaybeTlsStream};
use tokio::net::lookup_host;
use tokio::net::TcpStream;

use crate::error::{AppError, Result};
use crate::exchanges::ExchangeConnector;
use crate::types::{ConnectionStatus, ExchangeId, OrderBookSnapshot, PriceLevel, RawMessage, TradeUpdate, TradeSide};
use fast_float;

/// Binance Futures WebSocket depth update message
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceDepthUpdate {
    #[serde(rename = "e")]
    pub event_type: String, // "depthUpdate"

    #[serde(rename = "E")]
    pub event_time: i64, // Event time in milliseconds

    #[serde(rename = "T")]
    pub transaction_time: i64, // Transaction time in milliseconds

    #[serde(rename = "s")]
    pub symbol: String, // Symbol (e.g., "BTCUSDT")

    #[serde(rename = "U")]
    pub first_update_id: i64, // First update ID in event

    #[serde(rename = "u")]
    pub final_update_id: i64, // Final update ID in event

    #[serde(rename = "pu")]
    pub prev_final_update_id: i64, // Final update ID in previous event (for validation)


    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>, // Bids to be updated [price, qty]

    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>, // Asks to be updated [price, qty]
}

/// Minimal buffered representation to avoid full JSON deserialization on the hot path
#[derive(Debug, Clone)]
struct BufferedDepthUpdate {
    first_update_id: i64,
    final_update_id: i64,
    prev_final_update_id: i64,
    // Original JSON text for replay to processor (single parse downstream)
    text: String,
}

/// Binance Futures WebSocket trade message
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceTradeUpdate {
    #[serde(rename = "e")]
    pub event_type: String, // "trade"

    #[serde(rename = "E")]
    pub event_time: i64, // Event time in milliseconds

    #[serde(rename = "s")]
    pub symbol: String, // Symbol (e.g., "BTCUSDT")

    #[serde(rename = "t")]
    pub trade_id: i64, // Trade ID

    #[serde(rename = "p")]
    pub price: String, // Trade price

    #[serde(rename = "q")]
    pub quantity: String, // Trade quantity







    #[serde(rename = "T")]
    pub trade_time: i64, // Trade time in milliseconds

    #[serde(rename = "m")]
    pub is_buyer_maker: bool, // Is buyer the maker (true = buyer was maker, false = buyer was taker)

    #[serde(rename = "M")]
    pub ignore: Option<bool>, // Ignore field (optional as it may not be present in all trade messages)
}

/// Binance REST API depth response
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceDepthResponse {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: i64,

    pub bids: Vec<[String; 2]>, // [price, qty]
    pub asks: Vec<[String; 2]>, // [price, qty]
}

/// Connection state for managing synchronization
#[derive(Debug)]
enum SyncState {
    Disconnected,
    Connecting,
    BufferingUpdates, // Buffering updates while getting snapshot
    Synchronizing { last_update_id: i64 }, // Synchronizing with snapshot
    Synchronized { last_update_id: i64 }, // Fully synchronized
    Resynchronizing,  // Error recovery - need new snapshot
    TradeStream,     // Connected for trade stream - no synchronization needed
}

/// Binance Futures connector implementation
pub struct BinanceFuturesConnector {
    rest_client: Client,
    ws_stream: Option<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
    connection_status: ConnectionStatus,
    sync_state: SyncState,
    symbol: Option<String>,
    update_buffer: VecDeque<BufferedDepthUpdate>,
    base_url: String,
    ws_url: String,
}

impl BinanceFuturesConnector {
    /// Maximum number of depth updates to buffer while waiting for snapshot
    /// This prevents unbounded memory growth if REST snapshot is slow/unavailable
    const MAX_BUFFERED_UPDATES: usize = 50_000;
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

    /// Get WebSocket trade stream URL
    fn ws_trade_stream_url(&self, symbol: &str) -> String {
        format!("{}/ws/{}@trade", self.ws_url, symbol.to_lowercase())
    }

    fn parse_host_port(url: &str, default_port: u16) -> Option<(String, u16)> {
        // Very small, dependency-free parser for wss://host[:port]/path
        let without_scheme = url.strip_prefix("wss://").or_else(|| url.strip_prefix("ws://"))?;
        let host_port_path = without_scheme;
        let parts: Vec<&str> = host_port_path.splitn(2, '/').collect();
        let host_port = parts.get(0).copied().unwrap_or("");
        if host_port.is_empty() { return None; }
        if let Some((host, port_str)) = host_port.split_once(':') {
            if let Ok(port) = port_str.parse::<u16>() { return Some((host.to_string(), port)); }
            return Some((host_port.to_string(), default_port));
        }
        Some((host_port.to_string(), default_port))
    }

    async fn log_dns_and_target(&self, url: &str, default_port: u16) {
        if let Some((host, port)) = Self::parse_host_port(url, default_port) {
            match lookup_host((host.as_str(), port)).await {
                Ok(addrs) => {
                    let addrs_vec: Vec<String> = addrs.map(|a| a.to_string()).collect();
                    log::info!("[dns][binance] host={} port={} resolved={:?}", host, port, addrs_vec);
                }
                Err(e) => {
                    log::warn!("[dns][binance] host={} port={} resolution failed: {}", host, port, e);
                }
            }
        } else {
            log::warn!("[dns][binance] failed to parse host from URL: {}", url);
        }
    }

    fn log_connected_addrs(stream: &WebSocketStream<MaybeTlsStream<TcpStream>>) {
        use tokio_tungstenite::MaybeTlsStream as MT;
        let inner = stream.get_ref();
        match inner {
            MT::Plain(tcp) => {
                if let (Ok(local), Ok(peer)) = (tcp.local_addr(), tcp.peer_addr()) {
                    log::info!("[net][binance] connected local={} peer={}", local, peer);
                }
            }
            #[allow(unused_imports)]
            MT::Rustls(tls) => {
                // tokio_rustls::client::TlsStream<TcpStream>
                let (io, _session) = tls.get_ref();
                if let (Ok(local), Ok(peer)) = (io.local_addr(), io.peer_addr()) {
                    log::info!("[net][binance] connected local={} peer={}", local, peer);
                }
            }
            #[cfg(any())]
            MT::NativeTls(tls) => {
                if let Some(io) = tls.get_ref() {
                    if let (Ok(local), Ok(peer)) = (io.local_addr(), io.peer_addr()) {
                        log::info!("[net][binance] connected local={} peer={}", local, peer);
                    }
                }
            }
            _ => {
                log::info!("[net][binance] connected (unrecognized TLS stream variant)");
            }
        }
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

    /// Parse trade update from WebSocket message
    fn parse_trade_update(&self, text: &str) -> Result<BinanceTradeUpdate> {
        serde_json::from_str(text)
            .map_err(|e| AppError::parse(format!("Failed to parse trade update: {}", e)))
    }

    /// Fast string scan to extract "U", "u", and "pu" integer fields without JSON deserialization
    fn scan_depth_u_fields(text: &str) -> Option<(i64, i64, i64)> {
        fn scan_number(text: &str, key: &str) -> Option<i64> {
            let needle = format!("\"{}\":", key);
            let pos = text.find(&needle)?;
            let mut i = pos + needle.len();
            // Skip whitespace
            let bytes = text.as_bytes();
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n' || bytes[i] == b'\r') { i += 1; }
            let start = i;
            // Optional minus sign (Binance IDs are non-negative, but be defensive)
            if i < bytes.len() && bytes[i] == b'-' { i += 1; }
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() { i += 1; }
            if i == start { return None; }
            text[start..i].parse::<i64>().ok()
        }

        let first_u = scan_number(text, "U")?;
        let final_u = scan_number(text, "u")?;
        let prev_u = scan_number(text, "pu")?;
        Some((first_u, final_u, prev_u))
    }

    /// Convert Binance trade update to internal TradeUpdate format
    fn convert_trade_update(&self, trade: &BinanceTradeUpdate, rcv_timestamp: i64, packet_id: i64) -> Result<TradeUpdate> {
        let price = fast_float::parse::<f64, _>(&trade.price)
            .map_err(|e| AppError::parse(format!("Invalid trade price '{}': {}", trade.price, e)))?;
        
        let qty = fast_float::parse::<f64, _>(&trade.quantity)
            .map_err(|e| AppError::parse(format!("Invalid trade quantity '{}': {}", trade.quantity, e)))?;

        let trade_side = if trade.is_buyer_maker { TradeSide::Sell } else { TradeSide::Buy };

        Ok(TradeUpdate {
            timestamp: crate::types::time::millis_to_micros(trade.trade_time),
            rcv_timestamp,
            exchange: ExchangeId::BinanceFutures,
            ticker: trade.symbol.clone(),
            seq_id: trade.trade_id,
            packet_id,
            trade_id: trade.trade_id.to_string(),
            order_id: Some(trade.trade_id.to_string()), // Using trade_id since buyer/seller order IDs removed from API
            side: trade_side,
            price,
            qty,
        })
    }

    /// Process buffered updates during synchronization
    fn process_buffered_updates(
        &mut self,
        snapshot_last_update_id: i64,
    ) -> Result<Vec<BufferedDepthUpdate>> {
        let mut valid_updates = Vec::new();

        // Remove updates older than snapshot
        self.update_buffer
            .retain(|update| update.final_update_id > snapshot_last_update_id);

        // Find first valid update
        if let Some(first_update) = self.update_buffer.front() {
            if first_update.first_update_id <= snapshot_last_update_id + 1
                && first_update.final_update_id >= snapshot_last_update_id + 1
            {
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
    fn validate_update_sequence_fields(&self, prev_final_update_id: i64, last_update_id: i64) -> Result<()> {
        if prev_final_update_id != last_update_id {
            return Err(AppError::stream(format!(
                "Update sequence mismatch: expected pu={}, got pu={}",
                last_update_id, prev_final_update_id
            )));
        }
        Ok(())
    }

    /// Initialize synchronization state with snapshot data
    /// This method should be called after get_snapshot() to properly transition
    /// from BufferingUpdates to Synchronizing state
    pub fn initialize_synchronization(&mut self, snapshot: &OrderBookSnapshot) -> Result<()> {
        // Process any buffered updates before transitioning state
        // Switch to Synchronizing with snapshot's update id. Do not drain buffer here;
        // buffered updates will be reconciled and replayed by take_l2_replay_messages().
        self.sync_state = SyncState::Synchronizing {
            last_update_id: snapshot.last_update_id,
        };
        log::info!(
            "Initialized Binance synchronization with update_id: {}",
            snapshot.last_update_id
        );
        Ok(())
    }

    /// Build RawMessage list for buffered depth updates to be replayed via processor
    fn drain_buffered_as_raw(&mut self) -> Vec<RawMessage> {
        let mut out = Vec::with_capacity(self.update_buffer.len());
        let exchange_id = ExchangeId::BinanceFutures;
        while let Some(update) = self.update_buffer.pop_front() {
            out.push(RawMessage { exchange_id, data: update.text, timestamp: std::time::SystemTime::now() });
        }
        out
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
        let url = self.rest_url(&format!(
            "/fapi/v1/depth?symbol={}&limit=1000",
            symbol.to_uppercase()
        ));

        // Fetching snapshot from Binance

        let response = self
            .rest_client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::connection(format!("Network error: {}", e)))?;

        if !response.status().is_success() {
            return Err(AppError::stream(format!(
                "REST API error: {}",
                response.status()
            )));
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

        self.log_dns_and_target(&ws_url, 443).await;
        log::info!("Connecting to WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .map_err(|e| AppError::connection(format!("WebSocket error: {}", e)))?;

        self.ws_stream = Some(ws_stream);
        if let Some(ref stream) = self.ws_stream { Self::log_connected_addrs(stream); }
        self.symbol = Some(symbol.to_string());
        
        // Only set to BufferingUpdates if not already synchronized
        // (UnifiedExchangeHandler may have already called initialize_synchronization)
        if matches!(self.sync_state, SyncState::Disconnected | SyncState::Connecting) {
            self.sync_state = SyncState::BufferingUpdates;
            // Setting sync state to BufferingUpdates
        } else {
            // Preserving existing sync state (already initialized synchronization)
        }

        log::info!("WebSocket connected, starting order book synchronization");

        Ok(())
    }

    async fn subscribe_trades(&mut self, symbol: &str) -> Result<()> {
        let ws_url = self.ws_trade_stream_url(symbol);

        self.log_dns_and_target(&ws_url, 443).await;
        log::info!("Connecting to Binance trades WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .map_err(|e| AppError::connection(format!("WebSocket error: {}", e)))?;

        self.ws_stream = Some(ws_stream);
        if let Some(ref stream) = self.ws_stream { Self::log_connected_addrs(stream); }
        self.symbol = Some(symbol.to_string());
        
        // For trades, we don't need synchronization - set TradeStream state
        // Trade streams don't require orderbook synchronization
        self.sync_state = SyncState::TradeStream;
        self.connection_status = ConnectionStatus::Connected;

        log::info!("Binance trades WebSocket connected for {}", symbol);

        Ok(())
    }

    async fn next_message(&mut self) -> Result<Option<RawMessage>> {
        loop {
            // Waiting for WebSocket message
            
            let msg = {
                let ws_stream = self
                    .ws_stream
                    .as_mut()
                    .ok_or_else(|| AppError::connection("WebSocket not connected"))?;
                ws_stream.next().await
            };

            match msg {
                Some(Ok(Message::Text(text))) => {
                    // Processing WebSocket message
                    let timestamp = SystemTime::now();

                    // Handle based on sync state - for trade streams, skip depth parsing
                    match &self.sync_state {
                        SyncState::TradeStream => {
                            // For trade streams, return raw message without depth parsing
                            // Trade messages don't need synchronization
                        }
                        _ => {
                            // Minimal scan to extract U/u/pu fields without full JSON parse
                            if let Some((first_update_id, final_update_id, prev_final_update_id)) = Self::scan_depth_u_fields(&text) {
                                match &self.sync_state {
                                    SyncState::BufferingUpdates => {
                                        // Buffer updates until we get snapshot
                                        self.update_buffer.push_back(BufferedDepthUpdate { first_update_id, final_update_id, prev_final_update_id, text: text.clone() });
                                        if self.update_buffer.len() > Self::MAX_BUFFERED_UPDATES {
                                            let to_trim = self.update_buffer.len() - Self::MAX_BUFFERED_UPDATES;
                                            for _ in 0..to_trim { self.update_buffer.pop_front(); }
                                            log::warn!("Binance buffer reached limit ({}). Dropped {} oldest updates to cap memory.", Self::MAX_BUFFERED_UPDATES, to_trim);
                                        }
                                        continue;
                                    }
                                    SyncState::Synchronizing { last_update_id } => {
                                        if final_update_id <= *last_update_id { continue; }
                                        if first_update_id > *last_update_id {
                                            log::info!("Accepting update after snapshot gap: {} > {}", first_update_id, last_update_id);
                                            self.sync_state = SyncState::Synchronized { last_update_id: final_update_id };
                                            log::info!("Order book synchronized with gap handling");
                                        } else if first_update_id <= last_update_id + 1 && final_update_id >= last_update_id + 1 {
                                            self.sync_state = SyncState::Synchronized { last_update_id: final_update_id };
                                            log::info!("Order book synchronized perfectly");
                                        } else {
                                            log::warn!("Unexpected sync case: first_id={}, final_id={}, expected > {}", first_update_id, final_update_id, last_update_id);
                                            self.sync_state = SyncState::Synchronized { last_update_id: final_update_id };
                                            log::info!("Order book synchronized with fallback");
                                        }
                                    }
                                    SyncState::Synchronized { last_update_id } => {
                                        self.validate_update_sequence_fields(prev_final_update_id, *last_update_id)?;
                                        self.sync_state = SyncState::Synchronized { last_update_id: final_update_id };
                                    }
                                    _ => {
                                        log::error!("Received update in invalid state: {:?}", self.sync_state);
                                        return Err(AppError::stream(format!("Received update in invalid state: {:?}", self.sync_state)));
                                    }
                                }
                            } else {
                                // Could be a non-depth update or control message; pass through
                            }
                        }
                    }

                    let raw_message = RawMessage {
                        exchange_id: ExchangeId::BinanceFutures,
                        data: text,
                        timestamp,
                    };return Ok(Some(raw_message));
                }
                Some(Ok(Message::Close(_))) => {
                    log::warn!("WebSocket connection closed by server");
                    self.connection_status = ConnectionStatus::Disconnected;
                    self.sync_state = SyncState::Disconnected;
                    return Ok(None);
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                    // Handle ping/pong, continue reading
                    // Received ping/pong
                    continue;
                }
                Some(Ok(_msg)) => {
                    // Other message types, continue reading
                    // Received other message type
                    continue;
                }
                Some(Err(e)) => {
                    log::error!("WebSocket error: {}", e);
                    self.connection_status = ConnectionStatus::Failed;
                    return Err(AppError::connection(format!("WebSocket error: {}", e)));
                }
                None => {
                    log::warn!("WebSocket stream ended");
                    self.connection_status = ConnectionStatus::Disconnected;
                    return Ok(None);
                }
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
        self.sync_state = SyncState::Disconnected;
        self.connection_status = ConnectionStatus::Disconnected;
        Ok(())
    }

    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.ws_stream.as_ref().and_then(|ws| {
            match ws.get_ref() {
                tokio_tungstenite::MaybeTlsStream::Plain(tcp) => tcp.peer_addr().ok(),
                tokio_tungstenite::MaybeTlsStream::Rustls(tls) => {
                    let (io, _) = tls.get_ref();
                    io.peer_addr().ok()
                }
                #[cfg(feature = "native-tls")]
                tokio_tungstenite::MaybeTlsStream::NativeTls(tls) => tls.get_ref().and_then(|io| io.peer_addr().ok()),
                _ => None,
            }
        })
    }

    async fn validate_symbol(&self, _symbol: &str) -> Result<()> {
        // For now, accept all symbols - could implement validation later
        Ok(())
    }

    async fn start_depth_stream(
        &mut self,
        symbol: &str,
        _tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<()> {
        self.subscribe_l2(symbol).await
    }

    async fn start_trade_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<()> {
        let ws_url = self.ws_trade_stream_url(symbol);

        self.log_dns_and_target(&ws_url, 443).await;
        log::info!("Connecting to Binance trade WebSocket: {}", ws_url);

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .map_err(|e| AppError::connection(format!("WebSocket error: {}", e)))?;

        self.ws_stream = Some(ws_stream);
        if let Some(ref stream) = self.ws_stream { Self::log_connected_addrs(stream); }
        self.symbol = Some(symbol.to_string());
        self.connection_status = ConnectionStatus::Connected;

        log::info!("Binance trade stream connected for {}", symbol);

        // Message forwarding loop
        loop {
            let msg = {
                let ws_stream = self
                    .ws_stream
                    .as_mut()
                    .ok_or_else(|| AppError::connection("WebSocket not connected"))?;
                ws_stream.next().await
            };

            match msg {
                Some(Ok(Message::Text(text))) => {
                    let timestamp = SystemTime::now();
                    
                    let raw_message = RawMessage {
                        exchange_id: ExchangeId::BinanceFutures,
                        data: text,
                        timestamp,
                    };

                    if tx.send(raw_message).await.is_err() {
                        log::info!("Trade stream channel closed, stopping");
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    log::warn!("Binance trade WebSocket connection closed by server");
                    self.connection_status = ConnectionStatus::Disconnected;
                    break;
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                    // Handle ping/pong, continue reading
                    continue;
                }
                Some(Ok(_)) => {
                    // Other message types, continue reading
                    continue;
                }
                Some(Err(e)) => {
                    log::error!("Binance trade WebSocket error: {}", e);
                    self.connection_status = ConnectionStatus::Failed;
                    return Err(AppError::connection(format!("WebSocket error: {}", e)));
                }
                None => {
                    log::warn!("Binance trade WebSocket stream ended");
                    self.connection_status = ConnectionStatus::Disconnected;
                    break;
                }
            }
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn prepare_l2_sync(&mut self, snapshot: &OrderBookSnapshot) -> Result<()> {
        // Initialize internal state for synchronization
        self.initialize_synchronization(snapshot)
    }

    fn take_l2_replay_messages(&mut self) -> Option<Vec<RawMessage>> {
        // Only reconcile and replay if we are in Synchronizing state with a known last_update_id
        let mut last_id_opt: Option<i64> = None;
        if let SyncState::Synchronizing { last_update_id } = self.sync_state {
            last_id_opt = Some(last_update_id);
        }

        if let Some(last_id) = last_id_opt {
            if let Ok(valid) = self.process_buffered_updates(last_id) {
                if valid.is_empty() && self.update_buffer.is_empty() {
                    return None;
                }
                // valid were drained by process_buffered_updates(); now encode all drained items
                let msgs = self.drain_buffered_as_raw();
                return if msgs.is_empty() { None } else { Some(msgs) };
            }
        }
        None
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
        // Use socket receive time from RawMessage for code latency measurement
        let packet_arrival_us = raw_msg
            .timestamp
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        // Parse Binance message with timing
        let parse_start = crate::types::time::now_micros();
        
        let mut data_bytes = raw_msg.data.into_bytes();
        
        // First try to parse as generic JSON to check for subscription responses
        let json_value: serde_json::Value = serde_json::from_slice(&mut data_bytes).map_err(|e| {
            self.base.metrics.increment_parse_errors();
            crate::error::AppError::pipeline(format!("Failed to parse Binance JSON: {}", e))
        })?;



        // Check if this is a subscription response (has "result" field)
        if json_value.get("result").is_some() {
            // This is a subscription confirmation - no depth updates to process
            return Ok(vec![]);
        }

        // Check if this has at least one of the required fields for depth update
        // Binance @depth@0ms sends incremental updates that may contain only bids OR only asks
        // Note: Binance uses "b" for bids and "a" for asks, not "bids"/"asks"
        if json_value.get("b").is_none() && json_value.get("a").is_none() {
            // Not a depth update - might be other message type, skip silently
            log::debug!("Skipping message without b/a fields: {}", json_value);
            return Ok(vec![]);
        }

        // Parse as depth update
        let depth_update: BinanceDepthUpdate = serde_json::from_value(json_value).map_err(|e| {
            self.base.metrics.increment_parse_errors();
            log::error!("CRITICAL: Failed to parse Binance depth update: {}", e);
            crate::error::AppError::pipeline(format!("Failed to parse Binance depth update: {}", e))
        })?;
        
        // Removed per user request to reduce log spam

        // Calculate parse time
        let parse_end = crate::types::time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.base.metrics.update_parse_time(parse_time as u64);
        }

        // Calculate overall latency (local time vs exchange timestamp)
        let exchange_timestamp_us = crate::types::time::millis_to_micros(depth_update.event_time);
        if let Some(overall_latency) = rcv_timestamp.checked_sub(exchange_timestamp_us) {
            if overall_latency >= 0 {
                self.base
                    .metrics
                    .update_overall_latency(overall_latency as u64);
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
            let price = fast_float::parse::<f64, _>(&bid[0]).map_err(|e| {
                crate::error::AppError::pipeline(format!("Invalid bid price '{}': {}", bid[0], e))
            })?;
            let qty = fast_float::parse::<f64, _>(&bid[1]).map_err(|e| {
                crate::error::AppError::pipeline(format!(
                    "Invalid bid quantity '{}': {}",
                    bid[1], e
                ))
            })?;

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
            let price = fast_float::parse::<f64, _>(&ask[0]).map_err(|e| {
                crate::error::AppError::pipeline(format!("Invalid ask price '{}': {}", ask[0], e))
            })?;
            let qty = fast_float::parse::<f64, _>(&ask[1]).map_err(|e| {
                crate::error::AppError::pipeline(format!(
                    "Invalid ask quantity '{}': {}",
                    ask[1], e
                ))
            })?;

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
            self.base
                .metrics
                .update_transform_time(transform_time as u64);
        }

        // Update metrics
        self.base.metrics.increment_processed();
        self.base
            .metrics
            .update_message_complexity(bid_count, ask_count, message_bytes);

        // Update packet metrics (Binance sends 1 message per packet)
        let entries_in_packet = bid_count + ask_count;
        self.base
            .metrics
            .update_packet_metrics(1, entries_in_packet, message_bytes);

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

    fn process_trade_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        _symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> std::result::Result<Vec<crate::types::TradeUpdate>, Self::Error> {
        // Track message received
        self.base.metrics.increment_received();

        // Calculate packet arrival timestamp (when network packet was received)
        // Use socket receive time from RawMessage for code latency measurement
        let packet_arrival_us = raw_msg
            .timestamp
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        // Parse Binance trade message with timing
        let parse_start = crate::types::time::now_micros();
        let mut data_bytes = raw_msg.data.into_bytes();
        let trade_update: BinanceTradeUpdate =
            serde_json::from_slice(&mut data_bytes).map_err(|e| {
                self.base.metrics.increment_parse_errors();
                crate::error::AppError::pipeline(format!("Failed to parse Binance trade message: {}", e))
            })?;

        // Calculate parse time
        let parse_end = crate::types::time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.base.metrics.update_parse_time(parse_time as u64);
        }

        // Calculate overall latency (local time vs exchange timestamp)
        let exchange_timestamp_us = crate::types::time::millis_to_micros(trade_update.trade_time);
        if let Some(overall_latency) = rcv_timestamp.checked_sub(exchange_timestamp_us) {
            if overall_latency >= 0 {
                self.base
                    .metrics
                    .update_overall_latency(overall_latency as u64);
            }
        }

        // Start transformation timing
        let transform_start = crate::types::time::now_micros();

        // Calculate overhead time (time between parse end and transform start)
        if let Some(overhead_time) = transform_start.checked_sub(parse_end) {
            self.base.metrics.update_overhead_time(overhead_time as u64);
        }

        // Parse price and quantity
        let price = fast_float::parse::<f64, _>(&trade_update.price).map_err(|e| {
            crate::error::AppError::pipeline(format!("Invalid trade price '{}': {}", trade_update.price, e))
        })?;
        
        let qty = fast_float::parse::<f64, _>(&trade_update.quantity).map_err(|e| {
            crate::error::AppError::pipeline(format!("Invalid trade quantity '{}': {}", trade_update.quantity, e))
        })?;

        let trade_side = if trade_update.is_buyer_maker { crate::types::TradeSide::Sell } else { crate::types::TradeSide::Buy };

        let seq_id = self.next_sequence_id();
        let trade = crate::types::TradeUpdate {
            timestamp: crate::types::time::millis_to_micros(trade_update.trade_time),
            rcv_timestamp,
            exchange: crate::types::ExchangeId::BinanceFutures,
            ticker: trade_update.symbol,
            seq_id,
            packet_id: packet_id as i64,
            trade_id: trade_update.trade_id.to_string(),
            order_id: None, // Binance doesn't provide order IDs in trade stream
            side: trade_side,
            price,
            qty,
        };

        // Calculate transformation time
        let transform_end = crate::types::time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.base
                .metrics
                .update_transform_time(transform_time as u64);
        }

        // Update metrics
        self.base.metrics.increment_processed();
        self.base.metrics.update_message_complexity(0, 0, message_bytes); // Trade message has no bid/ask levels

        // Update packet metrics (Binance sends 1 trade per message)
        self.base.metrics.update_packet_metrics(1, 1, message_bytes);

        // Calculate CODE LATENCY: From network packet arrival to business logic ready
        let processing_complete = crate::types::time::now_micros();
        if let Some(code_latency) = processing_complete.checked_sub(packet_arrival_us) {
            if code_latency >= 0 {
                self.base.metrics.update_code_latency(code_latency as u64);
            }
        }

        Ok(vec![trade])
    }
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
        assert_eq!(
            connector.connection_status(),
            ConnectionStatus::Disconnected
        );
    }
}
