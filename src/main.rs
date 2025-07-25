mod cli;
mod types;
mod error;
mod exchanges;
mod pipeline;
mod output;

use clap::Parser;
use cli::Args;
use error::{AppError, Result};
use futures_util::FutureExt;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize TLS crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| AppError::fatal("Failed to install TLS crypto provider"))?;
    
    // Initialize logging
    env_logger::init();
    
    // Parse command line arguments
    let args = Args::parse();
    
    // Validate arguments
    if let Err(e) = args.validate() {
        return Err(AppError::cli(format!("Invalid arguments: {}", e)));
    }
    
    // Display configuration in verbose mode
    if args.verbose {
        log::info!("Starting xtrader with configuration:");
        log::info!("  Exchange: {:?}", args.exchange);
        log::info!("  Symbol: {}", args.symbol);
        log::info!("  Stream: {:?}", args.stream);
        log::info!("  Output: {}", args.output_parquet.display());
        log::info!("  Verbose: {}", args.verbose);
    }
    
    // Initialize and run the application
    match run_application(args).await {
        Ok(_) => {
            log::info!("Application completed successfully");
            Ok(())
        }
        Err(e) => {
            log::error!("Application failed: {}", e);
            Err(e)
        }
    }
}

async fn run_application(args: Args) -> Result<()> {
    use exchanges::binance::BinanceFuturesConnector;
    use exchanges::ExchangeConnector;
    use pipeline::processor::run_stream_processor;
    use output::parquet::run_parquet_sink;
    use tokio::sync::mpsc;
    use cli::{Exchange, StreamType};

    // Create communication channels
    let (raw_tx, raw_rx) = mpsc::channel(10000);
    let (processed_tx, processed_rx) = mpsc::channel(10000);

    // Initialize exchange connector based on CLI args
    let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match args.exchange {
        Exchange::BinanceFutures => {
            log::info!("Initializing Binance Futures connector");
            Box::new(BinanceFuturesConnector::new())
        }
        Exchange::OkxSwap => {
            log::info!("Initializing OKX SWAP connector");
            Box::new(exchanges::okx::OkxConnector::new_swap())
        }
        Exchange::OkxSpot => {
            log::info!("Initializing OKX SPOT connector");
            Box::new(exchanges::okx::OkxConnector::new_spot())
        }
    };

    // Connect to the exchange
    log::info!("Connecting to exchange...");
    connector.connect().await
        .map_err(|e| AppError::connection(format!("Failed to connect: {}", e)))?;

    // Validate symbol exists
    log::info!("Validating symbol: {}", args.symbol);
    connector.validate_symbol(&args.symbol).await
        .map_err(|e| AppError::validation(format!("Invalid symbol {}: {}", args.symbol, e)))?;

    // Get initial order book snapshot if L2 stream
    if matches!(args.stream, StreamType::L2) {
        log::info!("Getting initial order book snapshot for {}", args.symbol);
        let _snapshot = connector.get_snapshot(&args.symbol).await
            .map_err(|e| AppError::data(format!("Failed to get snapshot: {}", e)))?;
        log::info!("Successfully retrieved order book snapshot");
    }

    // Start stream processor task
    let processor_handle = {
        let symbol = args.symbol.clone();
        let verbose = args.verbose;
        tokio::spawn(async move {
            log::info!("Starting stream processor");
            run_stream_processor(raw_rx, processed_tx, symbol, verbose).await
        })
    };

    // Start Parquet sink task
    let sink_handle = {
        let output_path = args.output_parquet.clone();
        tokio::spawn(async move {
            log::info!("Starting Parquet sink");
            run_parquet_sink(processed_rx, output_path).await
        })
    };

    // Start market data stream based on type
    let stream_handle = {
        let symbol = args.symbol.clone();
        let stream_type = args.stream.clone();
        tokio::spawn(async move {
            log::info!("Starting {} stream for {}", stream_type, symbol);
            
            match stream_type {
                StreamType::L2 => {
                    connector.start_depth_stream(&symbol, raw_tx).await
                        .map_err(|e| AppError::stream(format!("L2 stream failed: {}", e)))
                }
                StreamType::Trades => {
                    connector.start_trade_stream(&symbol, raw_tx).await
                        .map_err(|e| AppError::stream(format!("Trade stream failed: {}", e)))
                }
            }
        })
    };

    // Set up graceful shutdown with optional timer
    let shutdown_future = if let Some(seconds) = args.shutdown_after {
        log::info!("Will shutdown automatically after {} seconds", seconds);
        tokio::time::sleep(tokio::time::Duration::from_secs(seconds)).boxed()
    } else {
        // Create a future that never completes
        std::future::pending().boxed()
    };
    
    tokio::select! {
        result = stream_handle => {
            match result {
                Ok(Ok(_)) => log::info!("Stream completed successfully"),
                Ok(Err(e)) => {
                    log::error!("Stream failed: {}", e);
                    return Err(e);
                }
                Err(e) => {
                    log::error!("Stream task panicked: {}", e);
                    return Err(AppError::internal(format!("Stream task failed: {}", e)));
                }
            }
        }
        result = processor_handle => {
            match result {
                Ok(Ok(_)) => log::info!("Processor completed successfully"),
                Ok(Err(e)) => {
                    log::error!("Processor failed: {}", e);
                    return Err(e);
                }
                Err(e) => {
                    log::error!("Processor task panicked: {}", e);
                    return Err(AppError::internal(format!("Processor task failed: {}", e)));
                }
            }
        }
        result = sink_handle => {
            match result {
                Ok(Ok(_)) => log::info!("Parquet sink completed successfully"),
                Ok(Err(e)) => {
                    log::error!("Parquet sink failed: {}", e);
                    return Err(e);
                }
                Err(e) => {
                    log::error!("Parquet sink task panicked: {}", e);
                    return Err(AppError::internal(format!("Parquet sink task failed: {}", e)));
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received shutdown signal, gracefully shutting down...");
        }
        _ = shutdown_future => {
            log::info!("Shutdown timer expired, gracefully shutting down...");
        }
    }

    log::info!("Shutdown complete");
    Ok(())
}