mod cli;
mod error;
mod exchanges;
mod output;
mod pipeline;
mod types;

use clap::Parser;
use cli::Args;
use cli::SubscriptionSpec;
use error::{AppError, Result};
use exchanges::ExchangeConnector;
use futures_util::FutureExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use types::{OrderBookL2Update, RawMessage};

/// Manager for handling multiple concurrent subscriptions
pub struct MultiSubscriptionManager {
    subscriptions: Vec<SubscriptionSpec>,
    connector_handles: Vec<JoinHandle<Result<()>>>,
}

impl MultiSubscriptionManager {
    pub fn new(subscriptions: Vec<SubscriptionSpec>) -> Self {
        Self {
            subscriptions,
            connector_handles: Vec::new(),
        }
    }

    /// Spawn per-subscription connector and processor pairs
    pub async fn spawn_all_subscriptions(
        &mut self,
        processed_tx: mpsc::Sender<OrderBookL2Update>,
        verbose: bool,
    ) -> Result<()> {
        use cli::{Exchange, StreamType};
        use exchanges::binance::BinanceFuturesConnector;
        use exchanges::okx::{InstrumentRegistry, OkxConnector};
        use pipeline::processor::run_stream_processor;
        use reqwest::Client;
        use std::sync::Arc;

        // Initialize singleton registries for OKX exchanges once
        let mut okx_swap_registry: Option<Arc<InstrumentRegistry>> = None;
        let mut okx_spot_registry: Option<Arc<InstrumentRegistry>> = None;

        // Pre-initialize registries based on subscriptions
        let needs_swap = self
            .subscriptions
            .iter()
            .any(|s| matches!(s.exchange, Exchange::OkxSwap));
        let needs_spot = self
            .subscriptions
            .iter()
            .any(|s| matches!(s.exchange, Exchange::OkxSpot));

        if needs_swap {
            log::info!("Initializing OKX SWAP instrument registry");
            let client = Client::new();
            let registry = InstrumentRegistry::initialize_with_instruments(
                &client,
                "https://www.okx.com",
                "SWAP",
            )
            .await
            .map_err(|e| {
                AppError::connection(format!("Failed to initialize SWAP registry: {}", e))
            })?;
            okx_swap_registry = Some(Arc::new(registry));
        }

        if needs_spot {
            log::info!("Initializing OKX SPOT instrument registry");
            let client = Client::new();
            let registry = InstrumentRegistry::initialize_with_instruments(
                &client,
                "https://www.okx.com",
                "SPOT",
            )
            .await
            .map_err(|e| {
                AppError::connection(format!("Failed to initialize SPOT registry: {}", e))
            })?;
            okx_spot_registry = Some(Arc::new(registry));
        }

        for (index, subscription) in self.subscriptions.iter().enumerate() {
            // Create dedicated channels for this subscription
            let (raw_tx, raw_rx) = mpsc::channel(10000);

            // Create connector based on exchange
            let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match subscription
                .exchange
            {
                Exchange::BinanceFutures => {
                    log::info!(
                        "[{}] Creating Binance Futures connector for {}",
                        index,
                        subscription.instrument
                    );
                    Box::new(BinanceFuturesConnector::new())
                }
                Exchange::OkxSwap => {
                    log::info!(
                        "[{}] Creating OKX SWAP connector for {}",
                        index,
                        subscription.instrument
                    );
                    Box::new(OkxConnector::new_swap(
                        okx_swap_registry.as_ref().unwrap().clone(),
                    ))
                }
                Exchange::OkxSpot => {
                    log::info!(
                        "[{}] Creating OKX SPOT connector for {}",
                        index,
                        subscription.instrument
                    );
                    Box::new(OkxConnector::new_spot(
                        okx_spot_registry.as_ref().unwrap().clone(),
                    ))
                }
                Exchange::Deribit => {
                    log::info!(
                        "[{}] Creating Deribit connector for {}",
                        index,
                        subscription.instrument
                    );
                    Box::new(exchanges::deribit::DeribitConnector::new(
                        exchanges::deribit::DeribitConfig::from_env().map_err(|e| {
                            AppError::connection(format!("Failed to load Deribit config: {}", e))
                        })?,
                    ))
                }
            };

            // Connect to the exchange
            log::info!(
                "[{}] Connecting to {:?} for {}...",
                index,
                subscription.exchange,
                subscription.instrument
            );
            connector.connect().await.map_err(|e| {
                AppError::connection(format!(
                    "Failed to connect to {:?}: {}",
                    subscription.exchange, e
                ))
            })?;

            // Validate symbol exists
            log::info!("[{}] Validating symbol: {}", index, subscription.instrument);
            connector
                .validate_symbol(&subscription.instrument)
                .await
                .map_err(|e| {
                    AppError::validation(format!(
                        "Invalid symbol {} on {:?}: {}",
                        subscription.instrument, subscription.exchange, e
                    ))
                })?;

            // Get initial order book snapshot if L2 stream
            if matches!(subscription.stream_type, StreamType::L2) {
                log::info!(
                    "[{}] Getting initial order book snapshot for {} on {:?}",
                    index,
                    subscription.instrument,
                    subscription.exchange
                );
                let _snapshot = connector
                    .get_snapshot(&subscription.instrument)
                    .await
                    .map_err(|e| {
                        AppError::data(format!(
                            "Failed to get snapshot for {} on {:?}: {}",
                            subscription.instrument, subscription.exchange, e
                        ))
                    })?;
                log::info!(
                    "[{}] Successfully retrieved order book snapshot for {} on {:?}",
                    index,
                    subscription.instrument,
                    subscription.exchange
                );
            }

            // Spawn dedicated StreamProcessor for this subscription
            let processor_tx = processed_tx.clone();
            let processor_symbol = subscription.instrument.clone();
            let processor_index = index;
            let processor_exchange = subscription.exchange.clone();

            // Get appropriate OKX registry for this exchange (clone outside async block)
            let processor_registry = match subscription.exchange {
                Exchange::OkxSwap => okx_swap_registry.as_ref().map(|r| r.clone()),
                Exchange::OkxSpot => okx_spot_registry.as_ref().map(|r| r.clone()),
                _ => None,
            };

            let processor_handle = tokio::spawn(async move {
                log::info!(
                    "[{}] Starting StreamProcessor for {:?}@{}",
                    processor_index,
                    processor_exchange,
                    processor_symbol
                );
                // Registry was already cloned outside the closure
                // processor_registry is captured from outer scope
                run_stream_processor(
                    raw_rx,
                    processor_tx,
                    processor_symbol,
                    verbose,
                    processor_registry,
                )
                .await
            });

            self.connector_handles.push(processor_handle);

            // Spawn connector task
            let connector_symbol = subscription.instrument.clone();
            let stream_type = subscription.stream_type.clone();
            let exchange = subscription.exchange.clone();
            let connector_index = index;

            let connector_handle = tokio::spawn(async move {
                log::info!(
                    "[{}] Starting {:?} stream for {} on {:?}",
                    connector_index,
                    stream_type,
                    connector_symbol,
                    exchange
                );

                match stream_type {
                    StreamType::L2 => connector
                        .start_depth_stream(&connector_symbol, raw_tx)
                        .await
                        .map_err(|e| {
                            AppError::stream(format!(
                                "L2 stream failed for {} on {:?}: {}",
                                connector_symbol, exchange, e
                            ))
                        }),
                    StreamType::Trades => connector
                        .start_trade_stream(&connector_symbol, raw_tx)
                        .await
                        .map_err(|e| {
                            AppError::stream(format!(
                                "Trade stream failed for {} on {:?}: {}",
                                connector_symbol, exchange, e
                            ))
                        }),
                }
            });

            self.connector_handles.push(connector_handle);
        }

        log::info!(
            "Successfully spawned {} subscription pairs (connector + processor)",
            self.subscriptions.len()
        );
        Ok(())
    }

    /// Wait for any subscription component to complete (usually due to error or shutdown)
    pub async fn wait_for_any_completion(&mut self) -> Result<()> {
        if self.connector_handles.is_empty() {
            return Ok(());
        }

        // Wait for first component to complete
        let (result, _index, remaining) =
            futures_util::future::select_all(self.connector_handles.drain(..)).await;

        // Store remaining handles back
        self.connector_handles = remaining;

        match result {
            Ok(Ok(_)) => {
                log::info!("One subscription component completed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                log::error!("Subscription component failed: {}", e);
                Err(e)
            }
            Err(e) => {
                log::error!("Subscription component task panicked: {}", e);
                Err(AppError::internal(format!(
                    "Subscription component task failed: {}",
                    e
                )))
            }
        }
    }

    /// Gracefully shutdown all subscription components
    pub async fn shutdown(&mut self) {
        log::info!(
            "Shutting down {} subscription component tasks",
            self.connector_handles.len()
        );

        for handle in self.connector_handles.drain(..) {
            handle.abort();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if it exists (ignore errors if file doesn't exist)
    if let Err(e) = dotenv::dotenv() {
        // Only log if it's not a "not found" error
        if !e.to_string().contains("not found") {
            log::warn!("Failed to load .env file: {}", e);
        }
    }

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

    // Configuration logging is now handled in each mode separately

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
    use cli::{Exchange, StreamType};
    use exchanges::ExchangeConnector;
    use exchanges::binance::BinanceFuturesConnector;
    use output::parquet::run_parquet_sink;
    use pipeline::processor::run_stream_processor;
    use tokio::sync::mpsc;

    // Create communication channels
    let (raw_tx, raw_rx) = mpsc::channel(10000);
    let (processed_tx, processed_rx) = mpsc::channel(10000);

    // Determine mode and start appropriate stream(s)
    if args.is_single_subscription_mode() {
        // Single subscription mode (backward compatibility)
        let exchange = args.exchange.unwrap();
        let symbol = args.symbol.unwrap();
        let stream = args.stream.unwrap();

        // Display configuration in verbose mode
        if args.verbose {
            log::info!("Starting xtrader with single subscription:");
            log::info!("  Exchange: {:?}", exchange);
            log::info!("  Symbol: {}", symbol);
            log::info!("  Stream: {:?}", stream);
            log::info!("  Output: {}", args.output_parquet.display());
        }

        // Initialize singleton registries for OKX exchanges if needed
        let okx_registry = match exchange {
            Exchange::OkxSwap => {
                log::info!("Initializing OKX SWAP instrument registry");
                let client = reqwest::Client::new();
                let registry = exchanges::okx::InstrumentRegistry::initialize_with_instruments(
                    &client,
                    "https://www.okx.com",
                    "SWAP",
                )
                .await
                .map_err(|e| {
                    AppError::connection(format!("Failed to initialize SWAP registry: {}", e))
                })?;
                Some(std::sync::Arc::new(registry))
            }
            Exchange::OkxSpot => {
                log::info!("Initializing OKX SPOT instrument registry");
                let client = reqwest::Client::new();
                let registry = exchanges::okx::InstrumentRegistry::initialize_with_instruments(
                    &client,
                    "https://www.okx.com",
                    "SPOT",
                )
                .await
                .map_err(|e| {
                    AppError::connection(format!("Failed to initialize SPOT registry: {}", e))
                })?;
                Some(std::sync::Arc::new(registry))
            }
            Exchange::BinanceFutures | Exchange::Deribit => None,
        };

        // Initialize exchange connector based on CLI args
        let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match exchange {
            Exchange::BinanceFutures => {
                log::info!("Initializing Binance Futures connector");
                Box::new(BinanceFuturesConnector::new())
            }
            Exchange::OkxSwap => {
                log::info!("Initializing OKX SWAP connector");
                Box::new(exchanges::okx::OkxConnector::new_swap(
                    okx_registry.as_ref().unwrap().clone(),
                ))
            }
            Exchange::OkxSpot => {
                log::info!("Initializing OKX SPOT connector");
                Box::new(exchanges::okx::OkxConnector::new_spot(
                    okx_registry.as_ref().unwrap().clone(),
                ))
            }
            Exchange::Deribit => {
                log::info!("Initializing Deribit connector");
                Box::new(exchanges::deribit::DeribitConnector::new(
                    exchanges::deribit::DeribitConfig::from_env().map_err(|e| {
                        AppError::connection(format!("Failed to load Deribit config: {}", e))
                    })?,
                ))
            }
        };

        // Connect to the exchange
        log::info!("Connecting to exchange...");
        connector
            .connect()
            .await
            .map_err(|e| AppError::connection(format!("Failed to connect: {}", e)))?;

        // Validate symbol exists
        log::info!("Validating symbol: {}", symbol);
        connector
            .validate_symbol(&symbol)
            .await
            .map_err(|e| AppError::validation(format!("Invalid symbol {}: {}", symbol, e)))?;

        // Get initial order book snapshot if L2 stream
        if matches!(stream, StreamType::L2) {
            log::info!("Getting initial order book snapshot for {}", symbol);
            let _snapshot = connector
                .get_snapshot(&symbol)
                .await
                .map_err(|e| AppError::data(format!("Failed to get snapshot: {}", e)))?;
            log::info!("Successfully retrieved order book snapshot");
        }

        // Start market data stream based on type
        let stream_handle = {
            let symbol_clone = symbol.clone();
            let stream_type = stream.clone();
            tokio::spawn(async move {
                log::info!("Starting {} stream for {}", stream_type, symbol_clone);

                match stream_type {
                    StreamType::L2 => connector
                        .start_depth_stream(&symbol_clone, raw_tx)
                        .await
                        .map_err(|e| AppError::stream(format!("L2 stream failed: {}", e))),
                    StreamType::Trades => connector
                        .start_trade_stream(&symbol_clone, raw_tx)
                        .await
                        .map_err(|e| AppError::stream(format!("Trade stream failed: {}", e))),
                }
            })
        };

        // Start stream processor task
        let processor_handle = {
            let symbol_clone = symbol.clone();
            let verbose = args.verbose;
            tokio::spawn(async move {
                log::info!("Starting stream processor");
                run_stream_processor(
                    raw_rx,
                    processed_tx,
                    symbol_clone,
                    verbose,
                    okx_registry.clone(),
                )
                .await
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
            _ = tokio::signal::ctrl_c() => {
                log::info!("Received shutdown signal, gracefully shutting down...");
            }
            _ = shutdown_future => {
                log::info!("Shutdown timer expired, gracefully shutting down...");
            }
        }

        // Ensure Parquet sink completes before exiting
        log::info!("Shutdown triggered. Finalizing Parquet sink...");
        match sink_handle.await {
            Ok(Ok(_)) => log::info!("Parquet sink finalized successfully"),
            Ok(Err(e)) => {
                log::error!("Parquet sink failed during finalization: {}", e);
                return Err(e);
            }
            Err(e) => {
                log::error!("Parquet sink task panicked during finalization: {}", e);
                return Err(AppError::internal(format!("Parquet sink task failed: {}", e)));
            }
        }
    } else {
        // Multi-subscription mode
        let subscriptions = args
            .get_subscriptions()
            .map_err(|e| AppError::cli(format!("Invalid subscriptions: {}", e)))?
            .unwrap();

        // Display configuration in verbose mode
        if args.verbose {
            log::info!(
                "Starting xtrader with {} subscriptions:",
                subscriptions.len()
            );
            for (i, sub) in subscriptions.iter().enumerate() {
                log::info!(
                    "  [{}] {:?}:{:?}@{}",
                    i + 1,
                    sub.stream_type,
                    sub.exchange,
                    sub.instrument
                );
            }
            log::info!("  Output: {}", args.output_parquet.display());
        }

        // Create and initialize multi-subscription manager
        let mut manager = MultiSubscriptionManager::new(subscriptions);
        manager
            .spawn_all_subscriptions(processed_tx, args.verbose)
            .await?;

        // Start Parquet sink task
        let sink_handle = {
            let output_path = args.output_parquet.clone();
            tokio::spawn(async move {
                log::info!("Starting Parquet sink");
                run_parquet_sink(processed_rx, output_path).await
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
            result = manager.wait_for_any_completion() => {
                match result {
                    Ok(_) => log::info!("One connector completed successfully"),
                    Err(e) => {
                        log::error!("Connector failed: {}", e);
                        manager.shutdown().await;
                        return Err(e);
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

        // Shutdown connectors and finalize Parquet sink
        log::info!("Shutting down connectors and finalizing Parquet sink...");
        manager.shutdown().await;
        match sink_handle.await {
            Ok(Ok(_)) => log::info!("Parquet sink finalized successfully"),
            Ok(Err(e)) => {
                log::error!("Parquet sink failed during finalization: {}", e);
                return Err(e);
            }
            Err(e) => {
                log::error!("Parquet sink task panicked during finalization: {}", e);
                return Err(AppError::internal(format!("Parquet sink task failed: {}", e)));
            }
        }
    }

    log::info!("Shutdown complete");
    Ok(())
}
