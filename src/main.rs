mod cli;
mod error;
mod exchanges;
mod output;
mod pipeline;
mod subscription_manager;
mod types;
mod metrics;

use clap::Parser;
use cli::{Args, Sink};
use error::{AppError, Result};
use futures_util::FutureExt;
use types::StreamData;
// use std::sync::{Arc, Mutex};



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
    use output::{run_multi_stream_parquet_sink, run_multi_stream_questdb_sink};
    use tokio::sync::mpsc;

    // Create communication channel for unified stream data
    let (stream_tx, stream_rx) = mpsc::channel::<StreamData>(10000);

    // Get subscriptions
    let subscriptions = args
        .get_subscriptions()
        .map_err(|e| AppError::cli(format!("Invalid subscriptions: {}", e)))?;

    // Display configuration in verbose mode
    if args.verbose {
        log::info!(
            "Starting xtrader with {} subscriptions (using optimized unified handlers):",
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
        log::info!("  Output: {}", args.output_directory.display());
    }

    // Initialize OKX registries
    let (okx_swap_registry, okx_spot_registry) = 
        subscription_manager::RegistryFactory::initialize_okx_registries(&subscriptions).await?;

    // Create optimized subscription manager with unified handlers
    let mut manager = subscription_manager::SubscriptionManager::new(subscriptions, args.verbose);
    manager
        .spawn_all_subscriptions(stream_tx.clone(), okx_swap_registry, okx_spot_registry)
        .await?;

    // Start sink task based on CLI
    let mut sink_handle = match args.sink {
        Sink::Parquet => {
            let output_directory = args.output_directory.clone();
            tokio::spawn(async move {
                log::info!("Starting Multi-Stream Parquet sink");
                run_multi_stream_parquet_sink(stream_rx, output_directory).await
            })
        }
        Sink::QuestDb => {
            tokio::spawn(async move {
                log::info!("Starting Multi-Stream QuestDB sink");
                run_multi_stream_questdb_sink(stream_rx).await
            })
        }
    };

    // Set up graceful shutdown with optional timer
    let shutdown_future = if let Some(seconds) = args.shutdown_after {
        log::info!("Will shutdown automatically after {} seconds", seconds);
        tokio::time::sleep(tokio::time::Duration::from_secs(seconds)).boxed()
    } else {
        std::future::pending().boxed()
    };

    tokio::select! {
        result = &mut sink_handle => {
            match result {
                Ok(Ok(_)) => log::info!("Sink completed successfully"),
                Ok(Err(e)) => {
                    log::error!("Sink failed: {}", e);
                    manager.shutdown().await;
                    return Err(e);
                }
                Err(e) => {
                    log::error!("Sink task panicked: {}", e);
                    manager.shutdown().await;
                    return Err(AppError::internal(format!("Sink task failed: {}", e)));
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

    // Shutdown handlers and finalize sink
    log::info!("Shutting down unified handlers and finalizing sink...");
    manager.shutdown().await;
    // Drop the last sender so sink can observe channel close and exit
    drop(stream_tx);
    
    match sink_handle.await {
        Ok(Ok(_)) => log::info!("Sink finalized successfully"),
        Ok(Err(e)) => {
            log::error!("Sink failed during finalization: {}", e);
            return Err(e);
        }
        Err(e) => {
            log::error!("Sink task panicked during finalization: {}", e);
            return Err(AppError::internal(format!("Sink task failed: {}", e)));
        }
    }

    log::info!("Shutdown complete - performance optimizations applied");
    Ok(())
}