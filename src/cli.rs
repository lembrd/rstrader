use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "xtrader")]
#[command(about = "High-performance cryptocurrency market data collector")]
pub struct Args {
    #[arg(
        long,
        help = "Subscriptions in format stream_type:exchange@instrument (e.g., L2:OKX_SWAP@BTCUSDT,L2:BINANCE_FUTURES@ETHUSDT)"
    )]
    pub subscriptions: Option<String>,

    #[arg(long, help = "Output directory for Parquet files (used when --sink parquet)", default_value = "./test_output")]
    pub output_directory: PathBuf,

    #[arg(long, help = "Enable verbose output with metrics and statistics")]
    pub verbose: bool,

    #[arg(long, help = "Shutdown after N seconds (for debugging)")]
    pub shutdown_after: Option<u64>,

    #[arg(long, value_enum, default_value_t = Sink::Parquet, help = "Sink to use: parquet or questdb")]
    pub sink: Sink,

    #[arg(long, help = "Run a named strategy instead of the legacy CLI path (e.g., md_collector)")]
    pub strategy: Option<String>,

    #[arg(long, help = "Path to YAML config for --strategy mode")]
    pub config: Option<PathBuf>,
}

#[derive(ValueEnum, Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Exchange {
    #[value(name = "BINANCE_FUTURES")]
    #[serde(rename = "BINANCE_FUTURES")]
    BinanceFutures,
    #[value(name = "OKX_SWAP")]
    #[serde(rename = "OKX_SWAP")]
    OkxSwap,
    #[value(name = "OKX_SPOT")]
    #[serde(rename = "OKX_SPOT")]
    OkxSpot,
    #[value(name = "DERIBIT")]
    #[serde(rename = "DERIBIT")]
    Deribit,
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StreamType {
    #[value(name = "L2")]
    #[serde(rename = "L2")]
    L2,
    #[value(name = "TRADES")]
    #[serde(rename = "TRADES")]
    Trades,
}
// removed duplicate Sink definition (single definition remains above)

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    #[value(name = "parquet")]
    Parquet,
    #[value(name = "questdb")]
    QuestDb,
}
#[derive(Clone, Debug)]
pub struct SubscriptionSpec {
    pub stream_type: StreamType,
    pub exchange: Exchange,
    pub instrument: String,
    /// Optional max concurrent connections for latency arbitrage per subscription
    pub max_connections: Option<usize>,
}

impl SubscriptionSpec {
    /// Parse subscription format: stream_type:exchange@instrument[connections]
    /// Examples:
    ///   L2:OKX_SWAP@BTCUSDT
    ///   L2:BINANCE_FUTURES@BTCUSDT[10]
    pub fn parse(input: &str) -> Result<Self, String> {
        let parts: Vec<&str> = input.split(':').collect();
        if parts.len() != 2 {
            return Err(format!(
                "Invalid format '{}'. Expected 'stream_type:exchange@instrument[connections]'",
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
        let instrument_raw = exchange_parts[1].trim();

        // Support optional [N] suffix for max connections
        let (instrument, max_connections) = if let Some(start_idx) = instrument_raw.rfind('[') {
            if instrument_raw.ends_with(']') {
                let base = &instrument_raw[..start_idx];
                let inside = &instrument_raw[start_idx + 1..instrument_raw.len() - 1];
                if base.is_empty() {
                    return Err("Instrument cannot be empty".to_string());
                }
                let n: usize = inside.parse().map_err(|_| {
                    format!("Invalid connections value '{}' in '{}'", inside, input)
                })?;
                if n == 0 {
                    return Err("Connections value must be > 0".to_string());
                }
                (base.to_string(), Some(n))
            } else {
                (instrument_raw.to_string(), None)
            }
        } else {
            (instrument_raw.to_string(), None)
        };

        if instrument.is_empty() {
            return Err("Instrument cannot be empty".to_string());
        }

        // Parse stream type and exchange using FromStr impls
        let stream_type = <StreamType as std::str::FromStr>::from_str(stream_type_str)
            .map_err(|e| format!("{}", e))?;
        let exchange = <Exchange as std::str::FromStr>::from_str(exchange_str)
            .map_err(|e| format!("{}", e))?;

        Ok(SubscriptionSpec {
            stream_type,
            exchange,
            instrument,
            max_connections,
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

impl FromStr for StreamType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "L2" => Ok(StreamType::L2),
            "TRADES" => Ok(StreamType::Trades),
            other => Err(format!("Invalid stream type '{}'. Supported: L2, TRADES", other)),
        }
    }
}

impl FromStr for Exchange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "BINANCE_FUTURES" => Ok(Exchange::BinanceFutures),
            "OKX_SWAP" => Ok(Exchange::OkxSwap),
            "OKX_SPOT" => Ok(Exchange::OkxSpot),
            "DERIBIT" => Ok(Exchange::Deribit),
            other => Err(format!("Invalid exchange '{}'. Supported: BINANCE_FUTURES, OKX_SWAP, OKX_SPOT, DERIBIT", other)),
        }
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
        // Validate subscriptions format when provided in legacy style
        if let Some(s) = &self.subscriptions {
            let _subscriptions = SubscriptionSpec::parse_multiple(s)
                .map_err(|e| anyhow::anyhow!("Invalid subscriptions format: {}", e))?;
        }

        // If Parquet sink, validate output directory exists or can be created
        if self.sink == Sink::Parquet {
            if !self.output_directory.exists() {
                std::fs::create_dir_all(&self.output_directory).map_err(|e| {
                    anyhow::anyhow!(
                        "Cannot create output directory {}: {}",
                        self.output_directory.display(),
                        e
                    )
                })?;
            } else if !self.output_directory.is_dir() {
                anyhow::bail!(
                    "Output path {} is not a directory",
                    self.output_directory.display()
                );
            }

            // Check if directory is writable using a unique temp file
            let _tmp = tempfile::Builder::new()
                .prefix(".write_test_")
                .tempfile_in(&self.output_directory)
                .map_err(|e| anyhow::anyhow!(
                    "Output directory {} is not writable: {}",
                    self.output_directory.display(),
                    e
                ))?;
        }

        Ok(())
    }

    

    pub fn get_subscriptions(&self) -> anyhow::Result<Vec<SubscriptionSpec>> {
        let s = self.subscriptions.as_deref().unwrap_or("");
        let specs = SubscriptionSpec::parse_multiple(s)
            .map_err(|e| anyhow::anyhow!("Invalid subscriptions: {}", e))?;
        Ok(specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_valid_args() -> Args {
        Args {
            subscriptions: Some("L2:OKX_SWAP@BTCUSDT".to_string()),
            output_directory: PathBuf::from("test_output"),
            verbose: false,
            shutdown_after: None,
            sink: Sink::Parquet,
            strategy: None,
            config: None,
        }
    }

    fn create_multi_subscription_args() -> Args {
        Args {
            subscriptions: Some("L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT".to_string()),
            output_directory: PathBuf::from("test_output"),
            verbose: false,
            shutdown_after: None,
            sink: Sink::Parquet,
            strategy: None,
            config: None,
        }
    }

    #[test]
    fn test_subscription_spec_parse_valid() {
        let spec = SubscriptionSpec::parse("L2:OKX_SWAP@BTCUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::L2));
        assert!(matches!(spec.exchange, Exchange::OkxSwap));
        assert_eq!(spec.instrument, "BTCUSDT");
        assert_eq!(spec.max_connections, None);

        let spec = SubscriptionSpec::parse("TRADES:BINANCE_FUTURES@ETHUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::Trades));
        assert!(matches!(spec.exchange, Exchange::BinanceFutures));
        assert_eq!(spec.instrument, "ETHUSDT");
        assert_eq!(spec.max_connections, None);

        let spec = SubscriptionSpec::parse("L2:OKX_SPOT@ADAUSDT").unwrap();
        assert!(matches!(spec.stream_type, StreamType::L2));
        assert!(matches!(spec.exchange, Exchange::OkxSpot));
        assert_eq!(spec.instrument, "ADAUSDT");

        // With max connections suffix
        let spec = SubscriptionSpec::parse("L2:BINANCE_FUTURES@BTCUSDT[10]").unwrap();
        assert_eq!(spec.instrument, "BTCUSDT");
        assert_eq!(spec.max_connections, Some(10));
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

        // Invalid bracket value
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP@BTCUSDT[abc]").is_err());
        // Zero not allowed
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP@BTCUSDT[0]").is_err());
        // Missing closing bracket treated as plain instrument (allowed)
        assert!(SubscriptionSpec::parse("L2:OKX_SWAP@BTCUSDT[").is_ok());
    }

    #[test]
    fn test_subscription_spec_parse_multiple() {
        let specs = SubscriptionSpec::parse_multiple(
            "L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT[3]",
        )
        .unwrap();
        assert_eq!(specs.len(), 2);

        assert!(matches!(specs[0].stream_type, StreamType::L2));
        assert!(matches!(specs[0].exchange, Exchange::OkxSwap));
        assert_eq!(specs[0].instrument, "BTCUSDT");

        assert!(matches!(specs[1].stream_type, StreamType::Trades));
        assert!(matches!(specs[1].exchange, Exchange::BinanceFutures));
        assert_eq!(specs[1].instrument, "ETHUSDT");
        assert_eq!(specs[1].max_connections, Some(3));

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
        args.subscriptions = Some("INVALID_FORMAT".to_string());
        assert!(args.validate().is_err());

        // Invalid stream type should fail
        args.subscriptions = Some("INVALID:OKX_SWAP@BTCUSDT".to_string());
        assert!(args.validate().is_err());

        // Invalid exchange should fail
        args.subscriptions = Some("L2:INVALID_EXCHANGE@BTCUSDT".to_string());
        assert!(args.validate().is_err());

        // Multiple valid subscriptions should pass
        args.subscriptions = Some("L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT".to_string());
        assert!(args.validate().is_ok());

        // Lowercase instruments are now allowed
        args.subscriptions = Some("L2:OKX_SWAP@btcusdt".to_string());
        assert!(args.validate().is_ok());

        // Non-USDT instruments are now allowed  
        args.subscriptions = Some("L2:OKX_SWAP@BTCEUR".to_string());
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
