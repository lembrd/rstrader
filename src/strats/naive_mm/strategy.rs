use async_trait::async_trait;

use crate::app::env::BinanceAccountParams;
use crate::xcommons::types::SubscriptionSpec;
use crate::strats::api::{Strategy, StrategyContext, StrategyRegistrar, SubscriptionId, StrategyIo};
use crate::xcommons::error::Result as AppResult;
use crate::xcommons::types::{OrderBookL2Update, TradeUpdate};
use crate::xcommons::oms::XExecution;
use crate::xcommons::position::Position;

use super::config::NaiveMmConfig;

pub struct NaiveMm {
    fair_px: f64,
}

impl Default for NaiveMm {
    fn default() -> Self { Self { fair_px: 0.0 } }
}

#[async_trait]
impl Strategy for NaiveMm {
    type Config = NaiveMmConfig;

    fn name(&self) -> &'static str { "naive_mm" }

    async fn configure(&mut self, reg: &mut StrategyRegistrar, _ctx: &StrategyContext, cfg: &Self::Config) -> AppResult<()> {
        for s in cfg.subscriptions.iter() {
            let max_connections = if s.arb_streams_num > 1 { Some(s.arb_streams_num) } else { None };
            let spec = SubscriptionSpec { stream_type: s.stream_type.clone(), exchange: s.exchange.clone(), instrument: s.instrument.clone(), max_connections };
            match spec.stream_type {
                crate::xcommons::types::StreamType::L2 => { let _ = reg.subscribe_l2(spec); }
                crate::xcommons::types::StreamType::Trade => { let _ = reg.subscribe_trades(spec); }
            }
        }

        // Subscribe to account executions/positions for each configured account in YAML (Binance section)
        reg.subscribe_binance_futures_accounts(BinanceAccountParams {
            api_key: cfg.binance.api_key.clone(),
            secret: cfg.binance.secret.clone(),
            account_id: cfg.binance.account_id,
            start_epoch_ts: cfg.binance.start_epoch_ts,
            fee_bps: cfg.binance.fee_bps,
            contract_size: cfg.binance.contract_size,
            symbols: cfg.binance.symbols.clone(),
        });
        Ok(())
    }

    fn on_l2(&mut self, _sub: SubscriptionId, msg: OrderBookL2Update, _io: &mut StrategyIo) {
        self.fair_px = msg.price;
    }

    fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}

    fn on_execution(&mut self, account_id: i64, exec: XExecution, _io: &mut StrategyIo) {
        log::info!(
            "[naive_mm] exec account_id={} market_id={} side={} qty={} px={} id={}",
            account_id, exec.market_id, exec.side as i8, exec.last_qty, exec.last_px, exec.native_execution_id
        );
    }

    fn on_position(&mut self, account_id: i64, market_id: i64, pos: Position, _io: &mut StrategyIo) {
        log::info!(
            "[naive_mm] position account_id={} market_id={} amount={} avp={} realized={} fees={} trades_count={} vol={} bps={}",
            account_id, market_id, pos.amount, pos.avp, pos.realized, pos.fees, pos.trades_count, pos.quote_volume, pos.bps()
        );
    }
}
