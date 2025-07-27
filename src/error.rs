use crate::exchanges::ExchangeError;

/// Main application error type
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("CLI argument error: {message}")]
    Cli { message: String },

    #[error("Configuration error: {message}")]
    Config { message: String },

    #[error("Exchange error: {source}")]
    Exchange {
        #[from]
        source: ExchangeError,
    },

    #[error("Pipeline error: {message}")]
    Pipeline { message: String },

    #[error("Output error: {message}")]
    Output { message: String },

    #[error("Parquet error: {source}")]
    Parquet {
        #[from]
        source: parquet::errors::ParquetError,
    },

    #[error("Arrow error: {source}")]
    Arrow {
        #[from]
        source: arrow::error::ArrowError,
    },

    #[error("IO error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("Serialization error: {source}")]
    Serde {
        #[from]
        source: serde_json::Error,
    },

    #[error("Channel error: {message}")]
    Channel { message: String },

    #[error("Timeout error: {message}")]
    Timeout { message: String },

    #[error("Fatal error: {message}")]
    Fatal { message: String },
}

impl AppError {
    pub fn cli(message: impl Into<String>) -> Self {
        Self::Cli {
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
        }
    }

    pub fn pipeline(message: impl Into<String>) -> Self {
        Self::Pipeline {
            message: message.into(),
        }
    }

    pub fn output(message: impl Into<String>) -> Self {
        Self::Output {
            message: message.into(),
        }
    }

    pub fn channel(message: impl Into<String>) -> Self {
        Self::Channel {
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout {
            message: message.into(),
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self::Fatal {
            message: message.into(),
        }
    }

    pub fn connection(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Connection {
                message: message.into(),
            },
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Symbol {
                message: message.into(),
            },
        }
    }

    pub fn data(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Parse {
                message: message.into(),
            },
        }
    }

    pub fn stream(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Protocol {
                message: message.into(),
            },
        }
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Parse {
                message: message.into(),
            },
        }
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::Exchange {
            source: ExchangeError::Io {
                source: std::io::Error::new(std::io::ErrorKind::Other, message.into()),
            },
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Fatal {
            message: message.into(),
        }
    }

    /// Check if error is recoverable (can retry) or fatal (should exit)
    pub fn is_recoverable(&self) -> bool {
        match self {
            AppError::Cli { .. } => false,
            AppError::Config { .. } => false,
            AppError::Fatal { .. } => false,
            AppError::Exchange { source } => match source {
                ExchangeError::Connection { .. } => true,
                ExchangeError::Network { .. } => true,
                ExchangeError::WebSocket { .. } => true,
                ExchangeError::RateLimit => true,
                ExchangeError::Authentication { .. } => false,
                ExchangeError::Symbol { .. } => false,
                ExchangeError::Parse { .. } => true, // Skip malformed messages
                ExchangeError::Protocol { .. } => true, // Can retry with resync
                ExchangeError::Io { .. } => true,
                ExchangeError::Api { .. } => true,
                ExchangeError::Unsupported { .. } => false,
            },
            AppError::Pipeline { .. } => true,
            AppError::Output { .. } => true,
            AppError::Channel { .. } => true,
            AppError::Timeout { .. } => true,
            AppError::Parquet { .. } => true,
            AppError::Arrow { .. } => true,
            AppError::Io { .. } => true,
            AppError::Serde { .. } => true,
        }
    }
}

/// Type alias for Result with AppError
pub type Result<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_recoverability() {
        let cli_error = AppError::cli("Invalid argument");
        assert!(!cli_error.is_recoverable());

        let timeout_error = AppError::timeout("Request timeout");
        assert!(timeout_error.is_recoverable());

        let fatal_error = AppError::fatal("Critical system failure");
        assert!(!fatal_error.is_recoverable());
    }

    #[test]
    fn test_error_creation() {
        let error = AppError::pipeline("Processing failed");
        match error {
            AppError::Pipeline { message } => assert_eq!(message, "Processing failed"),
            _ => panic!("Wrong error type"),
        }
    }
}
