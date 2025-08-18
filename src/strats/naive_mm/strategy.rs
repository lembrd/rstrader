use async_trait::async_trait;

use crate::app::env::BinanceAccountParams;
use crate::xcommons::types::SubscriptionSpec;
use crate::strats::api::{Strategy, StrategyContext, StrategyRegistrar, SubscriptionId, StrategyIo};
use crate::xcommons::error::Result as AppResult;
use crate::xcommons::types::{OrderBookL2Update, TradeUpdate, OrderBookSnapshot};
use crate::xcommons::oms::{XExecution, Side, OrderMode, TimeInForce, PostRequest, CancelRequest, CancelAllRequest};
use crate::xcommons::oms::OrderRequest;
use crate::xcommons::position::Position;
use crate::xcommons::xmarket_id::XMarketId;

use super::config::NaiveMmConfig;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveOrderStatus { PendingNew, Live, PendingCancel }

#[derive(Clone, Copy)]
struct LiveOrder { status: LiveOrderStatus, cl_ord_id: i64, price: f64 }

pub struct NaiveMm {
	// trading params
	lot_size: f64,
	max_position: f64,
	spread_bps: f64,
	displace_bps: f64,
	binance_account_id: Option<i64>,
	// single market context
	market_id: Option<i64>,
	symbol: Option<String>,
	ready: bool,
	position: Position,
	// last obs/print
	obs_last_print_us: i64,
	last_obs_ts_us: i64,
	// decision input
	fair_px: f64,
	last_mid: Option<f64>,
	// live bid/ask orders
	bid_order: Option<LiveOrder>,
	ask_order: Option<LiveOrder>,
	// pending cancel flags
	pending_cancel_bid: bool,
	pending_cancel_ask: bool,
	// request timestamps for metrics
	last_post_req_ts_us_bid: Option<i64>,
	last_post_req_ts_us_ask: Option<i64>,
	last_cancel_req_ts_us_bid: Option<i64>,
	last_cancel_req_ts_us_ask: Option<i64>,
	// one-time cancel-all init
	did_init: bool,
}

impl Default for NaiveMm {
	fn default() -> Self {
		Self {
			lot_size: 0.005,
			max_position: 0.015,
			spread_bps: 1.5,
			displace_bps: 1.0,
			binance_account_id: None,
			market_id: None,
			symbol: None,
			ready: false,
			position: Position::new(0.0, 1.0),
			obs_last_print_us: 0,
			last_obs_ts_us: 0,
			fair_px: 0.0,
			last_mid: None,
			bid_order: None,
			ask_order: None,
			pending_cancel_bid: false,
			pending_cancel_ask: false,
			last_post_req_ts_us_bid: None,
			last_post_req_ts_us_ask: None,
			last_cancel_req_ts_us_bid: None,
			last_cancel_req_ts_us_ask: None,
			did_init: false,
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
				crate::xcommons::types::StreamType::Obs => { let _ = reg.subscribe_obs(spec.clone()); }
			}
		}

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
		self.lot_size = cfg.lot_size;
		self.max_position = cfg.max_position;
		self.spread_bps = cfg.spread_bps;
		self.displace_bps = cfg.displace_th_bps;
		self.binance_account_id = Some(cfg.binance.account_id);
		Ok(())
	}

	fn on_l2(&mut self, _sub: SubscriptionId, msg: OrderBookL2Update, io: &mut StrategyIo) {
		self.fair_px = msg.price;
		io.schedule_update();
	}

	fn on_obs(&mut self, _sub: SubscriptionId, snapshot: OrderBookSnapshot, io: &mut StrategyIo) {
		const PRINT_INTERVAL_US: i64 = 5_000_000; // 5 seconds
		let now_us = crate::xcommons::types::time::now_micros();
		let should_print = now_us - self.obs_last_print_us >= PRINT_INTERVAL_US;
		if should_print { self.obs_last_print_us = now_us; }

		let market_id = XMarketId::make(snapshot.exchange_id, &snapshot.symbol);
		self.market_id = Some(market_id);
		self.symbol = Some(snapshot.symbol.clone());
		self.last_obs_ts_us = snapshot.timestamp;
		let is_ready = self.ready;

		if is_ready && !self.did_init {
			if let Some(tx) = io.order_txs.get(&market_id) {
				if let Some(account_id) = self.binance_account_id {
					let req = CancelAllRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), market_id, account_id };
					if let Err(e) = tx.try_send(OrderRequest::CancelAll(req)) {
						log::warn!("[naive_mm] mailbox overflow dropping CancelAll request: {}", e);
					}
					self.did_init = true;
				}
			}
		}

		let best_bid_opt = snapshot.bids.iter().max_by(|a,b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
		let best_ask_opt = snapshot.asks.iter().min_by(|a,b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
		let (best_bid, best_ask) = match (best_bid_opt, best_ask_opt) { (Some(b), Some(a)) => (b, a), _ => return };
		let mid_price = (best_bid.price * best_ask.qty + best_ask.price * best_bid.qty) / (best_bid.qty + best_ask.qty);
		self.last_mid = Some(mid_price);
		self.fair_px = mid_price;
		if should_print {
			let bid_state = match self.bid_order {
				Some(lo) => {
					let st = match lo.status { LiveOrderStatus::PendingNew => "PendingNew", LiveOrderStatus::Live => "Live", LiveOrderStatus::PendingCancel => "PendingCancel" };
					format!("{}@{:.2}/{}", st, lo.price, lo.cl_ord_id)
				}
				None => "None".to_string(),
			};
			let ask_state = match self.ask_order {
				Some(lo) => {
					let st = match lo.status { LiveOrderStatus::PendingNew => "PendingNew", LiveOrderStatus::Live => "Live", LiveOrderStatus::PendingCancel => "PendingCancel" };
					format!("{}@{:.2}/{}", st, lo.price, lo.cl_ord_id)
				}
				None => "None".to_string(),
			};
			log::info!(
				"[naive_mm] on_obs: symbol={} mid={} market_id={} ready={} bid={} ask={}",
				snapshot.symbol,
				mid_price,
				XMarketId::make(snapshot.exchange_id, &snapshot.symbol),
				self.ready,
				bid_state,
				ask_state,
			);
		}
		io.schedule_update();

		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let pos = self.position.clone();
			let pnl = pos.current_pnl(mid_price);
			let bps = pos.bps();
			prom.set_strategy_metrics(self.name(), &snapshot.symbol, pnl, pos.amount, bps);
		}

		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let symbol = snapshot.symbol.as_str();
			if let Ok(g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &g.latency_histograms;
				prom.set_strategy_latency_quantiles(self.name(), symbol, None, None, None, None, Some(h.code_us.value_at_quantile(0.50)), Some(h.code_us.value_at_quantile(0.99)), None, None);
			}
		}
	}

	fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}

	fn on_execution(&mut self, account_id: i64, exec: XExecution, io: &mut StrategyIo) {
		log::info!("[naive_mm] exec account_id={} market_id={} side={} qty={} px={} id={}", account_id, exec.market_id, exec.side as i8, exec.last_qty, exec.last_px, exec.native_execution_id);
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let strategy = self.name();
			let tick_ts = self.last_obs_ts_us;
			let base_ts = if tick_ts <= exec.timestamp { tick_ts } else { exec.timestamp };
			let ttt = (exec.rcv_timestamp - base_ts).max(0) as u64;
			if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &mut g.latency_histograms;
				if exec.exec_type == crate::xcommons::oms::ExecutionType::XOrderCanceled { h.record_tick_to_cancel(ttt); } else { h.record_tick_to_trade(ttt); }
			}
			if let Ok(g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &g.latency_histograms;
				let symbol = self.symbol.clone().unwrap_or_else(|| "UNKNOWN".to_string());
				prom.set_strategy_latency_quantiles(strategy, &symbol, Some(h.tick_to_trade_us.value_at_quantile(0.50)), Some(h.tick_to_trade_us.value_at_quantile(0.99)), Some(h.tick_to_cancel_us.value_at_quantile(0.50)), Some(h.tick_to_cancel_us.value_at_quantile(0.99)), Some(h.code_us.value_at_quantile(0.50)), Some(h.code_us.value_at_quantile(0.99)), None, None);
			}
		}
		if exec.cl_ord_id != -1 {
			if self.bid_order.map(|lo| lo.cl_ord_id == exec.cl_ord_id).unwrap_or(false) {
				match exec.exec_type {
					crate::xcommons::oms::ExecutionType::XOrderCanceled => { self.bid_order = None; self.pending_cancel_bid = false; }
					crate::xcommons::oms::ExecutionType::XOrderNew | crate::xcommons::oms::ExecutionType::XOrderReplaced => {
						if let Some(ref mut lo) = self.bid_order { lo.status = LiveOrderStatus::Live; }
					}
					_ => {
						if exec.ord_status.is_alive() {
							if let Some(ref mut lo) = self.bid_order { lo.status = LiveOrderStatus::Live; }
						} else {
							self.bid_order = None; self.pending_cancel_bid = false;
						}
					}
				}
			}
			if self.ask_order.map(|lo| lo.cl_ord_id == exec.cl_ord_id).unwrap_or(false) {
				match exec.exec_type {
					crate::xcommons::oms::ExecutionType::XOrderCanceled => { self.ask_order = None; self.pending_cancel_ask = false; }
					crate::xcommons::oms::ExecutionType::XOrderNew | crate::xcommons::oms::ExecutionType::XOrderReplaced => {
						if let Some(ref mut lo) = self.ask_order { lo.status = LiveOrderStatus::Live; }
					}
					_ => {
						if exec.ord_status.is_alive() {
							if let Some(ref mut lo) = self.ask_order { lo.status = LiveOrderStatus::Live; }
						} else {
							self.ask_order = None; self.pending_cancel_ask = false;
						}
					}
				}
			}
		}
		io.schedule_update();
	}

	fn on_order_response(&mut self, resp: crate::xcommons::oms::OrderResponse, io: &mut StrategyIo) {
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let strategy = self.name();
			let symbol = self.symbol.clone().unwrap_or_else(|| "UNKNOWN".to_string());
			let post_net = if let Some(cl) = resp.cl_ord_id {
				if self.bid_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.last_post_req_ts_us_bid.map(|sent| (resp.rcv_timestamp - sent).max(0) as u64) }
				else if self.ask_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.last_post_req_ts_us_ask.map(|sent| (resp.rcv_timestamp - sent).max(0) as u64) }
				else { None }
			} else { None };
			let cancel_net = if let Some(cl) = resp.cl_ord_id {
				if self.bid_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.last_cancel_req_ts_us_bid.map(|sent| (resp.rcv_timestamp - sent).max(0) as u64) }
				else if self.ask_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.last_cancel_req_ts_us_ask.map(|sent| (resp.rcv_timestamp - sent).max(0) as u64) }
				else { None }
			} else { None };
			if let Ok(mut g) = crate::metrics::GLOBAL_METRICS.lock() {
				let h = &mut g.latency_histograms;
				if let Some(nu) = post_net { h.record_post_network_latency(nu); }
				if let Some(nu) = cancel_net { h.record_cancel_network_latency(nu); }
			}
			if let Ok(g2) = crate::metrics::GLOBAL_METRICS.lock() {
				let h2 = &g2.latency_histograms;
				prom.set_strategy_post_cancel_latencies(strategy, &symbol, Some(h2.post_network_us.value_at_quantile(0.50)), Some(h2.post_network_us.value_at_quantile(0.99)), Some(h2.cancel_network_us.value_at_quantile(0.50)), Some(h2.cancel_network_us.value_at_quantile(0.99)));
			}
			if resp.status == crate::xcommons::oms::OrderResponseStatus::Ok {
				if let Some(cl) = resp.cl_ord_id {
					// If we were canceling this order, clear it; otherwise treat as Post ACK and mark Live
					if self.bid_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) {
						if self.pending_cancel_bid { self.pending_cancel_bid = false; self.bid_order = None; }
						else if let Some(ref mut lo) = self.bid_order { lo.status = LiveOrderStatus::Live; }
					}
					if self.ask_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) {
						if self.pending_cancel_ask { self.pending_cancel_ask = false; self.ask_order = None; }
						else if let Some(ref mut lo) = self.ask_order { lo.status = LiveOrderStatus::Live; }
					}
				}
			}
			if resp.status == crate::xcommons::oms::OrderResponseStatus::FailedPostOnly {
				if let Some(cl) = resp.cl_ord_id {
					if self.bid_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.bid_order = None; self.last_post_req_ts_us_bid = None; }
					if self.ask_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.ask_order = None; self.last_post_req_ts_us_ask = None; }
				}
			}
			// If the API reports order not found on a cancel request, clear the slot (race: fill before cancel ACK)
			if resp.status == crate::xcommons::oms::OrderResponseStatus::FailedOrderNotFound {
				if let Some(cl) = resp.cl_ord_id {
					if self.bid_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.pending_cancel_bid = false; self.bid_order = None; }
					if self.ask_order.map(|o| o.cl_ord_id == cl).unwrap_or(false) { self.pending_cancel_ask = false; self.ask_order = None; }
				}
			}
		}
		// After processing an order response, trigger a decision pass
		io.schedule_update();
	}

	fn on_position(&mut self, account_id: i64, market_id: i64, pos: Position, io: &mut StrategyIo) {
		log::info!("[naive_mm] position account_id={} market_id={} amount={} avp={} realized={} fees={} trades_count={} vol={} bps={}", account_id, market_id, pos.amount, pos.avp, pos.realized, pos.fees, pos.trades_count, pos.quote_volume, pos.bps());
		self.position = pos;
		self.market_id = Some(market_id);
		self.ready = true;
		log::info!("[naive_mm] on_position: ready set, market_id={} symbol={:?}", market_id, self.symbol);
		// Update strategy metrics on position event as well to avoid waiting for next OBS tick
		if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
			let symbol = self.symbol.clone().unwrap_or_else(|| "UNKNOWN".to_string());
			let mid = self.last_mid.unwrap_or(self.fair_px);
			let pnl = self.position.current_pnl(if mid.is_finite() && mid > 0.0 { mid } else { self.position.avp.max(0.0) });
			prom.set_strategy_metrics(self.name(), &symbol, pnl, self.position.amount, self.position.bps());
		}
		io.schedule_update();
	}

	fn update(&mut self, io: &mut StrategyIo) {
		let (Some(market_id), Some(mid_price)) = (self.market_id, self.last_mid) else {
			log::debug!("[naive_mm] update skipped: market_id or mid missing. market_id={:?} mid_present={} ready={}", self.market_id, self.last_mid.is_some(), self.ready);
			return
		};
		// Become ready when order channel is available (initial positions may not be pushed immediately)
		if !self.ready {
			let has_tx = io.order_txs.contains_key(&market_id);
			if has_tx {
				self.ready = true;
				log::info!("[naive_mm] became ready: market_id={} symbol={:?}", market_id, self.symbol);
				// One-time cancel-all on first readiness
				if !self.did_init {
					if let Some(tx) = io.order_txs.get(&market_id) {
						if let Some(account_id) = self.binance_account_id {
							let req = CancelAllRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), market_id, account_id };
							if let Err(e) = tx.try_send(OrderRequest::CancelAll(req)) { log::warn!("[naive_mm] mailbox overflow dropping CancelAll request: {}", e); }
							self.did_init = true;
						}
					}
				}
			} else {
				log::warn!("[naive_mm] update: not ready and no order tx for market_id={} symbol={:?}", market_id, self.symbol);
				return;
			}
		}
		let tick = 0.1f64;
		let round_to_tick = |px: f64| -> f64 { (px / tick).round() * tick };
		let strategy_name = self.name();
		let spread_bps = self.spread_bps;
		let displace_bps = self.displace_bps;
		let lot = self.lot_size;
		let symbol_clone = self.symbol.clone().unwrap_or_else(|| "UNKNOWN".to_string());
		let pos_amt = self.position.amount;
		let bid_target = round_to_tick(mid_price * (1.0 - spread_bps / 10_000.0));
		let ask_target = round_to_tick(mid_price * (1.0 + spread_bps / 10_000.0));
		let disp_th = displace_bps / 10_000.0 * mid_price;
		// If a cancel request errored upstream (not forwarded to strategy), clear pending cancel after a short TTL
		let now_us = crate::xcommons::types::time::now_micros();
		const CANCEL_TTL_US: i64 = 800_000; // 0.8s safety
		if self.pending_cancel_bid {
			if let Some(sent) = self.last_cancel_req_ts_us_bid {
				if now_us - sent > CANCEL_TTL_US { self.pending_cancel_bid = false; self.bid_order = None; }
			}
		}
		if self.pending_cancel_ask {
			if let Some(sent) = self.last_cancel_req_ts_us_ask {
				if now_us - sent > CANCEL_TTL_US { self.pending_cancel_ask = false; self.ask_order = None; }
			}
		}
		let has_tx = io.order_txs.contains_key(&market_id);
		if !has_tx {
			log::warn!("[naive_mm] no order tx for market_id={} symbol={:?}", market_id, self.symbol);
		}
		if let Some(tx) = io.order_txs.get(&market_id) {
			match self.bid_order {
				Some(lo) => {
					let cl = lo.cl_ord_id; let px = lo.price;
					if lo.status == LiveOrderStatus::Live && ((px - bid_target).abs() > disp_th || pos_amt >= self.max_position) {
						let creq = CancelRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), market_id, account_id: self.binance_account_id.unwrap_or_default(), cl_ord_id: Some(cl), native_ord_id: None };
						let sent = crate::xcommons::types::time::now_micros();
						if !self.pending_cancel_bid && tx.try_send(OrderRequest::Cancel(creq)).is_ok() {
							self.last_cancel_req_ts_us_bid = Some(sent);
							self.pending_cancel_bid = true;
							if let Some(ref mut cur) = self.bid_order { cur.status = LiveOrderStatus::PendingCancel; }
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_cancel_request(strategy_name, &symbol_clone); }
						} else if !self.pending_cancel_bid {
							log::warn!("[naive_mm] mailbox overflow dropping Cancel BUY request for market_id={} symbol={:?}", market_id, self.symbol);
						}
					}
				}
				None => {
					// Relax repost guard: allow equal to max_position to keep two-sided quoting after fills
					if pos_amt <= self.max_position && self.bid_order.is_none() {
						let preq = PostRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), cl_ord_id: crate::xcommons::monoseq::next_id(), market_id, account_id: self.binance_account_id.unwrap_or_default(), side: Side::Buy, qty: lot, price: bid_target, ord_mode: OrderMode::MLimit, tif: TimeInForce::TifGoodTillCancel, post_only: true, reduce_only: false, metadata: Default::default() };
						let cl = preq.cl_ord_id;
						if tx.try_send(OrderRequest::Post(preq)).is_ok() {
							self.bid_order = Some(LiveOrder { status: LiveOrderStatus::PendingNew, cl_ord_id: cl, price: bid_target });
							let sent = crate::xcommons::types::time::now_micros();
							self.last_post_req_ts_us_bid = Some(sent);
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_post_request(strategy_name, &symbol_clone); }
						} else { log::warn!("[naive_mm] mailbox overflow dropping Post BUY for market_id={} symbol={:?}", market_id, self.symbol); }
					}
				}
			}
			match self.ask_order {
				Some(lo) => {
					let cl = lo.cl_ord_id; let px = lo.price;
					if lo.status == LiveOrderStatus::Live && ((px - ask_target).abs() > disp_th || -pos_amt >= self.max_position) {
						let creq = CancelRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), market_id, account_id: self.binance_account_id.unwrap_or_default(), cl_ord_id: Some(cl), native_ord_id: None };
						let sent = crate::xcommons::types::time::now_micros();
						if !self.pending_cancel_ask && tx.try_send(OrderRequest::Cancel(creq)).is_ok() {
							self.last_cancel_req_ts_us_ask = Some(sent);
							self.pending_cancel_ask = true;
							if let Some(ref mut cur) = self.ask_order { cur.status = LiveOrderStatus::PendingCancel; }
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_cancel_request(strategy_name, &symbol_clone); }
						} else if !self.pending_cancel_ask {
							log::warn!("[naive_mm] mailbox overflow dropping Cancel SELL request for market_id={} symbol={:?}", market_id, self.symbol);
						}
					}
				}
				None => {
					if -pos_amt <= self.max_position && self.ask_order.is_none() {
						let preq = PostRequest { req_id: crate::xcommons::monoseq::next_id(), timestamp: crate::xcommons::types::time::now_micros(), cl_ord_id: crate::xcommons::monoseq::next_id(), market_id, account_id: self.binance_account_id.unwrap_or_default(), side: Side::Sell, qty: lot, price: ask_target, ord_mode: OrderMode::MLimit, tif: TimeInForce::TifGoodTillCancel, post_only: true, reduce_only: false, metadata: Default::default() };
						let cl = preq.cl_ord_id;

						if tx.try_send(OrderRequest::Post(preq)).is_ok() {
							self.ask_order = Some(LiveOrder { status: LiveOrderStatus::PendingNew, cl_ord_id: cl, price: ask_target });
							let sent = crate::xcommons::types::time::now_micros();
							self.last_post_req_ts_us_ask = Some(sent);
							if let Some(prom) = crate::metrics::PROM_EXPORTER.get() { prom.inc_strategy_post_request(strategy_name, &symbol_clone); }
						} else { log::warn!("[naive_mm] mailbox overflow dropping Post SELL for market_id={} symbol={:?}", market_id, self.symbol); }
					}
				}
			}
		}
	}
}
