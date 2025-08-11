#![allow(dead_code)]
use crate::types::{ConnectionStatus, ExchangeId, OrderBookSnapshot, RawMessage};
use async_trait::async_trait;

pub mod binance;
pub mod deribit;
pub mod okx;

/// Error types for exchange operations
#[allow(dead_code)]
#[derive(thiserror::Error, Debug)]
pub enum ExchangeError {
    #[error("Connection error: {message}")]
    Connection { message: String },

    #[error("Authentication error: {message}")]
    Authentication { message: String },

    #[error("Network error: {source}")]
    Network {
        #[from]
        source: reqwest::Error,
    },

    #[error("WebSocket error: {source}")]
    WebSocket {
        #[from]
        source: tokio_tungstenite::tungstenite::Error,
    },

    #[error("Parse error: {message}")]
    Parse { message: String },

    #[error("Protocol error: {message}")]
    Protocol { message: String },

    #[error("Rate limit exceeded")]
    RateLimit,

    #[error("Invalid symbol: {message}")]
    Symbol { message: String },

    #[error("API error: {message}")]
    Api { message: String },

    #[error("Unsupported operation: {message}")]
    Unsupported { message: String },

    #[error("IO error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
}

/// Trait for uniform exchange interface
#[async_trait]
pub trait ExchangeConnector: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Connect to the exchange
    async fn connect(&mut self) -> Result<(), Self::Error>;

    /// Get initial order book snapshot via REST API
    async fn get_snapshot(&self, symbol: &str) -> Result<OrderBookSnapshot, Self::Error>;

    /// Subscribe to L2 order book depth stream
    async fn subscribe_l2(&mut self, symbol: &str) -> Result<(), Self::Error>;

    /// Subscribe to trade stream without synchronization requirements
    async fn subscribe_trades(&mut self, symbol: &str) -> Result<(), Self::Error>;

    /// Get next message from WebSocket stream
    async fn next_message(&mut self) -> Result<Option<RawMessage>, Self::Error>;

    /// Get exchange identifier
    fn exchange_id(&self) -> ExchangeId;

    /// Get current connection status
    fn connection_status(&self) -> ConnectionStatus;

    /// Disconnect and cleanup resources
    async fn disconnect(&mut self) -> Result<(), Self::Error>;

    /// Get peer socket address if connected
    fn peer_addr(&self) -> Option<std::net::SocketAddr> { None }

    /// Check if connection is healthy
    fn is_connected(&self) -> bool {
        matches!(self.connection_status(), ConnectionStatus::Connected)
    }

    /// Validate that a symbol exists and is tradeable
    async fn validate_symbol(&self, symbol: &str) -> Result<(), Self::Error>;

    /// Start depth stream and send raw messages to channel
    async fn start_depth_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<(), Self::Error>;

    /// Start trade stream and send raw messages to channel  
    async fn start_trade_stream(
        &mut self,
        symbol: &str,
        tx: tokio::sync::mpsc::Sender<RawMessage>,
    ) -> Result<(), Self::Error>;

    /// Allow downcasting to concrete types for exchange-specific operations
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Optional: Prepare L2 synchronization after snapshot retrieval.
    /// Default no-op for connectors that do not require explicit preparation.
    fn prepare_l2_sync(&mut self, _snapshot: &OrderBookSnapshot) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Optional: Take pre-snapshot buffered L2 messages that must be replayed
    /// after reconciliation to ensure continuity. Default: no replay.
    fn take_l2_replay_messages(&mut self) -> Option<Vec<RawMessage>> {
        None
    }
}

/// Base processor containing common fields shared by all exchange processors
#[derive(Debug)]
pub struct BaseProcessor {
    pub sequence_counter: i64,
    pub packet_counter: i64,
    pub metrics: crate::types::Metrics,
    pub updates_buffer: Vec<crate::types::OrderBookL2Update>,
}

impl BaseProcessor {
    /// Create new base processor with default values
    pub fn new() -> Self {
        let metrics = {
            #[cfg(feature = "metrics-hdr")]
            let mut m = crate::types::Metrics::new();
            #[cfg(not(feature = "metrics-hdr"))]
            let m = crate::types::Metrics::new();
            #[cfg(feature = "metrics-hdr")]
            {
                m.enable_histograms(crate::metrics::HistogramBounds::default());
            }
            m
        };
        Self {
            sequence_counter: 0,
            packet_counter: 0,
            metrics,
            updates_buffer: Vec::with_capacity(20),
        }
    }
}

impl Default for BaseProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for exchange-specific message processing
/// Separates exchange-specific logic from core stream processing
pub trait ExchangeProcessor: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Process raw message and return normalized updates
    fn process_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> Result<Vec<crate::types::OrderBookL2Update>, Self::Error>;

    /// Process raw trade message and return normalized trade updates
    fn process_trade_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
    ) -> Result<Vec<crate::types::TradeUpdate>, Self::Error>;

    /// Process raw message and return unified stream data
    fn process_unified_message(
        &mut self,
        raw_msg: crate::types::RawMessage,
        symbol: &str,
        rcv_timestamp: i64,
        packet_id: u64,
        message_bytes: u32,
        stream_type: crate::types::StreamType,
    ) -> Result<Vec<crate::types::StreamData>, Self::Error> {
        match stream_type {
            crate::types::StreamType::L2 => {
                let l2_updates = self.process_message(raw_msg, symbol, rcv_timestamp, packet_id, message_bytes)?;
                Ok(l2_updates.into_iter().map(crate::types::StreamData::L2).collect())
            }
            crate::types::StreamType::Trade => {
                let trade_updates = self.process_trade_message(raw_msg, symbol, rcv_timestamp, packet_id, message_bytes)?;
                Ok(trade_updates.into_iter().map(crate::types::StreamData::Trade).collect())
            }
        }
    }

    /// Get mutable reference to base processor for shared functionality
    fn base_processor(&mut self) -> &mut BaseProcessor;

    /// Get immutable reference to base processor for shared functionality
    fn base_processor_ref(&self) -> &BaseProcessor;

    /// Get processor metrics
    fn metrics(&self) -> &crate::types::Metrics {
        &self.base_processor_ref().metrics
    }

    /// Get next sequence ID
    fn next_sequence_id(&mut self) -> i64 {
        let base = self.base_processor();
        base.sequence_counter += 1;
        base.sequence_counter
    }

    /// Get next packet ID  
    fn next_packet_id(&mut self) -> u64 {
        let base = self.base_processor();
        base.packet_counter += 1;
        base.packet_counter as u64
    }

    /// Update throughput metrics
    fn update_throughput(&mut self, messages_per_sec: f64) {
        self.base_processor()
            .metrics
            .update_throughput(messages_per_sec);
    }
}

/// Factory for creating exchange-specific processors
pub struct ProcessorFactory;

impl ProcessorFactory {
    /// Create processor for Binance Futures
    pub fn create_binance_processor() -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>>
    {
        Box::new(binance::BinanceProcessor::new())
    }

    /// Create processor for OKX SWAP
    pub fn create_okx_swap_processor(
        registry: Option<std::sync::Arc<okx::InstrumentRegistry>>,
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        Box::new(okx::OkxProcessor::new(
            crate::types::ExchangeId::OkxSwap,
            registry,
        ))
    }

    /// Create processor for OKX SPOT
    pub fn create_okx_spot_processor(
        registry: Option<std::sync::Arc<okx::InstrumentRegistry>>,
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        Box::new(okx::OkxProcessor::new(
            crate::types::ExchangeId::OkxSpot,
            registry,
        ))
    }

    /// Create processor for Deribit
    pub fn create_deribit_processor() -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>>
    {
        Box::new(deribit::DeribitProcessor::new())
    }

    /// Create processor by exchange ID
    pub fn create_processor(
        exchange_id: crate::types::ExchangeId,
        okx_registry: Option<std::sync::Arc<okx::InstrumentRegistry>>,
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        match exchange_id {
            crate::types::ExchangeId::BinanceFutures => Self::create_binance_processor(),
            crate::types::ExchangeId::OkxSwap => Self::create_okx_swap_processor(okx_registry),
            crate::types::ExchangeId::OkxSpot => Self::create_okx_spot_processor(okx_registry),
            crate::types::ExchangeId::Deribit => Self::create_deribit_processor(),
        }
    }
}

/// Factory for creating exchange connectors consistently (initial spawn and rotation)
pub struct ConnectorFactory;

impl ConnectorFactory {
    pub fn create_connector(
        exchange: crate::cli::Exchange,
        okx_swap_registry: Option<std::sync::Arc<okx::InstrumentRegistry>>,
        okx_spot_registry: Option<std::sync::Arc<okx::InstrumentRegistry>>,
    ) -> Box<dyn ExchangeConnector<Error = crate::error::AppError>> {
        match exchange {
            crate::cli::Exchange::BinanceFutures => Box::new(binance::BinanceFuturesConnector::new()),
            crate::cli::Exchange::OkxSwap => {
                let reg = okx_swap_registry.expect("OKX SWAP registry not initialized");
                Box::new(okx::OkxConnector::new_swap(reg))
            }
            crate::cli::Exchange::OkxSpot => {
                let reg = okx_spot_registry.expect("OKX SPOT registry not initialized");
                Box::new(okx::OkxConnector::new_spot(reg))
            }
            crate::cli::Exchange::Deribit => {
                let cfg = crate::exchanges::deribit::DeribitConfig::from_env()
                    .expect("Failed to load Deribit config");
                Box::new(crate::exchanges::deribit::DeribitConnector::new(cfg))
            }
        }
    }
}
