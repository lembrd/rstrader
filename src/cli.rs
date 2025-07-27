use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "xtrader")]
#[command(about = "High-performance cryptocurrency market data collector")]
pub struct Args {
    #[arg(
        long,
        help = "Subscriptions in format stream_type:exchange@instrument (e.g., L2:OKX_SWAP@BTCUSDT,L2:BINANCE_FUTURES@ETHUSDT)"
    )]
    pub subscriptions: String,

    #[arg(long, help = "Output directory for Parquet files (will create individual files per stream type)")]
    pub output_directory: PathBuf,

    #[arg(long, help = "Enable verbose output with metrics and statistics")]
    pub verbose: bool,

    #[arg(long, help = "Shutdown after N seconds (for debugging)")]
    pub shutdown_after: Option<u64>,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum Exchange {
    #[value(name = "BINANCE_FUTURES")]
    BinanceFutures,
    #[value(name = "OKX_SWAP")]
    OkxSwap,
    #[value(name = "OKX_SPOT")]
    OkxSpot,
    #[value(name = "DERIBIT")]
    Deribit,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum StreamType {
    #[value(name = "L2")]
    L2,
    #[value(name = "TRADES")]
    Trades,
}
#[derive(Clone, Debug)]
pub struct SubscriptionSpec {
    pub stream_type: StreamType,
    pub exchange: Exchange,
    pub instrument: String,
}

impl SubscriptionSpec {
    /// Parse subscription format: stream_type:exchange@instrument
    /// Example: L2:OKX_SWAP@BTCUSDT
    pub fn parse(input: &str) -> Result<Self, String> {
        let parts: Vec<&str> = input.split(':').collect();
        if parts.len() != 2 {
            return Err(format!(
                "Invalid format '{}'. Expected 'stream_type:exchange@instrument'",
                input
            ));
        }

        let stream_type_str = parts[0];
        let exchange_instrument = parts[1];

        let exchange_parts: Vec<&str> = exchange_instrument.split('@').collect();
        if exchange_parts.len() != 2 {
            return Err(format!(
                "Invalid format '{}'. Expected 'exchange@instrument' after ':'",
                exchange_instrument
            ));
        }

        let exchange_str = exchange_parts[0];
        let instrument = exchange_parts[1].to_string();

        if instrument.is_empty() {
            return Err("Instrument cannot be empty".to_string());
        }

        // Parse stream type
        let stream_type = match stream_type_str {
            "L2" => StreamType::L2,
            "TRADES" => StreamType::Trades,
            _ => {
                return Err(format!(
                    "Invalid stream type '{}'. Supported: L2, TRADES",
                    stream_type_str
                ));
            }
        };

        // Parse exchange
        let exchange = match exchange_str {
            "BINANCE_FUTURES" => Exchange::BinanceFutures,
            "OKX_SWAP" => Exchange::OkxSwap,
            "OKX_SPOT" => Exchange::OkxSpot,
            "DERIBIT" => Exchange::Deribit,
            _ => {
                return Err(format!(
                    "Invalid exchange '{}'. Supported: BINANCE_FUTURES, OKX_SWAP, OKX_SPOT, DERIBIT",
                    exchange_str
                ));
            }
        };

        Ok(SubscriptionSpec {
            stream_type,
            exchange,
            instrument,
        })
    }

    /// Parse multiple subscriptions from comma-separated string
    pub fn parse_multiple(input: &str) -> Result<Vec<Self>, String> {
        if input.trim().is_empty() {
            return Err("Subscriptions string cannot be empty".to_string());
        }

        let mut subscriptions = Vec::new();
        for sub_str in input.split(',') {
            let trimmed = sub_str.trim();
            if !trimmed.is_empty() {
                subscriptions.push(Self::parse(trimmed)?);
            }
        }

        if subscriptions.is_empty() {
            return Err("No valid subscriptions found".to_string());
        }

        Ok(subscriptions)
    }
}

impl std::fmt::Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamType::L2 => write!(f, "L2"),
            StreamType::Trades => write!(f, "TRADES"),
        }
    }
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate subscriptions format
        let _subscriptions = SubscriptionSpec::parse_multiple(&self.subscriptions)
            .map_err(|e| anyhow::anyhow!("Invalid subscriptions format: {}", e))?;

        // Validate output directory exists or can be created
        if !self.output_directory.exists() {
            // Try to create the directory
            std::fs::create_dir_all(&self.output_directory)
                .map_err(|e| anyhow::anyhow!("Cannot create output directory {}: {}", self.output_directory.display(), e))?;
        } else if !self.output_directory.is_dir() {
            anyhow::bail!("Output path {} is not a directory", self.output_directory.display());
        }

        // Check if directory is writable by attempting to create a test file
        let test_file = self.output_directory.join(".write_test");
        std::fs::write(&test_file, "test")
            .and_then(|_| std::fs::remove_file(&test_file))
            .map_err(|e| anyhow::anyhow!("Output directory {} is not writable: {}", self.output_directory.display(), e))?;

        Ok(())
    }

    

    pub fn get_subscriptions(&self) -> anyhow::Result<Vec<SubscriptionSpec>> {
        let specs = SubscriptionSpec::parse_multiple(&self.subscriptions)
            .map_err(|e| anyhow::anyhow!("Invalid subscriptions: {}", e))?;
        Ok(specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_valid_args() -> Args {
        Args {
            subscriptions: "L2:OKX_SWAP@BTCUSDT".to_string(),
            output_directory: PathBuf::from("test_output"),
            verbose: false,
            shutdown_after: None,
        }
    }

    fn create_multi_subscription_args() -> Args {
        Args {
            subscriptions: "L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT".to_string(),
            output_directory: PathBuf::from("test_output"),
            verbose: false,
            shutdown_after: None,
        }
    }

    #[test]
    fn test_subscription_spec_parse_valid() {
        let spec = SubscriptionSpec::parse("L2:OKX_SWAP@BTCUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::L2));
        assert!(matches!(spec.exchange, Exchange::OkxSwap));
        assert_eq!(spec.instrument, "BTCUSDT");

        let spec = SubscriptionSpec::parse("TRADES:BINANCE_FUTURES@ETHUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::Trades));
        assert!(matches!(spec.exchange, Exchange::BinanceFutures));
        assert_eq!(spec.instrument, "ETHUSDT");

        let spec = SubscriptionSpec::parse("L2:OKX_SPOT@ADAUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::L2));
        assert!(matches!(spec.exchange, Exchange::OkxSpot));
        assert_eq!(spec.instrument, "ADAUSDT");
    }

    #[test]
    fn test_subscription_spec_parse_invalid() {
        // Missing colon
        assert!(SubscriptionSpec::parse("L2_OKX_SWAP@BTCUSDT").is_err());

        // Missing @
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP_BTCUSDT").is_err());

        // Invalid stream type
        assert!(SubscriptionSpec::parse("INVALID:OKX_SWAP@BTCUSDT").is_err());

        // Invalid exchange
        assert!(SubscriptionSpec::parse("L2:INVALID_EXCHANGE@BTCUSDT").is_err());

        // Empty instrument
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP@").is_err());

        // Multiple colons
        assert!(SubscriptionSpec::parse("L2:OKX:SWAP@BTCUSDT").is_err());

        // Multiple @
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP@BTC@USDT").is_err());
    }

    #[test]
    fn test_subscription_spec_parse_multiple() {
        let specs =
            SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT")
                .unwrap();
        assert_eq!(specs.len(), 2);

        assert!(matches!(specs[0].stream_type, StreamType::L2));
        assert!(matches!(specs[0].exchange, Exchange::OkxSwap));
        assert_eq!(specs[0].instrument, "BTCUSDT");

        assert!(matches!(specs[1].stream_type, StreamType::Trades));
        assert!(matches!(specs[1].exchange, Exchange::BinanceFutures));
        assert_eq!(specs[1].instrument, "ETHUSDT");

        // Test with spaces
        let specs =
            SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT, TRADES:BINANCE_FUTURES@ETHUSDT")
                .unwrap();
        assert_eq!(specs.len(), 2);

        // Test single subscription
        let specs = SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT").unwrap();
        assert_eq!(specs.len(), 1);

        // Test empty string
        assert!(SubscriptionSpec::parse_multiple("").is_err());

        // Test with invalid subscription
        assert!(SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT,INVALID").is_err());
    }

    #[test]
    fn test_subscription_validation() {
        let mut args = create_valid_args();

        // Valid subscription should pass
        assert!(args.validate().is_ok());

        // Invalid format should fail
        args.subscriptions = "INVALID_FORMAT".to_string();
        assert!(args.validate().is_err());

        // Invalid stream type should fail
        args.subscriptions = "INVALID:OKX_SWAP@BTCUSDT".to_string();
        assert!(args.validate().is_err());

        // Invalid exchange should fail
        args.subscriptions = "L2:INVALID_EXCHANGE@BTCUSDT".to_string();
        assert!(args.validate().is_err());

        // Multiple valid subscriptions should pass
        args.subscriptions = "L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT".to_string();
        assert!(args.validate().is_ok());

        // Lowercase instruments are now allowed
        args.subscriptions = "L2:OKX_SWAP@btcusdt".to_string();
        assert!(args.validate().is_ok());

        // Non-USDT instruments are now allowed  
        args.subscriptions = "L2:OKX_SWAP@BTCEUR".to_string();
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_output_directory_validation() {
        let args = create_valid_args();
        
        // Valid directory should pass (will be created if needed)
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_get_subscriptions() {
        let args = create_valid_args();
        let subscriptions = args.get_subscriptions().unwrap();
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].instrument, "BTCUSDT");

        let args = create_multi_subscription_args();
        let subscriptions = args.get_subscriptions().unwrap();
        assert_eq!(subscriptions.len(), 2);
    }
}
