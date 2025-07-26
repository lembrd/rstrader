use async_trait::async_trait;
use crate::types::{ConnectionStatus, ExchangeId, OrderBookSnapshot, RawMessage};

pub mod binance;
pub mod okx;

/// Error types for exchange operations
#[derive(thiserror::Error, Debug)]
pub enum ExchangeError {
    #[error("Connection error: {message}")]
    Connection { message: String },
    
    #[error("Authentication error: {message}")]
    Authentication { message: String },
    
    #[error("Network error: {source}")]
    Network { #[from] source: reqwest::Error },
    
    #[error("WebSocket error: {source}")]
    WebSocket { #[from] source: tokio_tungstenite::tungstenite::Error },
    
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
    Io { #[from] source: std::io::Error },
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
    
    /// Get next message from WebSocket stream
    async fn next_message(&mut self) -> Result<Option<RawMessage>, Self::Error>;
    
    /// Get exchange identifier
    fn exchange_id(&self) -> ExchangeId;
    
    /// Get current connection status
    fn connection_status(&self) -> ConnectionStatus;
    
    /// Disconnect and cleanup resources
    async fn disconnect(&mut self) -> Result<(), Self::Error>;
    
    /// Check if connection is healthy
    fn is_connected(&self) -> bool {
        matches!(self.connection_status(), ConnectionStatus::Connected)
    }
    
    /// Validate that a symbol exists and is tradeable
    async fn validate_symbol(&self, symbol: &str) -> Result<(), Self::Error>;
    
    /// Start depth stream and send raw messages to channel
    async fn start_depth_stream(&mut self, symbol: &str, tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<(), Self::Error>;
    
    /// Start trade stream and send raw messages to channel  
    async fn start_trade_stream(&mut self, symbol: &str, tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<(), Self::Error>;
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
        Self {
            sequence_counter: 0,
            packet_counter: 0,
            metrics: crate::types::Metrics::new(),
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
        self.base_processor().metrics.update_throughput(messages_per_sec);
    }
}

/// Factory for creating exchange-specific processors
pub struct ProcessorFactory;

impl ProcessorFactory {
    /// Create processor for Binance Futures
    pub fn create_binance_processor() -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        Box::new(binance::BinanceProcessor::new())
    }
    
    /// Create processor for OKX SWAP
    pub fn create_okx_swap_processor(
        registry: Option<std::sync::Arc<okx::InstrumentRegistry>>
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        Box::new(okx::OkxProcessor::new(crate::types::ExchangeId::OkxSwap, registry))
    }
    
    /// Create processor for OKX SPOT
    pub fn create_okx_spot_processor(
        registry: Option<std::sync::Arc<okx::InstrumentRegistry>>
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        Box::new(okx::OkxProcessor::new(crate::types::ExchangeId::OkxSpot, registry))
    }
    
    /// Create processor by exchange ID
    pub fn create_processor(
        exchange_id: crate::types::ExchangeId,
        okx_registry: Option<std::sync::Arc<okx::InstrumentRegistry>>
    ) -> Box<dyn ExchangeProcessor<Error = crate::error::AppError>> {
        match exchange_id {
            crate::types::ExchangeId::BinanceFutures => Self::create_binance_processor(),
            crate::types::ExchangeId::OkxSwap => Self::create_okx_swap_processor(okx_registry),
            crate::types::ExchangeId::OkxSpot => Self::create_okx_spot_processor(okx_registry),
        }
    }
}
