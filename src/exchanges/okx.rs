//
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::collections::VecDeque;
use lru::LruCache;
use std::num::NonZeroUsize;

// Static error messages to avoid allocations in hot paths
const ERROR_JSON_PARSE: &str = "Failed to parse OKX message JSON";
const ERROR_PRICE_PARSE: &str = "Invalid price format";
const ERROR_QUANTITY_PARSE: &str = "Invalid quantity format";
const ERROR_TIMESTAMP_PARSE: &str = "Invalid timestamp format";
const ERROR_WEBSOCKET: &str = "WebSocket connection error";
const ERROR_SEQUENCE_GAP: &str = "OKX sequence gap detected";
use std::sync::Arc;
use std::time::SystemTime;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio::net::lookup_host;

use crate::xcommons::error::{AppError, Result};
use crate::exchanges::ExchangeConnector;
use crate::xcommons::types::{
    ConnectionStatus, ExchangeId, OrderBookL2Update, OrderBookL2UpdateBuilder,
    OrderBookSnapshot, PriceLevel, RawMessage, TradeUpdate,
};
use crate::xcommons::oms::Side;
use fast_float;

/// OKX WebSocket message wrapper for different types
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OkxMessage {
    /// Subscription acknowledgment message
    Subscription {
        #[serde(rename = "event")]
        event: String,
        #[serde(rename = "arg")]
        arg: OkxChannelArg,
    },
    /// Data message
    Data(OkxDepthUpdate),
    /// Error message  
    Error {
        #[serde(rename = "event")]
        event: String,
        #[serde(rename = "code")]
        code: String,
        #[serde(rename = "msg")]
        msg: String,
    },
}

/// OKX WebSocket message structures
#[derive(Debug, Clone, Deserialize)]
pub struct OkxDepthUpdate {
    #[serde(rename = "arg")]
    pub arg: OkxChannelArg,
    #[serde(rename = "action")]
    pub action: Option<String>, // "snapshot" or "update"
    #[serde(rename = "data")]
    pub data: Vec<OkxDepthData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkxChannelArg {
    #[serde(rename = "channel")]
    pub channel: String,
    #[serde(rename = "instId")]
    pub inst_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkxDepthData {
    #[serde(rename = "asks")]
    pub asks: Vec<Vec<String>>, // [price, size, liquidated_orders, orders]
    #[serde(rename = "bids")]
    pub bids: Vec<Vec<String>>, // [price, size, liquidated_orders, orders]
    #[serde(rename = "ts")]
    pub timestamp: String,
    #[serde(rename = "checksum")]
    pub checksum: Option<i64>,
    #[serde(rename = "prevSeqId")]
    pub prev_seq_id: Option<i64>,
    #[serde(rename = "seqId")]
    pub seq_id: Option<i64>,
}

/// OKX trade update structure
#[derive(Debug, Clone, Deserialize)]
pub struct OkxTradeUpdate {
    #[serde(rename = "arg")]
    pub arg: OkxChannelArg,
    #[serde(rename = "data")]
    pub data: Vec<OkxTradeData>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkxTradeData {
    #[serde(rename = "instId")]
    pub inst_id: String, // Instrument ID (e.g., "BTC-USDT-SWAP")
    #[serde(rename = "tradeId")]
    pub trade_id: String, // Trade ID
    #[serde(rename = "px")]
    pub price: String, // Trade price
    #[serde(rename = "sz")]
    pub size: String, // Trade size
    #[serde(rename = "side")]
    pub side: String, // Trade side: "buy" or "sell"
    #[serde(rename = "ts")]
    pub timestamp: String, // Trade timestamp
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkxInstrumentInfo {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "instType")]
    pub inst_type: String,
    #[serde(rename = "state")]
    pub state: String,
    // NEW FIELDS FOR UNIT CONVERSION
    #[serde(rename = "lotSz")]
    pub lot_size: String,
    #[serde(rename = "minSz", default)]
    pub min_size: Option<String>,
    #[serde(rename = "ctMult", default)]
    pub contract_multiplier: Option<String>,
    #[serde(rename = "ctVal", default)]
    pub contract_value: Option<String>,
    #[serde(rename = "tickSz")]
    pub tick_size: String,
    // Additional useful fields
    #[serde(rename = "baseCcy", default)]
    pub base_currency: Option<String>,
    #[serde(rename = "quoteCcy", default)]
    pub quote_currency: Option<String>,
}

/// Error types specific to unit conversion
#[derive(Debug, thiserror::Error)]
pub enum ConversionError {
    #[error("Instrument metadata not found for {0}")]
    MetadataNotFound(String),
    #[error("Invalid contract multiplier for {0}: {1}")]
    InvalidMultiplier(String, String),
    #[error("Failed to parse instrument metadata: {0}")]
    ParseError(String),
}

/// Processed instrument metadata for unit conversion
#[derive(Debug, Clone)]
pub struct InstrumentMetadata {
    pub inst_id: String,
    pub inst_type: String,
    pub lot_size: f64,
    pub contract_multiplier: Option<f64>,
    pub contract_value: Option<f64>,
    pub tick_size: f64,
    pub base_currency: Option<String>,
    pub quote_currency: Option<String>,
    pub last_updated: std::time::Instant,
}

/// Registry for caching instrument metadata with singleton pattern support
#[derive(Debug)]
pub struct InstrumentRegistry {
    cache: std::collections::HashMap<String, InstrumentMetadata>,
}

impl InstrumentRegistry {
    /// Create new empty registry
    pub fn new() -> Self {
        Self {
            cache: std::collections::HashMap::new(),
        }
    }

    /// Initialize registry with instruments from OKX API (singleton initialization)
    pub async fn initialize_with_instruments(
        client: &reqwest::Client,
        base_url: &str,
        inst_type: &str,
    ) -> Result<Self> {
        let url = format!(
            "{}/api/v5/public/instruments?instType={}",
            base_url, inst_type
        );

        log::info!(
            "Loading {} instruments for unit conversion cache",
            inst_type
        );

        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::connection(format!("Failed to get instruments: {}", e)))?;

        if !response.status().is_success() {
            return Err(AppError::connection(format!(
                "HTTP error getting instruments: {}",
                response.status()
            )));
        }

        #[derive(serde::Deserialize)]
        struct InstrumentResponse {
            code: String,
            msg: String,
            data: Vec<OkxInstrumentInfo>,
        }

        let data: InstrumentResponse = response
            .json()
            .await
            .map_err(|e| AppError::parse(format!("Failed to parse instruments response: {}", e)))?;

        if data.code != "0" {
            return Err(AppError::connection(format!(
                "OKX API error getting instruments: {} - {}",
                data.code, data.msg
            )));
        }

        let mut cache = std::collections::HashMap::new();
        for info in data.data {
            // Only cache active instruments
            if info.state == "live" {
                let metadata = InstrumentMetadata::from_okx_info(&info)?;
                log::debug!(
                    "Cached metadata for {}: lot_size={}, tick_size={}, multiplier={:?}",
                    metadata.inst_id,
                    metadata.lot_size,
                    metadata.tick_size,
                    metadata.contract_multiplier
                );
                cache.insert(info.inst_id.clone(), metadata);
            }
        }

        log::info!(
            "Loaded {} active {} instruments into cache",
            cache.len(),
            inst_type
        );

        Ok(Self { cache })
    }

    /// Get metadata for an instrument (fails fast if not found)
    pub fn get_metadata(&self, inst_id: &str) -> Result<&InstrumentMetadata> {
        self.cache.get(inst_id).ok_or_else(|| {
            AppError::parse(format!("Instrument metadata not found for {}", inst_id))
        })
    }

    /// Check if instrument exists in cache
    pub fn contains(&self, inst_id: &str) -> bool {
        self.cache.contains_key(inst_id)
    }

    /// Get number of cached instruments
    pub fn len(&self) -> usize {
        self.cache.len()
    }
}

impl InstrumentMetadata {
    /// Convert from OkxInstrumentInfo to processed metadata
    pub fn from_okx_info(info: &OkxInstrumentInfo) -> Result<Self> {
        let lot_size = fast_float::parse::<f64, _>(&info.lot_size)
            .map_err(|e| AppError::parse(format!("Invalid lot size '{}': {}", info.lot_size, e)))?;

        let tick_size = fast_float::parse::<f64, _>(&info.tick_size).map_err(|e| {
            AppError::parse(format!("Invalid tick size '{}': {}", info.tick_size, e))
        })?;

        // Parse contract multiplier (ctMult) - always present
        let contract_multiplier = if let Some(ref mult_str) = info.contract_multiplier {
            Some(fast_float::parse::<f64, _>(mult_str).map_err(|e| {
                AppError::parse(format!("Invalid contract multiplier '{}': {}", mult_str, e))
            })?)
        } else {
            None
        };

        // Parse contract value (ctVal) - present for derivatives like SWAP
        let contract_value = if let Some(ref val_str) = info.contract_value {
            Some(fast_float::parse::<f64, _>(val_str).map_err(|e| {
                AppError::parse(format!("Invalid contract value '{}': {}", val_str, e))
            })?)
        } else {
            None
        };

        Ok(InstrumentMetadata {
            inst_id: info.inst_id.clone(),
            inst_type: info.inst_type.clone(),
            lot_size,
            contract_multiplier,
            contract_value,
            tick_size,
            base_currency: info.base_currency.clone(),
            quote_currency: info.quote_currency.clone(),
            last_updated: std::time::Instant::now(),
        })
    }

    /// Normalize quantity using correct OKX formula: normal_qty = contractValue * contractMultiplier * qty
    pub fn normalize_quantity(&self, raw_qty: f64) -> f64 {
        let multiplier = self.contract_multiplier.unwrap_or(1.0);
        let value = self.contract_value.unwrap_or(1.0);
        raw_qty * value * multiplier
    }

    /// Get contract multiplier (1.0 if not present)
    pub fn get_contract_multiplier(&self) -> f64 {
        self.contract_multiplier.unwrap_or(1.0)
    }
}

#[derive(Debug, Clone)]
pub enum OkxSyncState {
    Disconnected,
    Connected,
    Subscribed,
    Synced,
    Error(String),
}

pub struct OkxConnector {
    client: Client,
    ws_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    exchange_id: ExchangeId,
    sync_state: OkxSyncState,
    base_url: String,
    ws_url: String,
    buffer: VecDeque<OkxDepthUpdate>,
    instrument_registry: std::sync::Arc<InstrumentRegistry>,
}

impl OkxConnector {
    pub fn new_swap(instrument_registry: Arc<InstrumentRegistry>) -> Self {
        Self {
            client: Client::new(),
            ws_stream: None,
            exchange_id: ExchangeId::OkxSwap,
            sync_state: OkxSyncState::Disconnected,
            base_url: "https://www.okx.com".to_string(),
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            buffer: VecDeque::new(),
            instrument_registry,
        }
    }

    pub fn new_spot(instrument_registry: Arc<InstrumentRegistry>) -> Self {
        Self {
            client: Client::new(),
            ws_stream: None,
            exchange_id: ExchangeId::OkxSpot,
            sync_state: OkxSyncState::Disconnected,
            base_url: "https://www.okx.com".to_string(),
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            buffer: VecDeque::new(),
            instrument_registry,
        }
    }

    fn get_inst_type(&self) -> &str {
        match self.exchange_id {
            ExchangeId::OkxSwap => "SWAP",
            ExchangeId::OkxSpot => "SPOT",
            _ => panic!("Invalid exchange ID for OKX connector"),
        }
    }

    fn convert_symbol(&self, symbol: &str) -> String {
        // Convert standard format (BTCUSDT) to OKX format
        match self.exchange_id {
            ExchangeId::OkxSwap => {
                // For SWAP: BTC-USDT-SWAP
                if symbol.ends_with("USDT") {
                    let base = &symbol[..symbol.len() - 4];
                    format!("{}-USDT-SWAP", base)
                } else if symbol.ends_with("USD") {
                    let base = &symbol[..symbol.len() - 3];
                    format!("{}-USD-SWAP", base)
                } else {
                    symbol.to_string()
                }
            }
            ExchangeId::OkxSpot => {
                // For SPOT: BTC-USDT
                if symbol.ends_with("USDT") {
                    let base = &symbol[..symbol.len() - 4];
                    format!("{}-USDT", base)
                } else if symbol.ends_with("USD") {
                    let base = &symbol[..symbol.len() - 3];
                    format!("{}-USD", base)
                } else {
                    symbol.to_string()
                }
            }
            _ => symbol.to_string(),
        }
    }

    fn parse_host_port(url: &str, default_port: u16) -> Option<(String, u16)> {
        let without_scheme = url.strip_prefix("wss://").or_else(|| url.strip_prefix("ws://"))?;
        let parts: Vec<&str> = without_scheme.splitn(2, '/').collect();
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
                    log::info!("[dns][okx] host={} port={} resolved={:?}", host, port, addrs_vec);
                }
                Err(e) => {
                    log::warn!("[dns][okx] host={} port={} resolution failed: {}", host, port, e);
                }
            }
        } else {
            log::warn!("[dns][okx] failed to parse host from URL: {}", url);
        }
    }

    fn log_connected_addrs(stream: &WebSocketStream<MaybeTlsStream<TcpStream>>) {
        let inner = stream.get_ref();
        match inner {
            MaybeTlsStream::Plain(tcp) => {
                if let (Ok(local), Ok(peer)) = (tcp.local_addr(), tcp.peer_addr()) {
                    log::info!("[net][okx] connected local={} peer={}", local, peer);
                }
            }
            MaybeTlsStream::Rustls(tls) => {
                let (io, _session) = tls.get_ref();
                if let (Ok(local), Ok(peer)) = (io.local_addr(), io.peer_addr()) {
                    log::info!("[net][okx] connected local={} peer={}", local, peer);
                }
            }
            #[cfg(any())]
            MaybeTlsStream::NativeTls(tls) => {
                if let Some(io) = tls.get_ref() {
                    if let (Ok(local), Ok(peer)) = (io.local_addr(), io.peer_addr()) {
                        log::info!("[net][okx] connected local={} peer={}", local, peer);
                    }
                }
            }
            _ => {
                log::info!("[net][okx] connected (unrecognized TLS stream variant)");
            }
        }
    }

    async fn get_depth_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot> {
        let okx_symbol = self.convert_symbol(symbol);
        let url = format!(
            "{}/api/v5/market/books?instId={}&sz=400",
            self.base_url, okx_symbol
        );

        let response =
            self.client.get(&url).send().await.map_err(|e| {
                AppError::connection(format!("Failed to get depth snapshot: {}", e))
            })?;

        if !response.status().is_success() {
            return Err(AppError::connection(format!(
                "HTTP error getting depth: {}",
                response.status()
            )));
        }

        #[derive(Deserialize)]
        struct DepthResponse {
            code: String,
            msg: String,
            data: Vec<OkxDepthData>,
        }

        let data: DepthResponse = response
            .json()
            .await
            .map_err(|e| AppError::parse(format!("Failed to parse depth response: {}", e)))?;

        if data.code != "0" {
            return Err(AppError::connection(format!(
                "OKX API error getting depth: {} - {}",
                data.code, data.msg
            )));
        }

        let depth_data = data
            .data
            .into_iter()
            .next()
            .ok_or_else(|| AppError::parse("No depth data in response".to_string()))?;

        // Get instrument metadata for unit conversion
        let metadata = self.instrument_registry.get_metadata(&okx_symbol)?;

        let mut bids = Vec::new();
        let mut asks = Vec::new();

        // Parse bids with unit conversion
        for bid_entry in depth_data.bids {
            if bid_entry.len() >= 2 {
                let price = fast_float::parse::<f64, _>(&bid_entry[0]).map_err(|e| {
                    AppError::parse(format!("Invalid bid price '{}': {}", bid_entry[0], e))
                })?;
                let raw_size = fast_float::parse::<f64, _>(&bid_entry[1]).map_err(|e| {
                    AppError::parse(format!("Invalid bid size '{}': {}", bid_entry[1], e))
                })?;

                // Apply unit conversion using contract multiplier
                let normalized_size = metadata.normalize_quantity(raw_size);

                bids.push(PriceLevel {
                    price,
                    qty: normalized_size,
                });
            }
        }

        // Parse asks with unit conversion
        for ask_entry in depth_data.asks {
            if ask_entry.len() >= 2 {
                let price = fast_float::parse::<f64, _>(&ask_entry[0]).map_err(|e| {
                    AppError::parse(format!("Invalid ask price '{}': {}", ask_entry[0], e))
                })?;
                let raw_size = fast_float::parse::<f64, _>(&ask_entry[1]).map_err(|e| {
                    AppError::parse(format!("Invalid ask size '{}': {}", ask_entry[1], e))
                })?;

                // Apply unit conversion using contract multiplier
                let normalized_size = metadata.normalize_quantity(raw_size);

                asks.push(PriceLevel {
                    price,
                    qty: normalized_size,
                });
            }
        }

        let timestamp = depth_data
            .timestamp
            .parse::<i64>()
            .map_err(|e| AppError::parse(format!("Invalid timestamp: {}", e)))?;

        Ok(OrderBookSnapshot {
            exchange_id: self.exchange_id,
            symbol: symbol.to_string(),
            last_update_id: depth_data.seq_id.unwrap_or(0),
            timestamp,
            sequence: depth_data.seq_id.unwrap_or(0),
            bids,
            asks,
        })
    }

    fn parse_depth_update(
        &self,
        update: &OkxDepthUpdate,
        symbol: &str,
        packet_id: u64,
        rcv_timestamp: i64,
    ) -> Result<Vec<OrderBookL2Update>> {
        let mut updates = Vec::new();

        // Get OKX symbol format for metadata lookup
        let okx_symbol = self.convert_symbol(symbol);
        let metadata = self.instrument_registry.get_metadata(&okx_symbol)?;

        for data in &update.data {
            let timestamp = data.timestamp.parse::<i64>().map_err(|e| {
                AppError::parse(format!("Invalid timestamp '{}': {}", data.timestamp, e))
            })?;

            let timestamp_micros = crate::xcommons::types::time::millis_to_micros(timestamp);
            let seq_id = data.seq_id.unwrap_or(0);
            let first_update_id = data.prev_seq_id.unwrap_or(seq_id);

            // Process bids with unit conversion
            for bid_entry in &data.bids {
                if bid_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&bid_entry[0]).map_err(|e| {
                        AppError::parse(format!("Invalid bid price '{}': {}", bid_entry[0], e))
                    })?;
                    let raw_size = fast_float::parse::<f64, _>(&bid_entry[1]).map_err(|e| {
                        AppError::parse(format!("Invalid bid size '{}': {}", bid_entry[1], e))
                    })?;

                    // Apply unit conversion using contract multiplier
                    let normalized_size = metadata.normalize_quantity(raw_size);

                    let builder = OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        self.exchange_id,
                        symbol.to_string(),
                        seq_id,
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );

                    updates.push(builder.build_bid(price, normalized_size));
                }
            }

            // Process asks with unit conversion
            for ask_entry in &data.asks {
                if ask_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&ask_entry[0]).map_err(|e| {
                        AppError::parse(format!("Invalid ask price '{}': {}", ask_entry[0], e))
                    })?;
                    let raw_size = fast_float::parse::<f64, _>(&ask_entry[1]).map_err(|e| {
                        AppError::parse(format!("Invalid ask size '{}': {}", ask_entry[1], e))
                    })?;

                    // Apply unit conversion using contract multiplier
                    let normalized_size = metadata.normalize_quantity(raw_size);

                    let builder = OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        self.exchange_id,
                        symbol.to_string(),
                        seq_id,
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );

                    updates.push(builder.build_ask(price, normalized_size));
                }
            }
        }

        Ok(updates)
    }

    /// Parse trade update from OKX WebSocket message
    fn parse_trade_update(&self, text: &str) -> Result<OkxTradeUpdate> {
        serde_json::from_str(text)
            .map_err(|e| AppError::parse(format!("Failed to parse OKX trade update: {}", e)))
    }

    /// Convert OKX trade update to internal TradeUpdate format
    fn convert_trade_update(&self, trade_data: &OkxTradeData, rcv_timestamp: i64, packet_id: i64) -> Result<TradeUpdate> {
        let price = fast_float::parse::<f64, _>(&trade_data.price)
            .map_err(|e| AppError::parse(format!("Invalid trade price '{}': {}", trade_data.price, e)))?;
        
        let raw_size = fast_float::parse::<f64, _>(&trade_data.size)
            .map_err(|e| AppError::parse(format!("Invalid trade size '{}': {}", trade_data.size, e)))?;

        // Get instrument metadata for unit conversion
        let metadata = self.instrument_registry.get_metadata(&trade_data.inst_id)?;
        let qty = metadata.normalize_quantity(raw_size);

        let trade_side = match trade_data.side.as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            _ => return Err(AppError::parse(format!("Invalid trade side: {}", trade_data.side))),
        };

        let timestamp = trade_data.timestamp.parse::<i64>()
            .map_err(|e| AppError::parse(format!("Invalid timestamp '{}': {}", trade_data.timestamp, e)))?;

        // Convert symbol back to standard format (BTC-USDT-SWAP -> BTCUSDT)
        let ticker = self.convert_symbol_from_okx(&trade_data.inst_id);

        Ok(TradeUpdate {
            timestamp: crate::xcommons::types::time::millis_to_micros(timestamp),
            rcv_timestamp,
            exchange: self.exchange_id,
            ticker,
            seq_id: trade_data.trade_id.parse().unwrap_or(0),
            packet_id,
            trade_id: trade_data.trade_id.clone(),
            order_id: None, // OKX doesn't provide order ID in trade stream
            side: trade_side,
            price,
            qty,
            // is_buyer_maker removed; side encodes aggressor
        })
    }

    /// Convert OKX symbol format back to standard format
    fn convert_symbol_from_okx(&self, okx_symbol: &str) -> String {
        match self.exchange_id {
            ExchangeId::OkxSwap => {
                // Convert BTC-USDT-SWAP -> BTCUSDT
                if let Some(base_part) = okx_symbol.strip_suffix("-SWAP") {
                    base_part.replace('-', "")
                } else {
                    okx_symbol.replace('-', "")
                }
            }
            ExchangeId::OkxSpot => {
                // Convert BTC-USDT -> BTCUSDT
                okx_symbol.replace('-', "")
            }
            _ => okx_symbol.to_string(),
        }
    }
}

#[async_trait]
impl ExchangeConnector for OkxConnector {
    type Error = AppError;

    async fn connect(&mut self) -> Result<()> {
        self.log_dns_and_target(&self.ws_url, 8443).await;
        let (ws_stream, _) = connect_async(&self.ws_url).await.map_err(|e| {
            AppError::connection(format!("Failed to connect to OKX WebSocket: {}", e))
        })?;

        self.ws_stream = Some(ws_stream);
        if let Some(ref stream) = self.ws_stream { Self::log_connected_addrs(stream); }
        self.sync_state = OkxSyncState::Connected;

        Ok(())
    }

    async fn get_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot> {
        self.get_depth_snapshot(symbol)
            .await
            .map_err(|e| AppError::connection(format!("Failed to get OKX snapshot: {}", e)))
    }

    async fn subscribe_l2(&mut self, symbol: &str) -> Result<()> {
        let okx_symbol = self.convert_symbol(symbol);
        let ws_stream = self
            .ws_stream
            .as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        // Subscribe to books5 channel (5 depth levels with 100ms updates)
        // For more granular data, use "books-l2-tbt" (requires VIP5) or "books50-l2-tbt" (requires VIP4)
        let subscribe_msg = serde_json::json!({
            "op": "subscribe",
            "args": [{
                "channel": "books5",
                "instId": okx_symbol
            }]
        });

        let message = Message::Text(subscribe_msg.to_string());
        ws_stream
            .send(message)
            .await
            .map_err(|e| AppError::connection(format!("Failed to send subscription: {}", e)))?;

        self.sync_state = OkxSyncState::Subscribed;
        Ok(())
    }

    async fn subscribe_trades(&mut self, symbol: &str) -> Result<()> {
        let okx_symbol = self.convert_symbol(symbol);
        let ws_stream = self
            .ws_stream
            .as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        // Subscribe to trades channel for actual trade data
        let subscribe_msg = serde_json::json!({
            "op": "subscribe",
            "args": [{
                "channel": "trades",
                "instId": okx_symbol
            }]
        });

        let message = Message::Text(subscribe_msg.to_string());
        ws_stream
            .send(message)
            .await
            .map_err(|e| AppError::connection(format!("Failed to send trades subscription: {}", e)))?;

        self.sync_state = OkxSyncState::Subscribed;
        log::info!("OKX trades subscription sent for {}", okx_symbol);
        Ok(())
    }

    async fn next_message(&mut self) -> Result<Option<RawMessage>> {
        let ws_stream = self
            .ws_stream
            .as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => Ok(Some(RawMessage {
                exchange_id: self.exchange_id,
                data: text.into_bytes(),
                timestamp: SystemTime::now(),
            })),
            Some(Ok(Message::Close(_))) => {
                self.sync_state = OkxSyncState::Disconnected;
                Ok(None)
            }
            Some(Ok(_)) => Ok(None), // Ignore other message types
            Some(Err(e)) => {
                self.sync_state = OkxSyncState::Error(e.to_string());
                Err(AppError::connection(format!("WebSocket error: {}", e)))
            }
            None => {
                self.sync_state = OkxSyncState::Disconnected;
                Ok(None)
            }
        }
    }

    fn exchange_id(&self) -> ExchangeId {
        self.exchange_id
    }

    fn connection_status(&self) -> ConnectionStatus {
        match self.sync_state {
            OkxSyncState::Disconnected => ConnectionStatus::Disconnected,
            OkxSyncState::Connected | OkxSyncState::Subscribed | OkxSyncState::Synced => {
                ConnectionStatus::Connected
            }
            OkxSyncState::Error(_) => ConnectionStatus::Error,
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut ws_stream) = self.ws_stream.take() {
            let _ = ws_stream.close(None).await;
        }
        self.sync_state = OkxSyncState::Disconnected;
        Ok(())
    }

    fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.ws_stream.as_ref().and_then(|ws| {
            match ws.get_ref() {
                MaybeTlsStream::Plain(tcp) => tcp.peer_addr().ok(),
                MaybeTlsStream::Rustls(tls) => {
                    let (io, _) = tls.get_ref();
                    io.peer_addr().ok()
                }
                #[cfg(feature = "native-tls")]
                MaybeTlsStream::NativeTls(tls) => tls.get_ref().and_then(|io| io.peer_addr().ok()),
                _ => None,
            }
        })
    }

    async fn validate_symbol(&self, symbol: &str) -> Result<()> {
        let okx_symbol = self.convert_symbol(symbol);
        let found = self.instrument_registry.contains(&okx_symbol);

        if !found {
            return Err(AppError::validation(format!(
                "Symbol {} not found or not active on OKX",
                symbol
            )));
        }

        Ok(())
    }

    async fn start_depth_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<()> {
        self.connect().await?;
        self.subscribe_l2(symbol).await?;

        let ws_stream = self
            .ws_stream
            .as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        while let Some(message_result) = ws_stream.next().await {
            match message_result {
                Ok(Message::Text(text)) => {
                    let raw_message = RawMessage {
                        exchange_id: self.exchange_id,
                        data: text.into_bytes(),
                        timestamp: SystemTime::now(),
                    };

                    if tx.send(raw_message).await.is_err() {
                        break; // Channel closed
                    }
                }
                Ok(Message::Close(_)) => {
                    break;
                }
                Ok(_) => continue, // Ignore other message types
                Err(e) => {
                    return Err(AppError::connection(format!("WebSocket error: {}", e)));
                }
            }
        }

        Ok(())
    }

    async fn start_trade_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<()> {
        self.connect().await?;
        
        let okx_symbol = self.convert_symbol(symbol);
        let ws_stream = self
            .ws_stream
            .as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        // Subscribe to trades channel
        let subscribe_msg = serde_json::json!({
            "op": "subscribe",
            "args": [{
                "channel": "trades",
                "instId": okx_symbol
            }]
        });

        let message = Message::Text(subscribe_msg.to_string());
        ws_stream
            .send(message)
            .await
            .map_err(|e| AppError::connection(format!("Failed to send trade subscription: {}", e)))?;

        log::info!("OKX trade stream subscribed for {}", symbol);

        // Message forwarding loop
        while let Some(message_result) = ws_stream.next().await {
            match message_result {
                Ok(Message::Text(text)) => {
                    let raw_message = RawMessage {
                        exchange_id: self.exchange_id,
                        data: text.into_bytes(),
                        timestamp: SystemTime::now(),
                    };

                    if tx.send(raw_message).await.is_err() {
                        log::info!("OKX trade stream channel closed, stopping");
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    log::warn!("OKX trade WebSocket connection closed");
                    break;
                }
                Ok(_) => continue, // Ignore other message types
                Err(e) => {
                    log::error!("OKX trade WebSocket error: {}", e);
                    return Err(AppError::connection(format!("WebSocket error: {}", e)));
                }
            }
        }

        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn prepare_l2_sync(&mut self, _snapshot: &OrderBookSnapshot) -> Result<()> {
        // OKX provides prevSeqId/seqId; we start in Subscribed state and allow normal processing.
        // For checksum-capable channels (books-l2-tbt), checksum verification should be added here.
        Ok(())
    }
}

/// Parse OKX raw message into L2 updates
pub fn parse_okx_message(
    raw_message: &RawMessage,
    symbol: &str,
    packet_id: u64,
    rcv_timestamp: i64,
) -> Result<Vec<OrderBookL2Update>> {
        let message: OkxMessage = serde_json::from_slice(&raw_message.data)
        .map_err(|e| AppError::parse(format!("Failed to parse OKX message: {}", e)))?;

    match message {
        OkxMessage::Data(update) => {
            // Create temporary connector to use parsing logic
            // Create empty registry for parsing (tests don't need actual metadata)
            let registry = Arc::new(InstrumentRegistry::new());
            let connector = if raw_message.exchange_id == ExchangeId::OkxSwap {
                OkxConnector::new_swap(registry)
            } else {
                OkxConnector::new_spot(registry)
            };
            connector.parse_depth_update(&update, symbol, packet_id, rcv_timestamp)
        }
        OkxMessage::Subscription { event: _, arg: _ } => {
            // Subscription acknowledgment - no data to process
            Ok(Vec::new())
        }
        OkxMessage::Error {
            event: _,
            code,
            msg,
        } => Err(AppError::parse(format!(
            "OKX WebSocket error: {} - {}",
            code, msg
        ))),
    }
}

pub fn create_okx_swap_connector() -> OkxConnector {
    let registry = Arc::new(InstrumentRegistry::new());
    OkxConnector::new_swap(registry)
}

pub fn create_okx_spot_connector() -> OkxConnector {
    let registry = Arc::new(InstrumentRegistry::new());
    OkxConnector::new_spot(registry)
}

/// OKX-specific message processor with performance optimizations
pub struct OkxProcessor {
    base: crate::exchanges::BaseProcessor,
    okx_registry: Option<std::sync::Arc<InstrumentRegistry>>,
    exchange_id: crate::xcommons::types::ExchangeId,
    // Performance optimizations with bounded caches
    symbol_cache: LruCache<String, String>, // Bounded cache for symbol conversions
    metadata_cache: LruCache<String, InstrumentMetadata>, // Bounded cache for metadata lookups
    string_buffer: String, // Pre-allocated buffer for string operations
    last_seq_id: Option<i64>,
}

impl OkxProcessor {
    pub fn new(
        exchange_id: crate::xcommons::types::ExchangeId,
        registry: Option<std::sync::Arc<InstrumentRegistry>>,
    ) -> Self {
        Self {
            base: crate::exchanges::BaseProcessor::new(),
            okx_registry: registry,
            exchange_id,
            // Performance optimizations with bounded caches
            symbol_cache: LruCache::new(NonZeroUsize::new(64).unwrap()), // Bounded cache for 64 symbols
            metadata_cache: LruCache::new(NonZeroUsize::new(32).unwrap()), // Bounded cache for 32 metadata entries
            string_buffer: String::with_capacity(32), // Pre-allocate buffer for string operations
            last_seq_id: None,
        }
    }

    /// Convert symbol to OKX format for metadata lookup (deprecated - use owned version)
    fn convert_symbol_to_okx_format(&self, symbol: &str) -> String {
        self.convert_symbol_to_okx_format_owned(symbol)
    }

    /// Get metadata with caching to avoid repeated registry lookups
    fn get_cached_metadata_by_owned_symbol(&mut self, okx_symbol: &str) -> Option<InstrumentMetadata> {
        // Check if we already have it cached
        if let Some(cached) = self.metadata_cache.get(okx_symbol) {
            return Some(cached.clone());
        }

        // Fetch from registry and cache it
        if let Some(ref registry) = self.okx_registry {
            if let Ok(metadata) = registry.get_metadata(okx_symbol) {
                let cloned_metadata = metadata.clone();
                self.metadata_cache.put(okx_symbol.to_string(), cloned_metadata.clone());
                Some(cloned_metadata)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Convert symbol to OKX format returning owned string (for avoiding borrow conflicts)
    fn convert_symbol_to_okx_format_owned(&self, symbol: &str) -> String {
        match self.exchange_id {
            crate::xcommons::types::ExchangeId::OkxSwap => {
                // For SWAP: BTC-USDT-SWAP
                if symbol.ends_with("USDT") {
                    let base = &symbol[..symbol.len() - 4];
                    format!("{}-USDT-SWAP", base)
                } else if symbol.ends_with("USD") {
                    let base = &symbol[..symbol.len() - 3];
                    format!("{}-USD-SWAP", base)
                } else {
                    symbol.to_string()
                }
            }
            crate::xcommons::types::ExchangeId::OkxSpot => {
                // For SPOT: BTC-USDT
                if symbol.ends_with("USDT") {
                    let base = &symbol[..symbol.len() - 4];
                    format!("{}-USDT", base)
                } else if symbol.ends_with("USD") {
                    let base = &symbol[..symbol.len() - 3];
                    format!("{}-USD", base)
                } else {
                    symbol.to_string()
                }
            }
            _ => symbol.to_string(),
        }
    }

    /// Convert OKX symbol format back to standard format
    fn convert_symbol_from_okx(&self, okx_symbol: &str) -> String {
        match self.exchange_id {
            crate::xcommons::types::ExchangeId::OkxSwap => {
                // Convert BTC-USDT-SWAP -> BTCUSDT
                if let Some(base_part) = okx_symbol.strip_suffix("-SWAP") {
                    base_part.replace('-', "")
                } else {
                    okx_symbol.replace('-', "")
                }
            }
            crate::xcommons::types::ExchangeId::OkxSpot => {
                // Convert BTC-USDT -> BTCUSDT
                okx_symbol.replace('-', "")
            }
            _ => okx_symbol.to_string(),
        }
    }
}

impl crate::exchanges::ExchangeProcessor for OkxProcessor {
    type Error = crate::xcommons::error::AppError;

    fn base_processor(&mut self) -> &mut crate::exchanges::BaseProcessor {
        &mut self.base
    }

    fn base_processor_ref(&self) -> &crate::exchanges::BaseProcessor {
        &self.base
    }

    fn process_message(
        &mut self,
        raw_msg: crate::xcommons::types::RawMessage,
        symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> std::result::Result<Vec<crate::xcommons::types::OrderBookL2Update>, Self::Error> {
        // Track message received
        self.base.metrics.increment_received();

        // Calculate packet arrival timestamp (when network packet was received)
        // Use socket receive time from RawMessage for code latency measurement
        let packet_arrival_us = raw_msg
            .timestamp
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        // Parse OKX message JSON
        let okx_message: OkxMessage = serde_json::from_slice(&raw_msg.data).map_err(|_e| {
            self.base.metrics.increment_parse_errors();
            crate::xcommons::error::AppError::pipeline(ERROR_JSON_PARSE.to_string())
        })?;

        let depth_update = match okx_message {
            OkxMessage::Data(depth_update) => depth_update,
            OkxMessage::Subscription { .. } => {
                // Subscription acknowledgment - no data to process
                return Ok(Vec::new());
            }
            OkxMessage::Error { .. } => {
                self.base.metrics.increment_parse_errors();
                return Err(crate::xcommons::error::AppError::pipeline(
                    "OKX error message received".to_string(),
                ));
            }
        };

        // Reuse pre-allocated buffer
        self.base.updates_buffer.clear();

        // Start transformation timing
        let transform_start = crate::xcommons::types::time::now_micros();

        // Get OKX symbol format for metadata lookup once (avoid borrowing issues)
        let okx_symbol = self.convert_symbol_to_okx_format_owned(symbol);

        // Track total bid/ask count across all data entries
        let mut total_bid_count = 0u32;
        let mut total_ask_count = 0u32;

        for data in &depth_update.data {
            let timestamp = data.timestamp.parse::<i64>().map_err(|e| {
                crate::xcommons::error::AppError::parse(format!(
                    "Invalid timestamp '{}': {}",
                    data.timestamp, e
                ))
            })?;

            let timestamp_micros = crate::xcommons::types::time::millis_to_micros(timestamp);

            // Calculate overall latency (local time vs exchange timestamp)
            if let Some(overall_latency_us) = rcv_timestamp.checked_sub(timestamp_micros) {
                if overall_latency_us >= 0 {
                    // Record metric
                    self.base
                        .metrics
                        .update_overall_latency(overall_latency_us as u64);

                    // Debug: flag suspiciously tiny latencies for investigation
                    if overall_latency_us < 1_000 {
                        log::debug!(
                            "[okx][lat] tiny overall latency: {}us (rcv={}, exch_ts={})",
                            overall_latency_us, rcv_timestamp, timestamp_micros
                        );
                    }
                } else {
                    // Debug: negative means local clock ahead of exchange clock (or unit mismatch)
                    log::debug!(
                        "[okx][lat] negative overall latency: {}us (rcv={}, exch_ts={})",
                        overall_latency_us, rcv_timestamp, timestamp_micros
                    );
                }
            }

            let seq_id = data.seq_id.unwrap_or(0);
            let first_update_id = data.prev_seq_id.unwrap_or(seq_id);

            // Minimal continuity check to detect gaps quickly
            if let Some(prev) = self.last_seq_id {
                if let Some(prev_seq) = data.prev_seq_id {
                    if prev_seq != prev {
                        return Err(crate::xcommons::error::AppError::pipeline(
                            format!("OKX sequence gap: prev_seq_id={} last_seq_id={}", prev_seq, prev),
                        ));
                    }
                }
            }
            self.last_seq_id = Some(seq_id);

            // Count bid/ask levels for complexity metrics
            total_bid_count += data.bids.len() as u32;
            total_ask_count += data.asks.len() as u32;

            // Process bids with unit conversion
            for bid_entry in &data.bids {
                if bid_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&bid_entry[0]).map_err(|_e| {
                        crate::xcommons::error::AppError::pipeline(ERROR_PRICE_PARSE.to_string())
                    })?;
                    let raw_size = fast_float::parse::<f64, _>(&bid_entry[1]).map_err(|_e| {
                        crate::xcommons::error::AppError::pipeline(ERROR_QUANTITY_PARSE.to_string())
                    })?;

                    // Apply unit conversion if metadata available
                    let size = if let Some(ref _registry) = self.okx_registry {
                        if let Some(metadata) = self.get_cached_metadata_by_owned_symbol(&okx_symbol) {
                            let converted = metadata.normalize_quantity(raw_size);
                            // log::debug!(
                            //     "Unit conversion: {} raw={} -> converted={} (multiplier={:?})",
                            //     okx_symbol,
                            //     raw_size,
                            //     converted,
                            //     metadata.contract_multiplier
                            // );
                            converted
                        } else {
                            log::warn!("No metadata found for symbol: {}", okx_symbol);
                            raw_size
                        }
                    } else {
                        log::warn!("No OKX registry available for unit conversion");
                        raw_size
                    };

                    let sequence_id = self.next_sequence_id();
                    let builder = crate::xcommons::types::OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        self.exchange_id,
                        symbol.to_string(),
                        sequence_id,
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );

                    self.base
                        .updates_buffer
                        .push(builder.build_bid(price, size));
                }
            }

            // Process asks with unit conversion
            for ask_entry in &data.asks {
                if ask_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&ask_entry[0]).map_err(|_e| {
                        crate::xcommons::error::AppError::pipeline(ERROR_PRICE_PARSE.to_string())
                    })?;
                    let raw_size = fast_float::parse::<f64, _>(&ask_entry[1]).map_err(|_e| {
                        crate::xcommons::error::AppError::pipeline(ERROR_QUANTITY_PARSE.to_string())
                    })?;

                    // Apply unit conversion if metadata available
                    let size = if let Some(ref _registry) = self.okx_registry {
                        if let Some(metadata) = self.get_cached_metadata_by_owned_symbol(&okx_symbol) {
                            let converted = metadata.normalize_quantity(raw_size);
                            // log::debug!(
                            //     "Unit conversion: {} raw={} -> converted={} (multiplier={:?})",
                            //     okx_symbol,
                            //     raw_size,
                            //     converted,
                            //     metadata.contract_multiplier
                            // );
                            converted
                        } else {
                            log::warn!("No metadata found for symbol: {}", okx_symbol);
                            raw_size
                        }
                    } else {
                        log::warn!("No OKX registry available for unit conversion");
                        raw_size
                    };

                    let sequence_id = self.next_sequence_id();
                    let builder = crate::xcommons::types::OrderBookL2UpdateBuilder::new(
                        timestamp_micros,
                        rcv_timestamp,
                        self.exchange_id,
                        symbol.to_string(),
                        sequence_id,
                        packet_id as i64,
                        seq_id,
                        first_update_id,
                    );

                    self.base
                        .updates_buffer
                        .push(builder.build_ask(price, size));
                }
            }
        }

        // Calculate transformation time
        let transform_end = crate::xcommons::types::time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.base
                .metrics
                .update_transform_time(transform_time as u64);
        }

        // Update metrics
        self.base.metrics.increment_processed();
        self.base.metrics.update_message_complexity(
            total_bid_count,
            total_ask_count,
            message_bytes,
        );

        // Update packet metrics (OKX can send multiple messages in one packet)
        let messages_in_packet = depth_update.data.len() as u32;
        let entries_in_packet = total_bid_count + total_ask_count;
        self.base.metrics.update_packet_metrics(
            messages_in_packet,
            entries_in_packet,
            message_bytes,
        );

        // Calculate CODE LATENCY: From network packet arrival to business logic ready
        // This measures the FULL time spent in Rust code processing
        let processing_complete = crate::xcommons::types::time::now_micros();
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
        raw_msg: crate::xcommons::types::RawMessage,
        _symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> std::result::Result<Vec<crate::xcommons::types::TradeUpdate>, Self::Error> {
        // Track message received
        self.base.metrics.increment_received();

        // Calculate packet arrival timestamp (when network packet was received)
        // Use socket receive time from RawMessage for code latency measurement
        let packet_arrival_us = raw_msg
            .timestamp
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        // Parse OKX message with timing
        let parse_start = crate::xcommons::types::time::now_micros();
        let mut data_bytes = raw_msg.data;
        
        // First try to parse as generic OKX message to handle different types
        let okx_message: serde_json::Value = serde_json::from_slice(&mut data_bytes).map_err(|e| {
            self.base.metrics.increment_parse_errors();
            crate::xcommons::error::AppError::pipeline(format!("Failed to parse OKX message: {}", e))
        })?;

        // Check if this is a subscription confirmation or error message
        if let Some(event) = okx_message.get("event") {
            match event.as_str() {
                Some("subscribe") => {
                    log::info!("OKX trades subscription confirmed");
                    return Ok(vec![]); // No trades to process
                }
                Some("error") => {
                    let code = okx_message.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                    let msg = okx_message.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown error");
                    log::error!("OKX subscription error: {} - {}", code, msg);
                    return Ok(vec![]); // No trades to process
                }
                _ => {
                    log::warn!("Unknown OKX event type: {}", event);
                    return Ok(vec![]);
                }
            }
        }

        // If no event field, this should be a data message - parse as trade update
        let trade_message: OkxTradeUpdate = serde_json::from_value(okx_message).map_err(|e| {
            self.base.metrics.increment_parse_errors();
            crate::xcommons::error::AppError::pipeline(format!("Failed to parse OKX trade data: {}", e))
        })?;

        // Calculate parse time
        let parse_end = crate::xcommons::types::time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.base.metrics.update_parse_time(parse_time as u64);
        }

        // Start transformation timing
        let transform_start = crate::xcommons::types::time::now_micros();

        // Calculate overhead time (time between parse end and transform start)
        if let Some(overhead_time) = transform_start.checked_sub(parse_end) {
            self.base.metrics.update_overhead_time(overhead_time as u64);
        }

        let mut trades = Vec::new();

        // Process each trade in the data array
        for trade_data in trade_message.data {
            // Parse price and size using fast_float for performance
            let price = fast_float::parse::<f64, _>(&trade_data.price).map_err(|e| {
                crate::xcommons::error::AppError::pipeline(format!("Invalid trade price '{}': {}", trade_data.price, e))
            })?;

            // Handle size conversion based on exchange type
            let size = if let Some(ref _registry) = self.okx_registry {
                // Use registry for contract size conversion if available
                let size = fast_float::parse::<f64, _>(&trade_data.size).map_err(|e| {
                    crate::xcommons::error::AppError::pipeline(format!("Invalid trade size '{}': {}", trade_data.size, e))
                })?;
                // TODO: Apply contract multiplier if needed
                size
            } else {
                fast_float::parse::<f64, _>(&trade_data.size).map_err(|e| {
                    crate::xcommons::error::AppError::pipeline(format!("Invalid trade size '{}': {}", trade_data.size, e))
                })?
            };

            // Parse trade side
            let trade_side = match trade_data.side.as_str() {
                "buy" => Side::Buy,
                "sell" => Side::Sell,
                _ => {
                    log::warn!("Unknown OKX trade side: {}", trade_data.side);
                    continue; // Skip this trade
                }
            };

            // Parse timestamp (OKX provides milliseconds since epoch)
            let timestamp_ms = trade_data.timestamp.parse::<i64>().map_err(|e| {
                crate::xcommons::error::AppError::pipeline(format!("Invalid timestamp '{}': {}", trade_data.timestamp, e))
            })?;
            let exchange_ts_us = crate::xcommons::types::time::millis_to_micros(timestamp_ms);

            // Calculate overall latency (local time vs exchange timestamp) for trades as well
            if let Some(overall_latency_us) = rcv_timestamp.checked_sub(exchange_ts_us) {
                if overall_latency_us >= 0 {
                    self.base.metrics.update_overall_latency(overall_latency_us as u64);
                    if overall_latency_us < 1_000 {
                        log::debug!(
                            "[okx][lat][trades] tiny overall latency: {}us (rcv={}, exch_ts={})",
                            overall_latency_us, rcv_timestamp, exchange_ts_us
                        );
                    }
                } else {
                    log::debug!(
                        "[okx][lat][trades] negative overall latency: {}us (rcv={}, exch_ts={})",
                        overall_latency_us, rcv_timestamp, exchange_ts_us
                    );
                }
            }

            let seq_id = self.next_sequence_id();
            let trade = crate::xcommons::types::TradeUpdate {
                timestamp: exchange_ts_us, 
                rcv_timestamp,
                exchange: self.exchange_id,
                ticker: self.convert_symbol_from_okx(&trade_data.inst_id),
                seq_id,
                packet_id: packet_id as i64,
                trade_id: trade_data.trade_id,
                order_id: None, // OKX doesn't provide order IDs in trade stream
                side: trade_side,
                price,
                qty: size,
                // is_buyer_maker removed
            };

            trades.push(trade);
        }

        // Calculate transformation time
        let transform_end = crate::xcommons::types::time::now_micros();
        if let Some(transform_time) = transform_end.checked_sub(transform_start) {
            self.base.metrics.update_transform_time(transform_time as u64);
        }

        // Update metrics
        self.base.metrics.increment_processed();
        self.base.metrics.update_message_complexity(0, 0, message_bytes); // Trade message has no bid/ask levels

        // Update packet metrics 
        let trades_count = trades.len() as u32;
        self.base.metrics.update_packet_metrics(1, trades_count, message_bytes);

        // Calculate CODE LATENCY: From network packet arrival to business logic ready
        let processing_complete = crate::xcommons::types::time::now_micros();
        if let Some(code_latency) = processing_complete.checked_sub(packet_arrival_us) {
            if code_latency >= 0 {
                self.base.metrics.update_code_latency(code_latency as u64);
            }
        }

        Ok(trades)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_conversion_swap() {
        let registry = Arc::new(InstrumentRegistry::new());
        let connector = OkxConnector::new_swap(registry);
        assert_eq!(connector.convert_symbol("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(connector.convert_symbol("ETHUSD"), "ETH-USD-SWAP");
    }

    #[test]
    fn test_symbol_conversion_spot() {
        let registry = Arc::new(InstrumentRegistry::new());
        let connector = OkxConnector::new_spot(registry);
        assert_eq!(connector.convert_symbol("BTCUSDT"), "BTC-USDT");
        assert_eq!(connector.convert_symbol("ETHUSD"), "ETH-USD");
    }

    #[test]
    fn test_exchange_ids() {
        let registry = Arc::new(InstrumentRegistry::new());
        let swap_connector = OkxConnector::new_swap(registry.clone());
        let spot_connector = OkxConnector::new_spot(registry);

        assert_eq!(swap_connector.exchange_id(), ExchangeId::OkxSwap);
        assert_eq!(spot_connector.exchange_id(), ExchangeId::OkxSpot);
    }

    #[test]
    fn test_instrument_metadata_creation() {
        let info = OkxInstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            inst_type: "SWAP".to_string(),
            state: "live".to_string(),
            lot_size: "1".to_string(),
            min_size: Some("1".to_string()),
            contract_multiplier: Some("0.01".to_string()),
            contract_value: Some("100".to_string()),
            tick_size: "0.1".to_string(),
            base_currency: Some("BTC".to_string()),
            quote_currency: Some("USDT".to_string()),
        };

        let metadata = InstrumentMetadata::from_okx_info(&info).unwrap();
        assert_eq!(metadata.inst_id, "BTC-USDT-SWAP");
        assert_eq!(metadata.lot_size, 1.0);
        assert_eq!(metadata.contract_multiplier, Some(0.01));
        assert_eq!(metadata.tick_size, 0.1);
    }

    #[test]
    fn test_quantity_normalization_with_multiplier() {
        let metadata = InstrumentMetadata {
            inst_id: "BTC-USDT-SWAP".to_string(),
            inst_type: "SWAP".to_string(),
            lot_size: 1.0,
            contract_multiplier: Some(0.01),
            contract_value: Some(1.0),
            tick_size: 0.1,
            base_currency: Some("BTC".to_string()),
            quote_currency: Some("USDT".to_string()),
            last_updated: std::time::Instant::now(),
        };

        // 100 contracts * 0.01 multiplier = 1.0 BTC
        assert_eq!(metadata.normalize_quantity(100.0), 1.0);
        assert_eq!(metadata.normalize_quantity(1000.0), 10.0);
    }

    #[test]
    fn test_quantity_normalization_without_multiplier() {
        let metadata = InstrumentMetadata {
            inst_id: "BTC-USDT".to_string(),
            inst_type: "SPOT".to_string(),
            lot_size: 0.001,
            contract_multiplier: None,
            contract_value: None,
            tick_size: 0.01,
            base_currency: Some("BTC".to_string()),
            quote_currency: Some("USDT".to_string()),
            last_updated: std::time::Instant::now(),
        };

        // No multiplier, should return same value
        assert_eq!(metadata.normalize_quantity(1.5), 1.5);
        assert_eq!(metadata.normalize_quantity(0.001), 0.001);
    }

    #[test]
    fn test_instrument_registry() {
        let mut registry = InstrumentRegistry::new();

        let metadata = InstrumentMetadata {
            inst_id: "BTC-USDT-SWAP".to_string(),
            inst_type: "SWAP".to_string(),
            lot_size: 1.0,
            contract_multiplier: Some(0.01),
            contract_value: Some(100.0),
            tick_size: 0.1,
            base_currency: Some("BTC".to_string()),
            quote_currency: Some("USDT".to_string()),
            last_updated: std::time::Instant::now(),
        };

        registry.cache.insert("BTC-USDT-SWAP".to_string(), metadata);

        assert!(registry.contains("BTC-USDT-SWAP"));
        assert!(!registry.contains("ETH-USDT-SWAP"));
        assert_eq!(registry.len(), 1);

        let retrieved = registry.get_metadata("BTC-USDT-SWAP").unwrap();
        assert_eq!(retrieved.contract_multiplier, Some(0.01));
    }
}
