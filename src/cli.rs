use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xtrader")]
#[command(about = "High-performance cryptocurrency market data collector")]
#[command(version = "0.1.0")]
pub struct Args {
    #[arg(long, help = "Exchange to connect to")]
    pub exchange: Exchange,
    
    #[arg(long, help = "Trading symbol (e.g., BTCUSDT)")]
    pub symbol: String,
    
    #[arg(long, help = "Output Parquet file path")]
    pub output_parquet: PathBuf,
    
    #[arg(long, help = "Stream type to collect")]
    pub stream: StreamType,
    
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
        // Validate symbol format (should be uppercase and contain USDT)
        if !self.symbol.chars().all(|c| c.is_ascii_uppercase()) {
            anyhow::bail!("Symbol must be uppercase (e.g., BTCUSDT)");
        }
        
        if !self.symbol.ends_with("USDT") {
            anyhow::bail!("Only USDT pairs are currently supported");
        }
        
        if self.symbol.len() < 5 {
            anyhow::bail!("Invalid symbol format");
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    
    #[test]
    fn test_symbol_validation() {
        let mut args = Args {
            exchange: Exchange::BinanceFutures,
            symbol: "BTCUSDT".to_string(),
            output_parquet: PathBuf::from("test.parquet"),
            stream: StreamType::L2,
            verbose: false,
            shutdown_after: None,
        };
        
        // Valid symbol should pass
        assert!(args.validate().is_ok());
        
        // Lowercase should fail
        args.symbol = "btcusdt".to_string();
        assert!(args.validate().is_err());
        
        // Non-USDT should fail
        args.symbol = "BTCEUR".to_string();
        assert!(args.validate().is_err());
        
        // Too short should fail
        args.symbol = "BTC".to_string();
        assert!(args.validate().is_err());
    }
    
    #[test]
    fn test_output_validation() {
        let mut args = Args {
            exchange: Exchange::BinanceFutures,
            symbol: "BTCUSDT".to_string(),
            output_parquet: PathBuf::from("test.parquet"),
            stream: StreamType::L2,
            verbose: false,
            shutdown_after: None,
        };
        
        // Wrong extension should fail
        args.output_parquet = PathBuf::from("test.txt");
        assert!(args.validate().is_err());
        
        // No extension should fail
        args.output_parquet = PathBuf::from("test");
        assert!(args.validate().is_err());
    }
}