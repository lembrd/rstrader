//

use crate::cli::Args;
use crate::xcommons::error::{AppError, Result};

use super::env::{AppEnvironment, DefaultEnvironment, EnvSinkConfig};

pub async fn run_with_env(args: Args) -> Result<()> { run_strategy_path(args).await }

async fn run_strategy_path(args: Args) -> Result<()> {
    use crate::strats::md_collector::config::MdCollectorConfig;
    use crate::strats::md_collector::strategy::MdCollector;
    use crate::strats::api::{Strategy, StrategyContext};
    use std::fs;

    let config_path = args
        .config
        .as_ref()
        .ok_or_else(|| AppError::cli("--config is required for --strategy"))?;
    let buf = fs::read_to_string(config_path).map_err(|e| AppError::cli(format!("Failed to read config {}: {}", config_path.display(), e)))?;
    let cfg: MdCollectorConfig = serde_yaml::from_str(&buf).map_err(|e| AppError::cli(format!("Invalid YAML: {}", e)))?;

    // metrics and exporters + HTTP server
    let prometheus_exporter = crate::metrics::exporters::PrometheusExporter::new();
    let prom_registry = std::sync::Arc::new(prometheus_exporter);
    let _ = crate::metrics::PROM_EXPORTER.set(prom_registry.clone());
    let metrics_ref = crate::metrics::GLOBAL_METRICS.clone();
    {
        let agg = crate::metrics::exporters::Aggregator::new(prom_registry.clone(), std::time::Duration::from_secs(5));
        tokio::spawn(async move { agg.run(&metrics_ref).await });
    }
    tokio::spawn({
        let prom_registry = prom_registry.clone();
        async move {
            use tokio::net::TcpListener;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = match TcpListener::bind("127.0.0.1:9898").await {
                Ok(l) => l,
                Err(e) => { log::warn!("failed to bind metrics listener: {}", e); return; }
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
                                status, body.len(), body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                    Err(e) => { log::warn!("metrics accept error: {}", e); }
                }
            }
        }
    });

    let env = super::env::DefaultEnvironment::new(cfg.runtime.channel_capacity, args.verbose, prom_registry.clone());
    let ctx = StrategyContext { env: std::sync::Arc::new(env) };
    let strat = MdCollector;
    // Configure sink from YAML in strategy mode
    let sink_cfg = match &cfg.sink {
        crate::strats::md_collector::config::SinkSection::Parquet { output_dir } => EnvSinkConfig::Parquet { output_dir: output_dir.clone() },
        crate::strats::md_collector::config::SinkSection::Questdb => EnvSinkConfig::QuestDb,
    };

    // Start subscriptions and sink via strategy impl
    // Re-use strategy start but provide required sink config
    // Strategy will call env.start_subscriptions and env.start_sink; we need to pass sink_cfg through context or use env accessor
    // For simplicity, run here directly using the same mapping as strategy start
    // Map config subscriptions into SubscriptionSpec and run sink
    use crate::cli::{Exchange, StreamType, SubscriptionSpec};
    let subscriptions: Vec<SubscriptionSpec> = cfg
        .subscriptions
        .iter()
        .cloned()
        .map(|s| {
            let exchange = match s.exchange.as_str() {
                "BINANCE_FUTURES" => Exchange::BinanceFutures,
                "OKX_SWAP" => Exchange::OkxSwap,
                "OKX_SPOT" => Exchange::OkxSpot,
                "DERIBIT" => Exchange::Deribit,
                other => return Err(AppError::cli(format!("Unsupported exchange '{}'", other))),
            };
            let stream_type = match s.stream_type.as_str() {
                "L2" => StreamType::L2,
                "TRADES" => StreamType::Trades,
                other => return Err(AppError::cli(format!("Unsupported stream type '{}'", other))),
            };
            let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
            Ok(SubscriptionSpec { stream_type, exchange, instrument: s.instrument, max_connections })
        })
        .collect::<std::result::Result<_, AppError>>()?;

    let rx = ctx.env.start_subscriptions(subscriptions);
    let mut handle = ctx.env.start_sink(sink_cfg, rx)?;
    // Optional shutdown timer
    if let Some(seconds) = args.shutdown_after {
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
        // Stop the sink task gracefully by aborting join handle; subscriptions will end when process exits
        handle.abort();
        log::info!("Shutdown timer elapsed; stopped sink task");
        return Ok(());
    }

    let _ = handle.await.map_err(|e| AppError::internal(format!("sink join error: {}", e)))?;
    Ok(())
}


