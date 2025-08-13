//

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::xcommons::types::SubscriptionSpec;
use crate::xcommons::error::Result;
use crate::metrics;
use crate::xcommons::types::StreamData;
use crate::xcommons::types::ExchangeId;

/// Minimal environment abstraction to gradually align with docs `AppEnvironment`
pub trait AppEnvironment: Send + Sync {
    fn channel_capacity(&self) -> usize;

    fn verbose(&self) -> bool;

    fn prom_registry(&self) -> Arc<metrics::exporters::PrometheusExporter>;

    fn start_subscriptions(
        &self,
        subscriptions: Vec<SubscriptionSpec>,
    ) -> mpsc::Receiver<StreamData>;

    fn start_sink(
        &self,
        config: EnvSinkConfig,
        rx: mpsc::Receiver<StreamData>,
    ) -> Result<tokio::task::JoinHandle<Result<()>>>;

    /// Start Binance Futures account runners for given symbols and configuration.
    /// Returns join handles for each per-symbol runner.
    fn start_binance_futures_accounts(&self, params: BinanceAccountParams) -> Result<Vec<tokio::task::JoinHandle<()>>>;
}

pub struct DefaultEnvironment {
    channel_capacity: usize,
    verbose: bool,
    prom_registry: Arc<metrics::exporters::PrometheusExporter>,
}

impl DefaultEnvironment {
    pub fn new(channel_capacity: usize, verbose: bool, prom_registry: Arc<metrics::exporters::PrometheusExporter>) -> Self { Self { channel_capacity, verbose, prom_registry } }
}

impl AppEnvironment for DefaultEnvironment {
    fn channel_capacity(&self) -> usize { self.channel_capacity }

    fn verbose(&self) -> bool { self.verbose }

    fn prom_registry(&self) -> Arc<metrics::exporters::PrometheusExporter> { self.prom_registry.clone() }

    fn start_subscriptions(&self, subscriptions: Vec<SubscriptionSpec>) -> mpsc::Receiver<StreamData> {
        let (stream_tx, stream_rx) = mpsc::channel::<StreamData>(self.channel_capacity());

        // Initialize OKX registries as per current main.rs
        let verbose = self.verbose();
        tokio::spawn(async move {
            // The registries are created inside runtime; keep this minimal env API
            let (okx_swap_registry, okx_spot_registry) =
                crate::app::subscription_manager::RegistryFactory::initialize_okx_registries(&subscriptions)
                    .await
                    .unwrap_or((None, None));

            let mut manager = crate::app::subscription_manager::SubscriptionManager::new(subscriptions, verbose);
            if let Err(e) = manager
                .spawn_all_subscriptions(
                    stream_tx.clone(),
                    okx_swap_registry,
                    okx_spot_registry,
                )
                .await
            {
                log::error!("Failed to spawn subscriptions: {}", e);
            }
        });

        stream_rx
    }

    fn start_sink(&self, config: EnvSinkConfig, rx: mpsc::Receiver<StreamData>) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let handle = match config {
            EnvSinkConfig::Parquet { output_dir } => {
                tokio::spawn(async move {
                    log::info!("Starting Multi-Stream Parquet sink (env)");
                    crate::output::run_multi_stream_parquet_sink(rx, output_dir).await
                })
            }
            EnvSinkConfig::QuestDb => {
                tokio::spawn(async move {
                    log::info!("Starting Multi-Stream QuestDB sink (env)");
                    crate::output::run_multi_stream_questdb_sink(rx).await
                })
            }
        };
        Ok(handle)
    }

    fn start_binance_futures_accounts(&self, params: BinanceAccountParams) -> Result<Vec<tokio::task::JoinHandle<()>>> {
        use crate::trading::account_state::{AccountState, PostgresExecutionsDatastore};
        use crate::exchanges::binance_account::BinanceFuturesAccountAdapter;
        use crate::xcommons::xmarket_id::XMarketId;

        // Init Postgres pool (use env DATABASE_URL)
        let db_url = std::env::var("DATABASE_URL")
            .map_err(|e| crate::xcommons::error::AppError::config(format!("DATABASE_URL missing: {}", e)))?;
        let cfg_pg: tokio_postgres::Config = db_url
            .parse()
            .map_err(|e| crate::xcommons::error::AppError::config(format!("pg url parse: {}", e)))?;
        let mgr = deadpool_postgres::Manager::from_config(
            cfg_pg,
            tokio_postgres::NoTls,
            deadpool_postgres::ManagerConfig { recycling_method: deadpool_postgres::RecyclingMethod::Fast },
        );
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(16).build().unwrap();

        // Shared adapter
        let adapter = std::sync::Arc::new(BinanceFuturesAccountAdapter::new(
            params.api_key.clone(),
            params.secret.clone(),
            params.symbols.clone(),
        ));

        let mut runners = Vec::new();
        for symbol in params.symbols.iter().cloned() {
            let adapter = adapter.clone();
            let pool = pool.clone();
            let account_id = params.account_id;
            let start_epoch_ts = params.start_epoch_ts;
            let fee_bps = params.fee_bps;
            let contract_size = params.contract_size;
            let market_id = XMarketId::make(ExchangeId::BinanceFutures, &symbol);
            log::info!(
                "[Env] starting binance account runner: symbol={} market_id={} start_epoch_ts={} fee_bps={} contract_size={}",
                symbol, market_id, start_epoch_ts, fee_bps, contract_size
            );
            let handle = tokio::spawn(async move {
                let mut account = AccountState::new(
                    account_id,
                    ExchangeId::BinanceFutures,
                    Box::new(PostgresExecutionsDatastore::new(pool)),
                    adapter,
                );
                // Subscribe to executions and positions and print them
                let mut exec_rx = account.subscribe_executions();
                let mut pos_rx = account.subscribe_positions();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            Ok(exec) = exec_rx.recv() => {
                                log::info!("[Env] Exec market_id={} side={} qty={} px={} id={}", exec.market_id, exec.side as i8, exec.last_qty, exec.last_px, exec.native_execution_id);
                                if log::log_enabled!(log::Level::Debug) {
                                    if !exec.metadata.is_empty() {
                                        if let Ok(json) = serde_json::to_string(&exec) { log::debug!("[Env] Exec JSON: {}", json); }
                                    }
                                }
                            }
                            Ok((mid, pos)) = pos_rx.recv() => {
                                log::info!(
                                    "[Env] Position market_id={} amount={} avp={} realized={} fees={} trades_count={} volume={} bps={}",
                                    mid, pos.amount, pos.avp, pos.realized, pos.fees, pos.trades_count, pos.quote_volume, pos.bps()
                                );
                            }
                            else => break,
                        }
                    }
                });
                if let Err(e) = account.run_market(market_id, start_epoch_ts, fee_bps, contract_size).await {
                    log::error!("[Env] run_market error for {}: {}", symbol, e);
                }
            });
            runners.push(handle);
        }

        Ok(runners)
    }
}

#[derive(Clone, Debug)]
pub enum EnvSinkConfig {
    Parquet { output_dir: std::path::PathBuf },
    QuestDb,
}

#[derive(Clone, Debug)]
pub struct BinanceAccountParams {
    pub api_key: String,
    pub secret: String,
    pub account_id: i64,
    pub start_epoch_ts: i64,
    pub fee_bps: f64,
    pub contract_size: f64,
    pub symbols: Vec<String>,
}


