//

use crate::cli::Args;
use crate::xcommons::error::{AppError, Result};

use super::env::{EnvSinkConfig};

pub async fn run_with_env(args: Args) -> Result<()> { run_strategy_path(args).await }

async fn run_strategy_path(args: Args) -> Result<()> {
    use crate::strats::md_collector::config::MdCollectorConfig;
    // use crate::strats::md_collector::strategy::MdCollector;
    use crate::strats::naive_mm::config::NaiveMmConfig;
    use crate::strats::naive_mm::strategy::NaiveMm;
    use crate::strats::api::{Strategy, StrategyContext};


    let config_path = args
        .config
        .as_ref()
        .ok_or_else(|| AppError::cli("--config is required for --strategy"))?;
    let buf = tokio::fs::read_to_string(config_path).await.map_err(|e| AppError::cli(format!("Failed to read config {}: {}", config_path.display(), e)))?;
    // Initialize metrics server first
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
            use std::sync::Arc;
            use std::sync::atomic::{AtomicUsize, Ordering};
            let listener = match TcpListener::bind("127.0.0.1:9898").await { Ok(l) => l, Err(_) => return };
            let active = Arc::new(AtomicUsize::new(0));
            const MAX_CONN: usize = 10;
            loop {
                if let Ok((mut socket, _addr)) = listener.accept().await {
                    let cur = active.fetch_add(1, Ordering::AcqRel);
                    if cur >= MAX_CONN { active.fetch_sub(1, Ordering::AcqRel); continue; }
                    let prom_registry = prom_registry.clone();
                    let active = active.clone();
                    tokio::spawn(async move {
                        let _guard = scopeguard::guard((), |_| { active.fetch_sub(1, Ordering::AcqRel); });
                        let mut buf = [0u8; 1024];
                        if socket.read(&mut buf).await.is_ok() {
                            let req = std::str::from_utf8(&buf).unwrap_or("");
                            let is_metrics = req.starts_with("GET /metrics");
                            let body = if is_metrics { prom_registry.gather() } else { "Not Found".to_string() };
                            let response = if is_metrics {
                                format!("HTTP/1.1 200 OK\nContent-Type: text/plain; version=0.0.4\nContent-Length: {}\nConnection: close\n\n{}", body.len(), body)
                            } else {
                                "HTTP/1.1 404 Not Found\nContent-Type: text/plain\nContent-Length: 9\nConnection: close\n\nNot Found".to_string()
                            };
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                        }
                    });
                }
            }
        }
    });

    // Try MD Collector first; if it fails, try NaiveMM
    let md_cfg: std::result::Result<MdCollectorConfig, serde_yaml::Error> = serde_yaml::from_str(&buf);
    if let Ok(cfg) = md_cfg {
        // existing MD collector path
        // metrics and exporters setup is above
        let env = super::env::DefaultEnvironment::new(cfg.runtime.channel_capacity, args.verbose, prom_registry.clone());
        let ctx = StrategyContext { env: std::sync::Arc::new(env) };
        // Configure sink from YAML in strategy mode
        let sink_cfg = match &cfg.sink {
            crate::strats::md_collector::config::SinkSection::Parquet { output_dir } => EnvSinkConfig::Parquet { output_dir: output_dir.clone() },
            crate::strats::md_collector::config::SinkSection::Questdb => EnvSinkConfig::QuestDb,
        };
        // Map config subscriptions into SubscriptionSpec and run sink (now using typed enums from YAML)
        use crate::cli::SubscriptionSpec;
        let subscriptions: Vec<SubscriptionSpec> = cfg
            .subscriptions
            .into_iter()
            .map(|s| {
                let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
                Ok(SubscriptionSpec { stream_type: s.stream_type, exchange: s.exchange, instrument: s.instrument, max_connections })
            })
            .collect::<std::result::Result<_, AppError>>()?;

        let rx = ctx.env.start_subscriptions(subscriptions);
        let handle = ctx.env.start_sink(sink_cfg, rx)?;
        if let Some(seconds) = args.shutdown_after {
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            handle.abort();
            log::info!("Shutdown timer elapsed; stopped sink task");
            return Ok(());
        }
        let _ = handle.await.map_err(|e| AppError::internal(format!("sink join error: {}", e)))?;
        return Ok(());
    }

    // Try NaiveMM strategy
    let cfg: NaiveMmConfig = serde_yaml::from_str(&buf).map_err(|e| AppError::cli(format!("Invalid YAML for known strategies: {}", e)))?;
    let env = super::env::DefaultEnvironment::new(cfg.runtime.channel_capacity, args.verbose, prom_registry.clone());
    let ctx = StrategyContext { env: std::sync::Arc::new(env) };
    let strat = NaiveMm;
    strat.start(ctx, cfg).await?;
    return Ok(());
}


