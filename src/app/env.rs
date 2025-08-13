//

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::cli::SubscriptionSpec;
use crate::xcommons::error::Result;
use crate::metrics;
use crate::xcommons::types::StreamData;

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
}

#[derive(Clone, Debug)]
pub enum EnvSinkConfig {
    Parquet { output_dir: std::path::PathBuf },
    QuestDb,
}


