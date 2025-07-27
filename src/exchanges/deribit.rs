use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{interval, timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::error::AppError;
use crate::exchanges::{ExchangeConnector, ExchangeError, ExchangeProcessor};
use crate::types::*;

/// Optimized Deribit connector for microsecond-level latency
/// Uses hybrid architecture: RPC for control, direct streaming for market data
#[derive(Debug)]
pub struct DeribitConnector {
    // WebSocket connection
    ws_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,

    // Authentication
    client_id: String,
    client_secret: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_expires_at: Option<Instant>,

    // Message routing - optimized for low latency
    next_request_id: AtomicU64,
    pending_rpc_calls: Arc<Mutex<FxHashMap<u64, oneshot::Sender<DeribitRpcResponse>>>>,

    // Market data - zero-copy hot path
    active_subscriptions: FxHashMap<String, String>, // channel -> instrument
    message_buffer: Box<[u8; 64 * 1024]>,            // Pre-allocated buffer

    // Connection management
    config: DeribitConfig,
    connection_status: ConnectionStatus,
}

#[derive(Debug, Clone)]
pub struct DeribitConfig {
    pub client_id: String,
    pub client_secret: String,
    pub use_testnet: bool,
    pub ws_url: String,
    pub heartbeat_interval: Duration,
    pub reconnect_delay: Duration,
    pub max_reconnect_attempts: usize,
}

// RPC message types (control path only)
#[derive(Debug, Serialize)]
struct DeribitRpcRequest {
    id: u64,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct DeribitRpcResponse {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<DeribitRpcError>,
}

#[derive(Debug, Deserialize)]
struct DeribitRpcError {
    code: i32,
    message: String,
}

// Authentication response
#[derive(Debug, Deserialize)]
struct AuthResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

// Market data message (hot path)
#[derive(Debug, Deserialize)]
struct DeribitNotification {
    method: String,
    params: DeribitNotificationParams,
}

#[derive(Debug, Deserialize)]
struct DeribitNotificationParams {
    channel: String,
    data: Value,
}

// L2 order book data (optimized structures)
#[derive(Debug, Deserialize)]
struct DeribitOrderBookData {
    instrument_name: String,
    change_id: u64,
    #[serde(deserialize_with = "deserialize_deribit_levels")]
    bids: Vec<DeribitLevel>,
    #[serde(deserialize_with = "deserialize_deribit_levels")]
    asks: Vec<DeribitLevel>,
    timestamp: u64,
}

#[derive(Debug)]
struct DeribitLevel {
    action: String,  // "new", "change", "delete"
    price: f64,
    amount: f64,
}

/// Custom deserializer for Deribit price levels in [action, price, amount] format
fn deserialize_deribit_levels<'de, D>(deserializer: D) -> Result<Vec<DeribitLevel>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    use std::fmt;

    struct DeribitLevelsVisitor;

    impl<'de> Visitor<'de> for DeribitLevelsVisitor {
        type Value = Vec<DeribitLevel>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an array of [action, price, amount] arrays")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Vec<DeribitLevel>, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut levels = Vec::new();
            
            while let Some(level) = seq.next_element::<serde_json::Value>()? {
                if let Some(array) = level.as_array() {
                    if array.len() >= 3 {
                        // Extract action, price, amount
                        let action = match &array[0] {
                            serde_json::Value::String(s) => s.clone(),
                            _ => continue, // Skip invalid entries
                        };
                        
                        let price = match &array[1] {
                            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
                            serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                            _ => 0.0,
                        };
                        
                        let amount = match &array[2] {
                            serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
                            serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
                            _ => 0.0,
                        };
                        
                        levels.push(DeribitLevel {
                            action,
                            price,
                            amount,
                        });
                    }
                }
            }
            
            Ok(levels)
        }
    }

    deserializer.deserialize_seq(DeribitLevelsVisitor)
}

#[derive(Debug, thiserror::Error)]
pub enum DeribitError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("RPC error: {0:?}")]
    RpcError(DeribitRpcError),

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Connection timeout")]
    ConnectionTimeout,

    #[error("Channel receive error: {0}")]
    ChannelRecv(#[from] tokio::sync::oneshot::error::RecvError),

    #[error("Invalid channel: {0}")]
    InvalidChannel(String),

    #[error("Missing environment variable: {0}")]
    MissingEnvVar(String),
}

impl From<DeribitError> for ExchangeError {
    fn from(error: DeribitError) -> Self {
        ExchangeError::Connection {
            message: error.to_string(),
        }
    }
}

impl DeribitConfig {
    pub fn from_env() -> Result<Self, AppError> {
        let client_id = std::env::var("DERIBIT_CLIENT_ID").map_err(|_| {
            AppError::config("Missing DERIBIT_CLIENT_ID environment variable".to_string())
        })?;

        let client_secret = std::env::var("DERIBIT_CLIENT_SECRET").map_err(|_| {
            AppError::config("Missing DERIBIT_CLIENT_SECRET environment variable".to_string())
        })?;

        let use_testnet = std::env::var("DERIBIT_USE_TESTNET")
            .unwrap_or_else(|_| "true".to_string())
            .parse()
            .unwrap_or(true);

        let ws_url = if use_testnet {
            "wss://test.deribit.com/ws/api/v2/"
        } else {
            "wss://www.deribit.com/ws/api/v2/"
        }
        .to_string();

        Ok(Self {
            client_id,
            client_secret,
            use_testnet,
            ws_url,
            heartbeat_interval: Duration::from_secs(60),
            reconnect_delay: Duration::from_millis(1000),
            max_reconnect_attempts: 5,
        })
    }
}

impl DeribitConnector {
    pub fn new(config: DeribitConfig) -> Self {
        Self {
            ws_stream: None,
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            access_token: None,
            refresh_token: None,
            token_expires_at: None,
            next_request_id: AtomicU64::new(1),
            pending_rpc_calls: Arc::new(Mutex::new(FxHashMap::default())),
            active_subscriptions: FxHashMap::default(),
            message_buffer: Box::new([0; 64 * 1024]),
            config,
            connection_status: ConnectionStatus::Disconnected,
        }
    }

    async fn send_rpc_request<T>(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, AppError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let request = DeribitRpcRequest {
            id,
            method: method.to_string(),
            params,
        };

        // Create response channel
        let (tx, mut rx) = oneshot::channel();
        self.pending_rpc_calls.lock().await.insert(id, tx);

        // Send request
        let message = serde_json::to_string(&request)
            .map_err(|e| AppError::data(format!("JSON serialization error: {}", e)))?;
        if let Some(ws) = &mut self.ws_stream {
            ws.send(Message::Text(message))
                .await
                .map_err(|e| AppError::connection(format!("WebSocket send error: {}", e)))?;
        } else {
            return Err(AppError::connection("WebSocket not connected".to_string()));
        }

        // Process messages until we get our response or timeout
        let start_time = Instant::now();
        let timeout_duration = Duration::from_secs(10);
        
        loop {
            // Check for timeout
            if start_time.elapsed() >= timeout_duration {
                // Clean up pending call before returning error
                self.pending_rpc_calls.lock().await.remove(&id);
                return Err(AppError::connection("RPC request timeout".to_string()));
            }

            // Try to receive the response (non-blocking)
            match rx.try_recv() {
                Ok(response) => {
                    // Got our response!
                    if let Some(error) = response.error {
                        return Err(AppError::connection(format!(
                            "RPC error: {}: {}",
                            error.code, error.message
                        )));
                    }

                    let result = response
                        .result
                        .ok_or_else(|| AppError::connection("Missing RPC result".to_string()))?;
                    return serde_json::from_value(result)
                        .map_err(|e| AppError::data(format!("RPC response parse error: {}", e)));
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // No response yet, continue processing messages
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    return Err(AppError::connection("RPC channel closed".to_string()));
                }
            }

            // Process incoming WebSocket message
            if let Some(ws) = &mut self.ws_stream {
                // Use a very short timeout to keep this loop responsive
                match timeout(Duration::from_millis(100), ws.next()).await {
                    Ok(Some(message)) => {
                        let message = message.map_err(|e| {
                            AppError::connection(format!("WebSocket message error: {}", e))
                        })?;
                        
                        // Process the message (this will handle RPC responses)
                        let _ = self.process_message(message).await.map_err(|e| {
                            AppError::connection(format!("Message processing error: {}", e))
                        })?;
                    }
                    Ok(None) => {
                        // WebSocket closed
                        return Err(AppError::connection("WebSocket connection closed".to_string()));
                    }
                    Err(_) => {
                        // Timeout waiting for message - continue loop to check response
                        continue;
                    }
                }
            } else {
                return Err(AppError::connection("WebSocket not connected".to_string()));
            }
        }
    }

    async fn authenticate(&mut self) -> Result<(), AppError> {
        let params = json!({
            "grant_type": "client_credentials",
            "client_id": self.client_id,
            "client_secret": self.client_secret
        });

        let auth_response: AuthResponse =
            self.send_rpc_request("public/auth", Some(params)).await?;

        self.access_token = Some(auth_response.access_token);
        self.refresh_token = Some(auth_response.refresh_token);
        self.token_expires_at =
            Some(Instant::now() + Duration::from_secs(auth_response.expires_in));

        Ok(())
    }

    async fn setup_heartbeat(&mut self) -> Result<(), AppError> {
        let params = json!({
            "interval": self.config.heartbeat_interval.as_secs()
        });

        let _: Value = self
            .send_rpc_request("public/set_heartbeat", Some(params))
            .await?;

        Ok(())
    }

    /// Process incoming message with optimized routing
    async fn process_message(&mut self, message: Message) -> Result<Option<RawMessage>, AppError> {
        let text = match message {
            Message::Text(text) => text,
            Message::Ping(data) => {
                // Respond to ping immediately
                if let Some(ws) = &mut self.ws_stream {
                    ws.send(Message::Pong(data))
                        .await
                        .map_err(|e| AppError::connection(format!("Pong send error: {}", e)))?;
                }
                return Ok(None);
            }
            Message::Close(_) => {
                self.connection_status = ConnectionStatus::Disconnected;
                return Ok(None);
            }
            _ => return Ok(None),
        };

        // CRITICAL: Fast path for market data - avoid full JSON parsing
        if text.contains("\"method\":\"subscription\"") {
            return self.process_market_data_fast(&text).await;
        }

        // Slow path for RPC responses and control messages
        if let Ok(rpc_response) = serde_json::from_str::<DeribitRpcResponse>(&text) {
            if let Some(id) = rpc_response.id {
                if let Some(tx) = self.pending_rpc_calls.lock().await.remove(&id) {
                    let _ = tx.send(rpc_response);
                }
            }
            return Ok(None);
        }

        // Heartbeat handling
        if text.contains("\"method\":\"heartbeat\"") {
            let response = json!({
                "method": "public/test",
                "params": {}
            });
            if let Some(ws) = &mut self.ws_stream {
                ws.send(Message::Text(response.to_string()))
                    .await
                    .map_err(|e| {
                        AppError::connection(format!("Heartbeat response error: {}", e))
                    })?;
            }
            return Ok(None);
        }

        Ok(None)
    }

    /// Optimized market data processing - HOT PATH
    /// Uses minimal JSON parsing for maximum performance
    async fn process_market_data_fast(&self, text: &str) -> Result<Option<RawMessage>, AppError> {
        // Fast channel extraction without full JSON parsing
        let channel = self.extract_channel_fast(text)?;

        // Check if this is a subscribed channel
        if let Some(instrument) = self.active_subscriptions.get(&channel) {
            return Ok(Some(RawMessage {
                exchange_id: ExchangeId::Deribit,
                data: text.to_string(),
                timestamp: SystemTime::now(),
            }));
        }

        Ok(None)
    }

    /// Fast channel extraction using string parsing instead of JSON
    fn extract_channel_fast(&self, text: &str) -> Result<String, AppError> {
        // Look for "channel":"book.BTC-PERPETUAL.raw" pattern
        if let Some(start) = text.find("\"channel\":\"") {
            let start = start + 11; // Length of "channel":"
            if let Some(end) = text[start..].find('"') {
                return Ok(text[start..start + end].to_string());
            }
        }
        Err(AppError::data("Channel not found in message".to_string()))
    }

    /// Subscribe to L2 order book with RAW interval for maximum speed
    async fn subscribe_l2_internal(&mut self, instrument: &str) -> Result<(), AppError> {
        // Use RAW interval for unfiltered, real-time updates
        let channel = format!("book.{}.raw", instrument);

        let params = json!({
            "channels": [&channel]
        });

        let _: Value = self
            .send_rpc_request("public/subscribe", Some(params))
            .await?;

        // Track subscription for fast message routing
        self.active_subscriptions
            .insert(channel, instrument.to_string());

        Ok(())
    }
}

#[async_trait]
impl ExchangeConnector for DeribitConnector {
    type Error = AppError;

    async fn connect(&mut self) -> Result<(), Self::Error> {
        // Connect WebSocket
        let (ws_stream, _) = connect_async(&self.config.ws_url)
            .await
            .map_err(|e| AppError::connection(format!("WebSocket connection failed: {}", e)))?;
        self.ws_stream = Some(ws_stream);
        self.connection_status = ConnectionStatus::Connected;

        // Authenticate
        self.authenticate()
            .await
            .map_err(|e| AppError::connection(format!("Authentication failed: {}", e)))?;

        // Setup heartbeat
        self.setup_heartbeat()
            .await
            .map_err(|e| AppError::connection(format!("Heartbeat setup failed: {}", e)))?;

        Ok(())
    }

    async fn get_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot, Self::Error> {
        // For now, return empty snapshot - implement REST API call if needed
        Ok(OrderBookSnapshot {
            exchange_id: ExchangeId::Deribit,
            symbol: symbol.to_string(),
            bids: vec![],
            asks: vec![],
            timestamp: chrono::Utc::now().timestamp_micros(),
            last_update_id: 0,
            sequence: 0,
        })
    }

    async fn subscribe_l2(&mut self, symbol: &str) -> Result<(), Self::Error> {
        self.subscribe_l2_internal(symbol)
            .await
            .map_err(|e| AppError::stream(format!("L2 subscription failed: {}", e)))
    }

    async fn next_message(&mut self) -> Result<Option<RawMessage>, Self::Error> {
        if let Some(ws) = &mut self.ws_stream {
            if let Some(message) = ws.next().await {
                return self
                    .process_message(
                        message.map_err(|e| {
                            AppError::stream(format!("WebSocket message error: {}", e))
                        })?,
                    )
                    .await
                    .map_err(|e| AppError::stream(format!("Message processing error: {}", e)));
            }
        }
        Ok(None)
    }

    fn exchange_id(&self) -> ExchangeId {
        ExchangeId::Deribit
    }

    fn connection_status(&self) -> ConnectionStatus {
        self.connection_status
    }

    async fn disconnect(&mut self) -> Result<(), Self::Error> {
        if let Some(mut ws) = self.ws_stream.take() {
            ws.close(None)
                .await
                .map_err(|e| AppError::connection(format!("WebSocket close error: {}", e)))?;
        }
        self.connection_status = ConnectionStatus::Disconnected;
        Ok(())
    }

    async fn validate_symbol(&self, symbol: &str) -> Result<(), Self::Error> {
        // Deribit uses native instrument names like BTC-PERPETUAL
        // Basic validation - could be enhanced with REST API call
        if symbol.contains("-") {
            Ok(())
        } else {
            Err(AppError::validation(format!(
                "Invalid instrument: {}",
                symbol
            )))
        }
    }

    async fn start_depth_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<(), Self::Error> {
        self.subscribe_l2(symbol).await?;

        // Message forwarding loop (this would typically run in a separate task)
        while let Some(message) = self.next_message().await? {
            if tx.send(message).await.is_err() {
                break; // Channel closed
            }
        }

        Ok(())
    }

    async fn start_trade_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<(), Self::Error> {
        // Similar to depth stream but for trades
        // TODO: Implement trade subscription
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Factory function for creating Deribit connector
pub fn create_deribit_connector()
-> Result<Box<dyn ExchangeConnector<Error = AppError>>, ExchangeError> {
    let config = DeribitConfig::from_env().map_err(|e| ExchangeError::Connection {
        message: e.to_string(),
    })?;
    Ok(Box::new(DeribitConnector::new(config)))
}

/// Deribit-specific processor for handling market data
pub struct DeribitProcessor {
    base_processor: crate::exchanges::BaseProcessor,
}

impl DeribitProcessor {
    pub fn new() -> Self {
        Self {
            base_processor: crate::exchanges::BaseProcessor::default(),
        }
    }

    /// Process Deribit order book data with zero-copy optimizations
    fn process_order_book_data(
        &mut self,
        data: &DeribitOrderBookData,
        rcv_timestamp: i64,
        packet_id: u64,
    ) -> Result<Vec<OrderBookL2Update>, crate::error::AppError> {
        let mut updates = Vec::with_capacity(data.bids.len() + data.asks.len());
        let timestamp = data.timestamp as i64 * 1000; // Convert to microseconds

        // Process bids
        for bid in &data.bids {
            let price = bid.price;
            let amount = bid.amount;

            let seq_id = self.next_sequence_id();

            // Determine action based on Deribit action string
            let action = match bid.action.as_str() {
                "delete" => L2Action::Delete,
                "new" | "change" => L2Action::Update,
                _ => L2Action::Update, // Default to update for unknown actions
            };

            updates.push(OrderBookL2Update {
                timestamp,
                rcv_timestamp,
                exchange: ExchangeId::Deribit,
                ticker: data.instrument_name.clone(),
                seq_id,
                packet_id: packet_id as i64,
                update_id: data.change_id as i64,
                first_update_id: data.change_id as i64,
                action,
                side: OrderSide::Bid,
                price,
                qty: amount,
            });
        }

        // Process asks
        for ask in &data.asks {
            let price = ask.price;
            let amount = ask.amount;

            let seq_id = self.next_sequence_id();

            // Determine action based on Deribit action string
            let action = match ask.action.as_str() {
                "delete" => L2Action::Delete,
                "new" | "change" => L2Action::Update,
                _ => L2Action::Update, // Default to update for unknown actions
            };

            updates.push(OrderBookL2Update {
                timestamp,
                rcv_timestamp,
                exchange: ExchangeId::Deribit,
                ticker: data.instrument_name.clone(),
                seq_id,
                packet_id: packet_id as i64,
                update_id: data.change_id as i64,
                first_update_id: data.change_id as i64,
                action,
                side: OrderSide::Ask,
                price,
                qty: amount,
            });
        }

        Ok(updates)
    }
}

impl ExchangeProcessor for DeribitProcessor {
    type Error = crate::error::AppError;

    fn process_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> Result<Vec<crate::types::OrderBookL2Update>, Self::Error> {
        // Track message received
        self.base_processor.metrics.increment_received();

        let packet_arrival_us = rcv_timestamp;

        // Start parsing timer
        let parse_start = crate::types::time::now_micros();

        // Parse the notification
        let notification: DeribitNotification =
            serde_json::from_str(&raw_msg.data).map_err(|e| crate::error::AppError::Pipeline {
                message: format!("Failed to parse Deribit message: {}", e),
            })?;

        // Calculate parse time
        let parse_end = crate::types::time::now_micros();
        if let Some(parse_time) = parse_end.checked_sub(parse_start) {
            self.base_processor.metrics.update_parse_time(parse_time as u64);
        }

        if notification.method == "subscription" {
            // Check if this is an order book update
            if notification.params.channel.starts_with("book.") {
                // Start transformation timer
                let transform_start = crate::types::time::now_micros();

                let order_book_data: DeribitOrderBookData =
                    serde_json::from_value(notification.params.data).map_err(|e| {
                        crate::error::AppError::Pipeline {
                            message: format!("Failed to parse Deribit order book data: {}", e),
                        }
                    })?;

                // Calculate overall latency (exchange timestamp to receive timestamp)
                let exchange_timestamp_us = order_book_data.timestamp as i64 * 1000; // Convert to microseconds
                if let Some(overall_latency) = rcv_timestamp.checked_sub(exchange_timestamp_us) {
                    if overall_latency >= 0 {
                        self.base_processor.metrics.update_overall_latency(overall_latency as u64);
                    }
                }

                let updates =
                    self.process_order_book_data(&order_book_data, rcv_timestamp, packet_id)?;

                // Calculate transformation time
                let transform_end = crate::types::time::now_micros();
                if let Some(transform_time) = transform_end.checked_sub(transform_start) {
                    self.base_processor
                        .metrics
                        .update_transform_time(transform_time as u64);
                }

                // Update metrics
                self.base_processor.metrics.increment_processed();

                // Calculate bid/ask counts for complexity metrics
                let total_bid_count = order_book_data.bids.len() as u32;
                let total_ask_count = order_book_data.asks.len() as u32;
                
                self.base_processor.metrics.update_message_complexity(
                    total_bid_count,
                    total_ask_count,
                    message_bytes,
                );

                // Update packet metrics (Deribit sends one message per packet)
                let messages_in_packet = 1;
                let entries_in_packet = total_bid_count + total_ask_count;
                self.base_processor.metrics.update_packet_metrics(
                    messages_in_packet,
                    entries_in_packet,
                    message_bytes,
                );

                // Calculate CODE LATENCY: From network packet arrival to business logic ready
                let processing_complete = crate::types::time::now_micros();
                if let Some(code_latency) = processing_complete.checked_sub(packet_arrival_us) {
                    if code_latency >= 0 {
                        self.base_processor.metrics.update_code_latency(code_latency as u64);
                    }
                }

                return Ok(updates);
            }
        }

        Ok(vec![])
    }

    fn base_processor(&mut self) -> &mut crate::exchanges::BaseProcessor {
        &mut self.base_processor
    }

    fn base_processor_ref(&self) -> &crate::exchanges::BaseProcessor {
        &self.base_processor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_extraction() {
        let connector = DeribitConnector::new(DeribitConfig {
            client_id: "test".to_string(),
            client_secret: "test".to_string(),
            use_testnet: true,
            ws_url: "wss://test.deribit.com/ws/api/v2/".to_string(),
            heartbeat_interval: Duration::from_secs(60),
            reconnect_delay: Duration::from_millis(1000),
            max_reconnect_attempts: 5,
        });

        let message =
            r#"{"method":"subscription","params":{"channel":"book.BTC-PERPETUAL.raw","data":{}}}"#;
        let channel = connector.extract_channel_fast(message).unwrap();
        assert_eq!(channel, "book.BTC-PERPETUAL.raw");
    }

    #[test]
    fn test_deribit_processor_creation() {
        let processor = DeribitProcessor::new();
        // Verify processor is created successfully
        assert_eq!(processor.base_processor_ref().sequence_counter, 0);
    }

    #[test]
    fn test_order_book_data_processing() {
        let mut processor = DeribitProcessor::new();

        let test_data = DeribitOrderBookData {
            instrument_name: "BTC-PERPETUAL".to_string(),
            change_id: 12345,
            bids: vec![
                DeribitLevel { action: "new".to_string(), price: 50000.0, amount: 1.5 },
                DeribitLevel { action: "new".to_string(), price: 49999.0, amount: 2.0 },
            ],
            asks: vec![
                DeribitLevel { action: "new".to_string(), price: 50001.0, amount: 1.0 },
                DeribitLevel { action: "new".to_string(), price: 50002.0, amount: 1.5 },
            ],
            timestamp: 1640995200000, // Example timestamp
        };

        let updates = processor
            .process_order_book_data(&test_data, 1640995200000000, 1)
            .unwrap();

        // Should have 4 updates (2 bids + 2 asks)
        assert_eq!(updates.len(), 4);

        // Check first bid update
        assert_eq!(updates[0].ticker, "BTC-PERPETUAL");
        assert_eq!(updates[0].exchange, ExchangeId::Deribit);
        assert_eq!(updates[0].side, OrderSide::Bid);
        assert_eq!(updates[0].price, 50000.0);
        assert_eq!(updates[0].qty, 1.5);
        assert_eq!(updates[0].action, L2Action::Update);
    }
}
