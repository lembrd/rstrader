mod cli;
mod error;
mod exchanges;
mod output;
mod pipeline;
mod subscription_manager;
mod types;
mod metrics;
pub mod monoseq;
mod oms;

use clap::Parser;
use cli::{Args, Sink};
use error::{AppError, Result};
use futures_util::FutureExt;
use types::StreamData;
// minimal HTTP server for /metrics via tokio TcpListener to avoid heavy deps
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

    // Start metrics HTTP server (/metrics) and snapshot aggregator in background (minimal TCP server)
    let prometheus_exporter = crate::metrics::exporters::PrometheusExporter::new();
    let prom_registry = std::sync::Arc::new(prometheus_exporter);
    let _ = crate::metrics::PROM_EXPORTER.set(prom_registry.clone());
    tokio::spawn({
        let prom_registry = prom_registry.clone();
        async move {
            use tokio::net::TcpListener;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = match TcpListener::bind("127.0.0.1:9898").await {
                Ok(l) => l,
                Err(e) => {
                    log::warn!("failed to bind metrics listener: {}", e);
                    return;
                }
            };
            loop {
                match listener.accept().await {
                    Ok((mut socket, _)) => {
                        let prom_registry = prom_registry.clone();
                        tokio::spawn(async move {
                            let mut buf = [0u8; 1024];
                            let _ = socket.read(&mut buf).await;
                            let req = std::str::from_utf8(&buf).unwrap_or("");
                            let is_metrics = req.starts_with("GET /metrics");
                            let body = if is_metrics { prom_registry.gather() } else { String::from("Not Found") };
                            let status = if is_metrics { "200 OK" } else { "404 Not Found" };
                            let response = format!(
                                "HTTP/1.1 {}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                status,
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                    Err(e) => {
                        log::warn!("metrics accept error: {}", e);
                    }
                }
            }
        }
    });

    // Aggregators (Prometheus + optional StatsD)
    let metrics_ref = crate::metrics::GLOBAL_METRICS.clone();
    {
        let agg = crate::metrics::exporters::Aggregator::new(prom_registry.clone(), std::time::Duration::from_secs(5));
        tokio::spawn(async move { agg.run(&metrics_ref).await });
    }
    // StatsD exporter (optional): env XTRADER_STATSD_HOST, XTRADER_STATSD_PORT
    if let (Ok(host), Ok(port_s)) = (std::env::var("XTRADER_STATSD_HOST"), std::env::var("XTRADER_STATSD_PORT")) {
        if let Ok(port) = port_s.parse::<u16>() {
            let exporter = crate::metrics::exporters::StatsdExporter::new("xtrader", &host, port);
        let metrics_ref = crate::metrics::GLOBAL_METRICS.clone();
        let agg = crate::metrics::exporters::Aggregator::new(exporter, std::time::Duration::from_secs(5));
        tokio::spawn(async move { agg.run(&metrics_ref).await });
        }
    }

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