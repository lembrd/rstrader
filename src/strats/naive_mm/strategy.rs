use async_trait::async_trait;

use crate::app::env::BinanceAccountParams;
use crate::xcommons::types::SubscriptionSpec;
use crate::strats::api::{Strategy, StrategyContext, StrategyRegistrar, SubscriptionId, StrategyIo};
use crate::xcommons::error::Result as AppResult;
use crate::xcommons::types::{OrderBookL2Update, TradeUpdate, OrderBookSnapshot};
use crate::xcommons::oms::{XExecution, Side, OrderMode, TimeInForce, PostRequest, CancelRequest, CancelAllRequest};
use crate::xcommons::oms::OrderRequest;
use crate::xcommons::position::Position;
use std::collections::HashMap;
use crate::xcommons::xmarket_id::XMarketId;

use super::config::NaiveMmConfig;

pub struct NaiveMm {
    fair_px: f64,
    obs_last_print_us: HashMap<SubscriptionId, i64>,
    // per symbol state
    live_orders: HashMap<String, (Option<(i64, f64)>, Option<(i64, f64)>)>, // symbol -> (bid: (cl_id, px), ask: (cl_id, px))
    did_init: std::collections::HashSet<String>,
    positions: HashMap<i64, Position>,
    // trading params
    lot_size: f64,
    max_position: f64,
    spread_bps: f64,
    displace_bps: f64,
    binance_account_id: Option<i64>,
}

impl Default for NaiveMm {
    fn default() -> Self {
        Self {
            fair_px: 0.0,
            obs_last_print_us: HashMap::new(),
            live_orders: HashMap::new(),
            did_init: std::collections::HashSet::new(),
            positions: HashMap::new(),
            lot_size: 0.005,
            max_position: 0.015,
            spread_bps: 1.5,
            displace_bps: 1.0,
            binance_account_id: None,
        }
    }
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
            ed25519_key: cfg.binance.ed25519_key.clone(),
            ed25519_secret: cfg.binance.ed25519_secret.clone(),
            account_id: cfg.binance.account_id,
            start_epoch_ts: cfg.binance.start_epoch_ts,
            fee_bps: cfg.binance.fee_bps,
            contract_size: cfg.binance.contract_size,
            symbols: cfg.binance.symbols.clone(),
        });
        // cache trading params
        self.lot_size = cfg.lot_size;
        self.max_position = cfg.max_position;
        self.spread_bps = cfg.spread_bps;
        self.displace_bps = cfg.displace_th_bps;
        self.binance_account_id = Some(cfg.binance.account_id);
        Ok(())
    }

    fn on_l2(&mut self, _sub: SubscriptionId, msg: OrderBookL2Update, _io: &mut StrategyIo) { self.fair_px = msg.price; }

    fn on_obs(&mut self, sub: SubscriptionId, snapshot: OrderBookSnapshot, io: &mut StrategyIo) {
        const PRINT_INTERVAL_US: i64 = 5_000_000; // 5 seconds
        let now_us = crate::xcommons::types::time::now_micros();
        let last_us = *self.obs_last_print_us.get(&sub).unwrap_or(&0);
        let should_print = now_us - last_us >= PRINT_INTERVAL_US;
        if should_print { self.obs_last_print_us.insert(sub, now_us); }

        // One-time cancel all at startup per symbol
        if !self.did_init.contains(&snapshot.symbol) {
            if let Some(tx) = io.order_txs.get(&snapshot.symbol) {
                if let Some(account_id) = self.binance_account_id {
                    let req = CancelAllRequest {
                        req_id: crate::xcommons::monoseq::next_id(),
                        timestamp: crate::xcommons::types::time::now_micros(),
                        market_id: XMarketId::make(snapshot.exchange_id, &snapshot.symbol),
                        account_id,
                    };
                    let _ = tx.try_send(OrderRequest::CancelAll(req));
                    self.did_init.insert(snapshot.symbol.clone());
                }
            }
        }
        // Throttled pretty print of top10
        if should_print {
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

        // Trading logic
        // Fast path: compute best bid/ask without sorting for trading decisions
        let best_bid_opt = snapshot.bids.iter().max_by(|a,b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        let best_ask_opt = snapshot.asks.iter().min_by(|a,b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        let (best_bid, best_ask) = match (best_bid_opt, best_ask_opt) { (Some(b), Some(a)) => (b, a), _ => return };
        let mid_price = (best_bid.price * best_ask.qty + best_ask.price * best_bid.qty) / (best_bid.qty + best_ask.qty);
        // Fast strategy metrics push: mm PnL, amount, bps (non-blocking best-effort)
        if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
            let market_id = XMarketId::make(snapshot.exchange_id, &snapshot.symbol);
            let pos = self.positions.get(&market_id).cloned().unwrap_or_else(|| crate::xcommons::position::Position::new(0.0, 1.0));
            let pnl = pos.current_pnl(mid_price);
            let bps = pos.bps();
            prom.set_strategy_metrics(self.name(), &snapshot.symbol, pnl, pos.amount, bps);
        }
        let spread_bps = self.spread_bps;
        let displace_bps = self.displace_bps;
        let lot = self.lot_size;
        let tick = 0.1f64;
        let round_to_tick = |px: f64| -> f64 { (px / tick).round() * tick };

        // position constraints from last known snapshot
        let market_id = XMarketId::make(snapshot.exchange_id, &snapshot.symbol);
        let pos_amt = self.positions.get(&market_id).map(|p| p.amount).unwrap_or(0.0);
        // compute targets
        let bid_target = round_to_tick(mid_price * (1.0 - spread_bps / 10_000.0));
        let ask_target = round_to_tick(mid_price * (1.0 + spread_bps / 10_000.0));
        let disp_th = displace_bps / 10_000.0 * mid_price;
        let entry = self.live_orders.entry(snapshot.symbol.clone()).or_insert((None, None));
        if let Some(tx) = io.order_txs.get(&snapshot.symbol) {
            // BID side
            match entry.0 {
                Some((cl, px)) => {
                    if (px - bid_target).abs() > disp_th || pos_amt >= self.max_position { // displace or limit
                        let creq = CancelRequest {
                            req_id: crate::xcommons::monoseq::next_id(),
                            timestamp: crate::xcommons::types::time::now_micros(),
                            market_id,
                            account_id: self.binance_account_id.unwrap_or_default(),
                            cl_ord_id: Some(cl),
                            native_ord_id: None,
                        };
                        let _ = tx.try_send(OrderRequest::Cancel(creq));
                        entry.0 = None;
                    }
                }
                None => {
                    if pos_amt < self.max_position {
                        let preq = PostRequest {
                            req_id: crate::xcommons::monoseq::next_id(),
                            timestamp: crate::xcommons::types::time::now_micros(),
                            cl_ord_id: crate::xcommons::monoseq::next_id(),
                            market_id,
                            account_id: self.binance_account_id.unwrap_or_default(),
                            side: Side::Buy,
                            qty: lot,
                            price: bid_target,
                            ord_mode: OrderMode::MLimit,
                            tif: TimeInForce::TifGoodTillCancel,
                            post_only: true,
                            reduce_only: false,
                            metadata: Default::default(),
                        };
                        let cl = preq.cl_ord_id;
                        if tx.try_send(OrderRequest::Post(preq)).is_ok() {
                            entry.0 = Some((cl, bid_target));
                        }
                    }
                }
            }
            // ASK side
            match entry.1 {
                Some((cl, px)) => {
                    if (px - ask_target).abs() > disp_th || -pos_amt >= self.max_position { // displace or limit
                        let creq = CancelRequest {
                            req_id: crate::xcommons::monoseq::next_id(),
                            timestamp: crate::xcommons::types::time::now_micros(),
                            market_id,
                            account_id: self.binance_account_id.unwrap_or_default(),
                            cl_ord_id: Some(cl),
                            native_ord_id: None,
                        };
                        let _ = tx.try_send(OrderRequest::Cancel(creq));
                        entry.1 = None;
                    }
                }
                None => {
                    if -pos_amt < self.max_position {
                        let preq = PostRequest {
                            req_id: crate::xcommons::monoseq::next_id(),
                            timestamp: crate::xcommons::types::time::now_micros(),
                            cl_ord_id: crate::xcommons::monoseq::next_id(),
                            market_id,
                            account_id: self.binance_account_id.unwrap_or_default(),
                            side: Side::Sell,
                            qty: lot,
                            price: ask_target,
                            ord_mode: OrderMode::MLimit,
                            tif: TimeInForce::TifGoodTillCancel,
                            post_only: true,
                            reduce_only: false,
                            metadata: Default::default(),
                        };
                        let cl = preq.cl_ord_id;
                        if tx.try_send(OrderRequest::Post(preq)).is_ok() {
                            entry.1 = Some((cl, ask_target));
                        }
                    }
                }
            }
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
        self.positions.insert(market_id, pos);
    }
}
