use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::SystemTime;
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream, MaybeTlsStream};
use tokio::net::TcpStream;

use crate::error::{AppError, Result};
use fast_float;
use crate::exchanges::ExchangeConnector;
use crate::types::{
    ConnectionStatus, ExchangeId, L2Action, OrderBookL2Update, OrderBookL2UpdateBuilder,
    OrderBookSnapshot, OrderSide, PriceLevel, RawMessage
};

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
    pub action: Option<String>,  // "snapshot" or "update"
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
    pub asks: Vec<Vec<String>>,  // [price, size, liquidated_orders, orders]
    #[serde(rename = "bids")]
    pub bids: Vec<Vec<String>>,  // [price, size, liquidated_orders, orders]
    #[serde(rename = "ts")]
    pub timestamp: String,
    #[serde(rename = "checksum")]
    pub checksum: Option<i64>,
    #[serde(rename = "prevSeqId")]
    pub prev_seq_id: Option<i64>,
    #[serde(rename = "seqId")]
    pub seq_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OkxInstrumentInfo {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "instType")]
    pub inst_type: String,
    #[serde(rename = "state")]
    pub state: String,
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
}

impl OkxConnector {
    pub fn new_swap() -> Self {
        Self {
            client: Client::new(),
            ws_stream: None,
            exchange_id: ExchangeId::OkxSwap,
            sync_state: OkxSyncState::Disconnected,
            base_url: "https://www.okx.com".to_string(),
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            buffer: VecDeque::new(),
        }
    }

    pub fn new_spot() -> Self {
        Self {
            client: Client::new(),
            ws_stream: None,
            exchange_id: ExchangeId::OkxSpot,
            sync_state: OkxSyncState::Disconnected,
            base_url: "https://www.okx.com".to_string(),
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".to_string(),
            buffer: VecDeque::new(),
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

    async fn get_instruments(&self, inst_type: &str) -> Result<Vec<OkxInstrumentInfo>> {
        let url = format!("{}/api/v5/public/instruments?instType={}", self.base_url, inst_type);
        
        let response = self.client
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

        #[derive(Deserialize)]
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

        Ok(data.data)
    }

    async fn get_depth_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot> {
        let okx_symbol = self.convert_symbol(symbol);
        let url = format!("{}/api/v5/market/books?instId={}&sz=400", self.base_url, okx_symbol);
        
        let response = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::connection(format!("Failed to get depth snapshot: {}", e)))?;

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

        let depth_data = data.data.into_iter().next()
            .ok_or_else(|| AppError::parse("No depth data in response".to_string()))?;

        let mut bids = Vec::new();
        let mut asks = Vec::new();

        // Parse bids
        for bid_entry in depth_data.bids {
            if bid_entry.len() >= 2 {
                let price = fast_float::parse::<f64, _>(&bid_entry[0])
                        .map_err(|e| AppError::parse(format!("Invalid bid price '{}': {}", bid_entry[0], e)))?;
                let size = fast_float::parse::<f64, _>(&bid_entry[1])
                        .map_err(|e| AppError::parse(format!("Invalid bid size '{}': {}", bid_entry[1], e)))?;
                
                bids.push(PriceLevel { price, qty: size });
            }
        }

        // Parse asks
        for ask_entry in depth_data.asks {
            if ask_entry.len() >= 2 {
                let price = fast_float::parse::<f64, _>(&ask_entry[0])
                        .map_err(|e| AppError::parse(format!("Invalid ask price '{}': {}", ask_entry[0], e)))?;
                let size = fast_float::parse::<f64, _>(&ask_entry[1])
                        .map_err(|e| AppError::parse(format!("Invalid ask size '{}': {}", ask_entry[1], e)))?;
                
                asks.push(PriceLevel { price, qty: size });
            }
        }

        let timestamp = depth_data.timestamp.parse::<i64>()
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

    fn parse_depth_update(&self, update: &OkxDepthUpdate, symbol: &str, packet_id: u64, rcv_timestamp: i64) -> Result<Vec<OrderBookL2Update>> {
        let mut updates = Vec::new();
        
        for data in &update.data {
            let timestamp = data.timestamp.parse::<i64>()
                .map_err(|e| AppError::parse(format!("Invalid timestamp '{}': {}", data.timestamp, e)))?;

            let timestamp_micros = crate::types::time::millis_to_micros(timestamp);
            let seq_id = data.seq_id.unwrap_or(0);
            let first_update_id = data.prev_seq_id.unwrap_or(seq_id);

            // Process bids
            for bid_entry in &data.bids {
                if bid_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&bid_entry[0])
                        .map_err(|e| AppError::parse(format!("Invalid bid price '{}': {}", bid_entry[0], e)))?;
                    let size = fast_float::parse::<f64, _>(&bid_entry[1])
                        .map_err(|e| AppError::parse(format!("Invalid bid size '{}': {}", bid_entry[1], e)))?;

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
                    
                    updates.push(builder.build_bid(price, size));
                }
            }

            // Process asks
            for ask_entry in &data.asks {
                if ask_entry.len() >= 2 {
                    let price = fast_float::parse::<f64, _>(&ask_entry[0])
                        .map_err(|e| AppError::parse(format!("Invalid ask price '{}': {}", ask_entry[0], e)))?;
                    let size = fast_float::parse::<f64, _>(&ask_entry[1])
                        .map_err(|e| AppError::parse(format!("Invalid ask size '{}': {}", ask_entry[1], e)))?;

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
                    
                    updates.push(builder.build_ask(price, size));
                }
            }
        }

        Ok(updates)
    }
}

#[async_trait]
impl ExchangeConnector for OkxConnector {
    type Error = AppError;

    async fn connect(&mut self) -> Result<()> {
        let (ws_stream, _) = connect_async(&self.ws_url)
            .await
            .map_err(|e| AppError::connection(format!("Failed to connect to OKX WebSocket: {}", e)))?;

        self.ws_stream = Some(ws_stream);
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
        let ws_stream = self.ws_stream.as_mut()
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
        ws_stream.send(message)
            .await
            .map_err(|e| AppError::connection(format!("Failed to send subscription: {}", e)))?;

        self.sync_state = OkxSyncState::Subscribed;
        Ok(())
    }

    async fn next_message(&mut self) -> Result<Option<RawMessage>> {
        let ws_stream = self.ws_stream.as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                Ok(Some(RawMessage {
                    exchange_id: self.exchange_id,
                    data: text,
                    timestamp: SystemTime::now(),
                }))
            }
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
            OkxSyncState::Connected | OkxSyncState::Subscribed | OkxSyncState::Synced => ConnectionStatus::Connected,
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

    async fn validate_symbol(&self, symbol: &str) -> Result<()> {
        let inst_type = self.get_inst_type();
        let instruments = self.get_instruments(inst_type)
            .await
            .map_err(|e| AppError::connection(format!("Failed to get instruments: {}", e)))?;

        let okx_symbol = self.convert_symbol(symbol);
        let found = instruments.iter().any(|inst| {
            inst.inst_id == okx_symbol && inst.state == "live"
        });

        if !found {
            return Err(AppError::validation(format!("Symbol {} not found or not active on OKX {}", symbol, inst_type)));
        }

        Ok(())
    }

    async fn start_depth_stream(&mut self, symbol: &str, tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<()> {
        self.connect().await?;
        self.subscribe_l2(symbol).await?;

        let ws_stream = self.ws_stream.as_mut()
            .ok_or_else(|| AppError::connection("WebSocket not connected".to_string()))?;

        while let Some(message_result) = ws_stream.next().await {
            match message_result {
                Ok(Message::Text(text)) => {
                    let raw_message = RawMessage {
                        exchange_id: self.exchange_id,
                        data: text,
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

    async fn start_trade_stream(&mut self, _symbol: &str, _tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<()> {
        Err(AppError::connection("Trade streams not implemented for OKX yet".to_string()))
    }
}

/// Parse OKX raw message into L2 updates
pub fn parse_okx_message(raw_message: &RawMessage, symbol: &str, packet_id: u64, rcv_timestamp: i64) -> Result<Vec<OrderBookL2Update>> {
    let message: OkxMessage = serde_json::from_str(&raw_message.data)
        .map_err(|e| AppError::parse(format!("Failed to parse OKX message: {}", e)))?;

    match message {
        OkxMessage::Data(update) => {
            // Create temporary connector to use parsing logic
            let connector = if raw_message.exchange_id == ExchangeId::OkxSwap {
                OkxConnector::new_swap()
            } else {
                OkxConnector::new_spot()
            };
            connector.parse_depth_update(&update, symbol, packet_id, rcv_timestamp)
        }
        OkxMessage::Subscription { event: _, arg: _ } => {
            // Subscription acknowledgment - no data to process
            Ok(Vec::new())
        }
        OkxMessage::Error { event: _, code, msg } => {
            Err(AppError::parse(format!("OKX WebSocket error: {} - {}", code, msg)))
        }
    }
}

pub fn create_okx_swap_connector() -> OkxConnector {
    OkxConnector::new_swap()
}

pub fn create_okx_spot_connector() -> OkxConnector {
    OkxConnector::new_spot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_conversion_swap() {
        let connector = OkxConnector::new_swap();
        assert_eq!(connector.convert_symbol("BTCUSDT"), "BTC-USDT-SWAP");
        assert_eq!(connector.convert_symbol("ETHUSD"), "ETH-USD-SWAP");
    }

    #[test]
    fn test_symbol_conversion_spot() {
        let connector = OkxConnector::new_spot();
        assert_eq!(connector.convert_symbol("BTCUSDT"), "BTC-USDT");
        assert_eq!(connector.convert_symbol("ETHUSD"), "ETH-USD");
    }

    #[test]
    fn test_exchange_ids() {
        let swap_connector = OkxConnector::new_swap();
        let spot_connector = OkxConnector::new_spot();
        
        assert_eq!(swap_connector.exchange_id(), ExchangeId::OkxSwap);
        assert_eq!(spot_connector.exchange_id(), ExchangeId::OkxSpot);
    }
}