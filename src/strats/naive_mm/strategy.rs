use async_trait::async_trait;

use crate::app::env::BinanceAccountParams;
use crate::xcommons::types::SubscriptionSpec;
use crate::strats::api::{Strategy, StrategyContext, StrategyRegistrar, SubscriptionId, StrategyIo};
use crate::xcommons::error::Result as AppResult;
use crate::xcommons::types::{OrderBookL2Update, TradeUpdate, OrderBookSnapshot};
use crate::xcommons::oms::XExecution;
use crate::xcommons::position::Position;
use std::collections::HashMap;

use super::config::NaiveMmConfig;

pub struct NaiveMm {
    fair_px: f64,
    obs_last_print_us: HashMap<SubscriptionId, i64>,
}

impl Default for NaiveMm {
    fn default() -> Self { Self { fair_px: 0.0, obs_last_print_us: HashMap::new() } }
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
                crate::xcommons::types::StreamType::Obs => {
                    let _ = reg.subscribe_obs(spec.clone());
                }
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

    fn on_l2(&mut self, _sub: SubscriptionId, msg: OrderBookL2Update, _io: &mut StrategyIo) { self.fair_px = msg.price; }

    fn on_obs(&mut self, sub: SubscriptionId, snapshot: OrderBookSnapshot, _io: &mut StrategyIo) {
        const PRINT_INTERVAL_US: i64 = 5_000_000; // 5 seconds
        let now_us = crate::xcommons::types::time::now_micros();
        let last_us = *self.obs_last_print_us.get(&sub).unwrap_or(&0);
        if now_us - last_us < PRINT_INTERVAL_US { return; }
        self.obs_last_print_us.insert(sub, now_us);
        // print from framework-provided snapshot
        // Build truncated top 10 rows and print
        let mut bids = snapshot.bids.clone();
        let mut asks = snapshot.asks.clone();
        bids.sort_by(|a,b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        asks.sort_by(|a,b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        let rows = 10usize;
        log::info!("[OBS] {} top10:", snapshot.symbol);
        log::info!("{:<12} {:<14} | {:<14} {:<12}", "bid_amt", "bid_px", "ask_px", "ask_amt");
        for i in 0..rows {
            let (bid_px, bid_qty) = if i < bids.len() { (bids[i].price, bids[i].qty) } else { (0.0, 0.0) };
            let (ask_px, ask_qty) = if i < asks.len() { (asks[i].price, asks[i].qty) } else { (0.0, 0.0) };
            if bid_qty == 0.0 && ask_qty == 0.0 { continue; }
            log::info!(
                "{:<12.4} {:<14.4} | {:<14.4} {:<12.4}",
                bid_qty, bid_px, ask_px, ask_qty
            );
        }
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
