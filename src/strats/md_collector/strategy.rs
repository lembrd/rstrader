use async_trait::async_trait;

use crate::app::env::{AppEnvironment, EnvSinkConfig};
use crate::cli::{Exchange, StreamType, SubscriptionSpec};
use crate::xcommons::error::Result as AppResult;
use crate::strats::api::{Strategy, StrategyContext};

use super::config::{MdCollectorConfig, SinkSection, SubscriptionItem};

pub struct MdCollector;

#[async_trait]
impl Strategy for MdCollector {
    type Config = MdCollectorConfig;

    fn name(&self) -> &'static str { "md_collector" }

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()> {
        // Map config subscriptions into existing SubscriptionSpec
        let subscriptions: Vec<SubscriptionSpec> = cfg
            .subscriptions
            .into_iter()
            .map(|s| map_item_to_spec(s))
            .collect::<Result<_, String>>()
            .map_err(|e| crate::xcommons::error::AppError::cli(e))?;

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

fn map_item_to_spec(s: SubscriptionItem) -> Result<SubscriptionSpec, String> {
    let exchange = match s.exchange.as_str() {
        "BINANCE_FUTURES" => Exchange::BinanceFutures,
        "OKX_SWAP" => Exchange::OkxSwap,
        "OKX_SPOT" => Exchange::OkxSpot,
        "DERIBIT" => Exchange::Deribit,
        other => return Err(format!("Unsupported exchange '{}'", other)),
    };
    let stream_type = match s.stream_type.as_str() {
        "L2" => StreamType::L2,
        "TRADES" => StreamType::Trades,
        other => return Err(format!("Unsupported stream type '{}'", other)),
    };
    let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
    Ok(SubscriptionSpec { stream_type, exchange, instrument: s.instrument, max_connections })
}


