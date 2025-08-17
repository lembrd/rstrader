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
	ready_markets: std::collections::HashSet<i64>,
	// trading params
	lot_size: f64,
	max_position: f64,
	spread_bps: f64,
	displace_bps: f64,
	binance_account_id: Option<i64>,
	// latency tracking
	last_obs_ts_us: HashMap<i64, i64>, // market_id -> last OBS ts (exchange)
	last_req_ts_us: HashMap<i64, i64>,    // cl_ord_id -> post req time (local)
	last_cancel_req_ts_us: HashMap<i64, i64>, // cl_ord_id -> cancel req time (local)
	cl_to_symbol: HashMap<i64, String>, // cl_ord_id -> symbol
	market_id_to_symbol: HashMap<i64, String>,
}

impl Default for NaiveMm {
	fn default() -> Self {
		Self {
			fair_px: 0.0,
			obs_last_print_us: HashMap::new(),
			live_orders: HashMap::new(),
			did_init: std::collections::HashSet::new(),
			positions: HashMap::new(),
			ready_markets: std::collections::HashSet::new(),
			lot_size: 0.005,
			max_position: 0.015,
			spread_bps: 1.5,
			displace_bps: 1.0,
			binance_account_id: None,
			last_obs_ts_us: HashMap::new(),
			last_req_ts_us: HashMap::new(),
			last_cancel_req_ts_us: HashMap::new(),
			cl_to_symbol: HashMap::new(),
			market_id_to_symbol: HashMap::new(),
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

		// Gate order management until we have at least one position snapshot for this market
		let market_id = XMarketId::make(snapshot.exchange_id, &snapshot.symbol);
		// Track last market tick (exchange ts) and symbol mapping
		self.last_obs_ts_us.insert(market_id, snapshot.timestamp);
		self.market_id_to_symbol.insert(market_id, snapshot.symbol.clone());
		let is_ready = self.ready_markets.contains(&market_id);

		// One-time cancel all at startup per symbol (only after ready)
		if is_ready && !self.did_init.contains(&snapshot.symbol) {
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
		let code_start_us = crate::xcommons::types::time::now_micros();
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
		// If not ready, only publish metrics and return (no order management)
		if !is_ready { return; }

		let spread_bps = self.spread_bps;
		let displace_bps = self.displace_bps;
		let lot = self.lot_size;
		let tick = 0.1f64;
		let round_to_tick = |px: f64| -> f64 { (px / tick).round() * tick };
		let strategy_name = self.name();
		let symbol_clone = snapshot.symbol.clone();

		// position constraints from last known snapshot
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
						let sent = crate::xcommons::types::time::now_micros();
						if tx.try_send(OrderRequest::Cancel(creq)).is_ok() {
							self.last_cancel_req_ts_us.insert(cl, sent);
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_cancel_request(strategy_name, &symbol_clone); }
						}
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
							let sent = crate::xcommons::types::time::now_micros();
							self.last_req_ts_us.insert(cl, sent);
							self.cl_to_symbol.insert(cl, snapshot.symbol.clone());
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_post_request(strategy_name, &symbol_clone); }
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
						let sent = crate::xcommons::types::time::now_micros();
						if tx.try_send(OrderRequest::Cancel(creq)).is_ok() {
							self.last_cancel_req_ts_us.insert(cl, sent);
							self.cl_to_symbol.insert(cl, snapshot.symbol.clone());
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_cancel_request(strategy_name, &symbol_clone); }
						}
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
							let sent = crate::xcommons::types::time::now_micros();
							self.last_req_ts_us.insert(cl, sent);
							self.cl_to_symbol.insert(cl, snapshot.symbol.clone());
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_post_request(strategy_name, &symbol_clone); }
						}
					}
				}
			}
		}

		// Record strategy code latency (best-effort)
		let code_us_now = (crate::xcommons::types::time::now_micros() - code_start_us) as u64;
		if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
			g.latency_histograms.record_code_latency(code_us_now);
		}
		// Export code latency immediately so Prometheus shows non-zero even before executions
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let symbol = snapshot.symbol.as_str();
			if let Ok(g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &g.latency_histograms;
				prom.set_strategy_latency_quantiles(
					self.name(),
					symbol,
					None, None, // tick-to-trade
					None, None, // tick-to-cancel
					Some(h.code_us.value_at_quantile(0.50)),
					Some(h.code_us.value_at_quantile(0.99)),
					None, None, // network
				);
			}
		}
	}

	fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}

	fn on_execution(&mut self, account_id: i64, exec: XExecution, _io: &mut StrategyIo) {
		log::info!(
			"[naive_mm] exec account_id={} market_id={} side={} qty={} px={} id={}",
			account_id, exec.market_id, exec.side as i8, exec.last_qty, exec.last_px, exec.native_execution_id
		);
		// Latency measurements
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let strategy = self.name();
			let tick_ts = self.last_obs_ts_us.get(&exec.market_id).copied().unwrap_or(exec.timestamp);
			// Avoid negative due to clock/order; base on the earlier of (tick, exec)
			let base_ts = if tick_ts <= exec.timestamp { tick_ts } else { exec.timestamp };
			let ttt = (exec.rcv_timestamp - base_ts).max(0) as u64;
			if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &mut g.latency_histograms;
				if exec.exec_type == crate::xcommons::oms::ExecutionType::XOrderCanceled {
					h.record_tick_to_cancel(ttt);
				} else {
					h.record_tick_to_trade(ttt);
				}
			}
			// full network latencies for post/cancel (request -> ack)
			let post_net = if exec.cl_ord_id != -1 { self.last_req_ts_us.remove(&exec.cl_ord_id).map(|sent| (exec.rcv_timestamp - sent).max(0) as u64) } else { None };
			let cancel_net = if exec.cl_ord_id != -1 { self.last_cancel_req_ts_us.remove(&exec.cl_ord_id).map(|sent| (exec.rcv_timestamp - sent).max(0) as u64) } else { None };
			if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &mut g.latency_histograms;
				if let Some(nu) = post_net { h.record_post_network_latency(nu); }
				if let Some(nu) = cancel_net { h.record_cancel_network_latency(nu); }
				// we can later split network into separate hist if needed
			}
			// Export quantiles
			if let Ok(g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &g.latency_histograms;
				let symbol = exec.metadata.get("symbol").cloned()
					.or_else(|| if exec.cl_ord_id != -1 { self.cl_to_symbol.get(&exec.cl_ord_id).cloned() } else { None })
					.or_else(|| self.market_id_to_symbol.get(&exec.market_id).cloned())
					.unwrap_or_else(|| "UNKNOWN".to_string());
				prom.set_strategy_latency_quantiles(
					strategy,
					&symbol,
					Some(h.tick_to_trade_us.value_at_quantile(0.50)),
					Some(h.tick_to_trade_us.value_at_quantile(0.99)),
					cancel_net.map(|_| h.tick_to_cancel_us.value_at_quantile(0.50)),
					cancel_net.map(|_| h.tick_to_cancel_us.value_at_quantile(0.99)),
					Some(h.code_us.value_at_quantile(0.50)),
					Some(h.code_us.value_at_quantile(0.99)),
					post_net.map(|_| h.post_network_us.value_at_quantile(0.50)),
					post_net.map(|_| h.post_network_us.value_at_quantile(0.99)),
				);
				// Also set explicit post/cancel gauges using quantiles
				prom.set_strategy_post_cancel_latencies(
					strategy,
					&symbol,
					post_net.map(|_| h.post_network_us.value_at_quantile(0.50)),
					post_net.map(|_| h.post_network_us.value_at_quantile(0.99)),
					cancel_net.map(|_| h.cancel_network_us.value_at_quantile(0.50)),
					cancel_net.map(|_| h.cancel_network_us.value_at_quantile(0.99)),
				);
			}
		}
	}

	fn on_order_response(&mut self, resp: crate::xcommons::oms::OrderResponse, _io: &mut StrategyIo) {
		// Use REST/WS API ack timestamps to compute post/cancel latencies independent of executions
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let strategy = self.name();
			// Derive symbol using cl_ord_id lookup from live_orders
			let mut symbol = "UNKNOWN".to_string();
			if let Some(cl) = resp.cl_ord_id {
				if let Some(s) = self.cl_to_symbol.get(&cl) { symbol = s.clone(); }
			}
			let post_net = resp.cl_ord_id.and_then(|cl| self.last_req_ts_us.remove(&cl).map(|sent| (resp.rcv_timestamp - sent).max(0) as u64));
			let cancel_net = resp.cl_ord_id.and_then(|cl| self.last_cancel_req_ts_us.remove(&cl).map(|sent| (resp.rcv_timestamp - sent).max(0) as u64));
			{
				if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
					let h = &mut g.latency_histograms;
					if let Some(nu) = post_net { h.record_post_network_latency(nu); }
					if let Some(nu) = cancel_net { h.record_cancel_network_latency(nu); }
				}
				if let Ok(g) = crate::metrics::GLOBAL_METRICS.lock() {
					let h = &g.latency_histograms;
					prom.set_strategy_post_cancel_latencies(
						strategy,
						&symbol,
						post_net.map(|_| h.post_network_us.value_at_quantile(0.50)),
						post_net.map(|_| h.post_network_us.value_at_quantile(0.99)),
						cancel_net.map(|_| h.cancel_network_us.value_at_quantile(0.50)),
						cancel_net.map(|_| h.cancel_network_us.value_at_quantile(0.99)),
					);
				}
			}
		}
	}

	fn on_position(&mut self, account_id: i64, market_id: i64, pos: Position, _io: &mut StrategyIo) {
		log::info!(
			"[naive_mm] position account_id={} market_id={} amount={} avp={} realized={} fees={} trades_count={} vol={} bps={}",
			account_id, market_id, pos.amount, pos.avp, pos.realized, pos.fees, pos.trades_count, pos.quote_volume, pos.bps()
		);
		self.positions.insert(market_id, pos);
		self.ready_markets.insert(market_id);
	}
}
