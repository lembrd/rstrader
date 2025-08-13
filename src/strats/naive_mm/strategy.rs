use async_trait::async_trait;

use crate::app::env::{BinanceAccountParams};
use crate::cli::SubscriptionSpec;
use crate::strats::api::{Strategy, StrategyContext};
use crate::xcommons::error::Result as AppResult;

use super::config::NaiveMmConfig;

pub struct NaiveMm;

#[async_trait]
impl Strategy for NaiveMm {
    type Config = NaiveMmConfig;

    fn name(&self) -> &'static str { "naive_mm" }

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()> {
        // 1) Start market data subscriptions
        let subscriptions: Vec<SubscriptionSpec> = cfg
            .subscriptions
            .into_iter()
            .map(|s| -> std::result::Result<SubscriptionSpec, crate::xcommons::error::AppError> {
                let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
                Ok(SubscriptionSpec { stream_type: s.stream_type, exchange: s.exchange, instrument: s.instrument, max_connections })
            })
            .collect::<std::result::Result<_, _>>()?;

        let mut rx = ctx.env.start_subscriptions(subscriptions);
        // Drain market data to avoid backpressure; we may use it later for quoting
        tokio::spawn(async move { while let Some(_msg) = rx.recv().await {} });

        // 2) Start Binance Futures trading account(s) via environment
        let _runners = ctx.env.start_binance_futures_accounts(BinanceAccountParams {
            api_key: cfg.binance.api_key.clone(),
            secret: cfg.binance.secret.clone(),
            account_id: cfg.binance.account_id,
            start_epoch_ts: cfg.binance.start_epoch_ts,
            fee_bps: cfg.binance.fee_bps,
            contract_size: cfg.binance.contract_size,
            symbols: cfg.binance.symbols.clone(),
        })?;

        // Keep the strategy alive until CTRL-C
        tokio::signal::ctrl_c().await.map_err(|e| crate::xcommons::error::AppError::internal(format!("signal error: {}", e)))?;
        Ok(())
    }

    async fn stop(&self) -> AppResult<()> { Ok(()) }
}

