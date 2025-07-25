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