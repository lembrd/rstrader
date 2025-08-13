use async_trait::async_trait;

use crate::app::env::EnvSinkConfig;
use crate::cli::SubscriptionSpec;
use crate::xcommons::error::Result as AppResult;
use crate::strats::api::{Strategy, StrategyContext};

use super::config::{MdCollectorConfig, SinkSection};

pub struct MdCollector;

#[async_trait]
impl Strategy for MdCollector {
    type Config = MdCollectorConfig;

    fn name(&self) -> &'static str { "md_collector" }

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()> {
        // Map config subscriptions into SubscriptionSpec using typed enums
        let subscriptions: Vec<SubscriptionSpec> = cfg
            .subscriptions
            .into_iter()
            .map(|s| {
                let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
                SubscriptionSpec { stream_type: s.stream_type, exchange: s.exchange, instrument: s.instrument, max_connections }
            })
            .collect();

        let rx = ctx.env.start_subscriptions(subscriptions);

        let sink_handle = match cfg.sink {
            SinkSection::Parquet { output_dir } => ctx.env.start_sink(EnvSinkConfig::Parquet { output_dir }, rx)?,
            SinkSection::Questdb => ctx.env.start_sink(EnvSinkConfig::QuestDb, rx)?,
        };

        let _ = sink_handle.await.map_err(|e| crate::xcommons::error::AppError::internal(format!("sink join error: {}", e)))?;
        Ok(())
    }

    async fn stop(&self) -> AppResult<()> { Ok(()) }
}

// no helper needed; mapping is direct with typed enums now


