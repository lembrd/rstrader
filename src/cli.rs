use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xtrader")]
#[command(about = "High-performance cryptocurrency market data collector")]
#[command(version = "0.1.0")]
pub struct Args {
    #[arg(long, help = "Exchange to connect to (single subscription mode)")]
    pub exchange: Option<Exchange>,
    
    #[arg(long, help = "Trading symbol (e.g., BTCUSDT) (single subscription mode)")]
    pub symbol: Option<String>,
    
    #[arg(long, help = "Output Parquet file path")]
    pub output_parquet: PathBuf,
    
    #[arg(long, help = "Stream type to collect (single subscription mode)")]
    pub stream: Option<StreamType>,
    
    #[arg(long, help = "Multiple subscriptions in format stream_type:exchange@instrument (e.g., L2:OKX_SWAP@BTCUSDT,L2:BINANCE_FUTURES@ETHUSDT)")]
    pub subscriptions: Option<String>,
    
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
            return Err(format!("Invalid format '{}'. Expected 'stream_type:exchange@instrument'", input));
        }
        
        let stream_type_str = parts[0];
        let exchange_instrument = parts[1];
        
        let exchange_parts: Vec<&str> = exchange_instrument.split('@').collect();
        if exchange_parts.len() != 2 {
            return Err(format!("Invalid format '{}'. Expected 'exchange@instrument' after ':'", exchange_instrument));
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
            _ => return Err(format!("Invalid stream type '{}'. Supported: L2, TRADES", stream_type_str)),
        };
        
        // Parse exchange
        let exchange = match exchange_str {
            "BINANCE_FUTURES" => Exchange::BinanceFutures,
            "OKX_SWAP" => Exchange::OkxSwap,
            "OKX_SPOT" => Exchange::OkxSpot,
            _ => return Err(format!("Invalid exchange '{}'. Supported: BINANCE_FUTURES, OKX_SWAP, OKX_SPOT", exchange_str)),
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
        // Check for mutually exclusive modes
        let has_single_args = self.exchange.is_some() || self.symbol.is_some() || self.stream.is_some();
        let has_subscriptions = self.subscriptions.is_some();
        
        if has_single_args && has_subscriptions {
            anyhow::bail!("Cannot use both single subscription arguments (--exchange, --symbol, --stream) and --subscriptions together");
        }
        
        if !has_single_args && !has_subscriptions {
            anyhow::bail!("Must provide either single subscription arguments (--exchange, --symbol, --stream) or --subscriptions");
        }
        
        // Validate single subscription mode
        if has_single_args {
            if self.exchange.is_none() || self.symbol.is_none() || self.stream.is_none() {
                anyhow::bail!("Single subscription mode requires --exchange, --symbol, and --stream arguments");
            }
            
            let symbol = self.symbol.as_ref().unwrap();
            
            // Validate symbol format (should be uppercase and contain USDT)
            if !symbol.chars().all(|c| c.is_ascii_uppercase()) {
                anyhow::bail!("Symbol must be uppercase (e.g., BTCUSDT)");
            }
            
            if !symbol.ends_with("USDT") {
                anyhow::bail!("Only USDT pairs are currently supported");
            }
            
            if symbol.len() < 5 {
                anyhow::bail!("Invalid symbol format");
            }
        }
        
        // Validate multi-subscription mode
        if let Some(subscriptions_str) = &self.subscriptions {
            let _subscriptions = SubscriptionSpec::parse_multiple(subscriptions_str)
                .map_err(|e| anyhow::anyhow!("Invalid subscriptions format: {}", e))?;
            
            // Validate each subscription's symbol format
            for sub in &_subscriptions {
                if !sub.instrument.chars().all(|c| c.is_ascii_uppercase()) {
                    anyhow::bail!("Instrument '{}' must be uppercase (e.g., BTCUSDT)", sub.instrument);
                }
                
                if !sub.instrument.ends_with("USDT") {
                    anyhow::bail!("Only USDT pairs are currently supported, found: {}", sub.instrument);
                }
                
                if sub.instrument.len() < 5 {
                    anyhow::bail!("Invalid instrument format: {}", sub.instrument);
                }
            }
        }
        
        // Validate output path - only check if parent is not current directory
        if let Some(parent) = self.output_parquet.parent() {
            if parent != std::path::Path::new(".") && parent != std::path::Path::new("") && !parent.exists() {
                anyhow::bail!("Output directory does not exist: {}", parent.display());
            }
        }
        
        // Validate file extension
        if let Some(extension) = self.output_parquet.extension() {
            if extension != "parquet" {
                anyhow::bail!("Output file must have .parquet extension");
            }
        } else {
            anyhow::bail!("Output file must have .parquet extension");
        }
        
        Ok(())
    }
    
    /// Helper to determine if running in single subscription mode
    pub fn is_single_subscription_mode(&self) -> bool {
        self.exchange.is_some() && self.symbol.is_some() && self.stream.is_some()
    }
    
    /// Helper to get parsed subscriptions (for multi-subscription mode)
    pub fn get_subscriptions(&self) -> anyhow::Result<Option<Vec<SubscriptionSpec>>> {
        if let Some(subscriptions_str) = &self.subscriptions {
            let specs = SubscriptionSpec::parse_multiple(subscriptions_str)
                .map_err(|e| anyhow::anyhow!("Invalid subscriptions: {}", e))?;
            Ok(Some(specs))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    
    fn create_single_subscription_args() -> Args {
        Args {
            exchange: Some(Exchange::BinanceFutures),
            symbol: Some("BTCUSDT".to_string()),
            output_parquet: PathBuf::from("test.parquet"),
            stream: Some(StreamType::L2),
            subscriptions: None,
            verbose: false,
            shutdown_after: None,
        }
    }
    
    fn create_multi_subscription_args() -> Args {
        Args {
            exchange: None,
            symbol: None,
            output_parquet: PathBuf::from("test.parquet"),
            stream: None,
            subscriptions: Some("L2:OKX_SWAP@BTCUSDT".to_string()),
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
        let specs = SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT").unwrap();
        assert_eq!(specs.len(), 2);
        
        assert!(matches!(specs[0].stream_type, StreamType::L2));
        assert!(matches!(specs[0].exchange, Exchange::OkxSwap));
        assert_eq!(specs[0].instrument, "BTCUSDT");
        
        assert!(matches!(specs[1].stream_type, StreamType::Trades));
        assert!(matches!(specs[1].exchange, Exchange::BinanceFutures));
        assert_eq!(specs[1].instrument, "ETHUSDT");
        
        // Test with spaces
        let specs = SubscriptionSpec::parse_multiple("L2:OKX_SWAP@BTCUSDT, TRADES:BINANCE_FUTURES@ETHUSDT").unwrap();
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
    fn test_single_subscription_validation() {
        let mut args = create_single_subscription_args();
        
        // Valid single subscription should pass
        assert!(args.validate().is_ok());
        assert!(args.is_single_subscription_mode());
        
        // Lowercase symbol should fail
        args.symbol = Some("btcusdt".to_string());
        assert!(args.validate().is_err());
        
        // Non-USDT should fail
        args.symbol = Some("BTCEUR".to_string());
        assert!(args.validate().is_err());
        
        // Too short should fail
        args.symbol = Some("BTC".to_string());
        assert!(args.validate().is_err());
        
        // Missing required fields should fail
        let mut args = create_single_subscription_args();
        args.exchange = None;
        assert!(args.validate().is_err());
        
        args = create_single_subscription_args();
        args.symbol = None;
        assert!(args.validate().is_err());
        
        args = create_single_subscription_args();
        args.stream = None;
        assert!(args.validate().is_err());
    }
    
    #[test]
    fn test_multi_subscription_validation() {
        let mut args = create_multi_subscription_args();
        
        // Valid multi subscription should pass
        assert!(args.validate().is_ok());
        assert!(!args.is_single_subscription_mode());
        
        // Invalid subscription format should fail
        args.subscriptions = Some("INVALID_FORMAT".to_string());
        assert!(args.validate().is_err());
        
        // Lowercase instrument should fail
        args.subscriptions = Some("L2:OKX_SWAP@btcusdt".to_string());
        assert!(args.validate().is_err());
        
        // Non-USDT should fail
        args.subscriptions = Some("L2:OKX_SWAP@BTCEUR".to_string());
        assert!(args.validate().is_err());
        
        // Multiple valid subscriptions should pass
        args.subscriptions = Some("L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT".to_string());
        assert!(args.validate().is_ok());
    }
    
    #[test]
    fn test_mutually_exclusive_modes() {
        let mut args = Args {
            exchange: Some(Exchange::BinanceFutures),
            symbol: Some("BTCUSDT".to_string()),
            output_parquet: PathBuf::from("test.parquet"),
            stream: Some(StreamType::L2),
            subscriptions: Some("L2:OKX_SWAP@BTCUSDT".to_string()),
            verbose: false,
            shutdown_after: None,
        };
        
        // Both modes specified should fail
        assert!(args.validate().is_err());
        
        // No mode specified should fail
        args.exchange = None;
        args.symbol = None;
        args.stream = None;
        args.subscriptions = None;
        assert!(args.validate().is_err());
    }
    
    #[test]
    fn test_output_validation() {
        let mut args = create_single_subscription_args();
        
        // Wrong extension should fail
        args.output_parquet = PathBuf::from("test.txt");
        assert!(args.validate().is_err());
        
        // No extension should fail
        args.output_parquet = PathBuf::from("test");
        assert!(args.validate().is_err());
    }
    
    #[test]
    fn test_get_subscriptions() {
        let args = create_multi_subscription_args();
        let subscriptions = args.get_subscriptions().unwrap().unwrap();
        assert_eq!(subscriptions.len(), 1);
        assert_eq!(subscriptions[0].instrument, "BTCUSDT");
        
        let args = create_single_subscription_args();
        assert!(args.get_subscriptions().unwrap().is_none());
    }
}