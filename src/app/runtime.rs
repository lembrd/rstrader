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
    use crate::strats::api::{StrategyContext, StrategyRunner};


    let config_path = args
        .config
        .as_ref()
        .ok_or_else(|| AppError::cli("--config is required for --strategy"))?;
    let buf = tokio::fs::read_to_string(config_path).await.map_err(|e| AppError::cli(format!("Failed to read config {}: {}", config_path.display(), e)))?;
    // Initialize metrics server first
    let prometheus_exporter = crate::metrics::exporters::PrometheusExporter::new();
    let prom_registry = std::sync::Arc::new(prometheus_exporter);
    let _ = crate::metrics::PROM_EXPORTER.set(prom_registry.clone());
    // Histograms are always enabled in Metrics::new()
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
            let addr = std::env::var("PROM_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:9898".to_string());
            let listener = match TcpListener::bind(&addr).await { Ok(l) => l, Err(_) => return };
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


    // Try NaiveMM strategy
    let cfg: NaiveMmConfig = serde_yaml::from_str(&buf).map_err(|e| AppError::cli(format!("Invalid YAML for known strategies: {}", e)))?;
    let env = super::env::DefaultEnvironment::new(cfg.runtime.channel_capacity, args.verbose, prom_registry.clone());
    let ctx = StrategyContext { env: std::sync::Arc::new(env) };
    let strat = NaiveMm::default();
    StrategyRunner::run(strat, ctx, cfg).await?;
    return Ok(());
}


