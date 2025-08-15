//

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::Arc;

use crate::app::env::{AppEnvironment, BinanceAccountParams};
use crate::xcommons::error::Result as AppResult;
use crate::xcommons::types::{ExchangeId, OrderBookL2Update, StreamData, TradeUpdate, StreamType as CoreStreamType, SubscriptionSpec, OrderBookSnapshot};
use crate::xcommons::position::Position;
use crate::xcommons::oms::{XExecution, OrderRequest, OrderResponse};
use crate::trading::account_state::ExchangeAccountAdapter;

#[derive(Clone)]
pub struct StrategyContext {
    pub env: Arc<dyn AppEnvironment>,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct SubscriptionId(pub u32);

#[derive(Debug)]
pub enum StrategyControl {
    Start,
    Stop,
    Timer,
    User(serde_json::Value),
}

#[derive(Debug)]
pub enum StrategyMessage {
    MarketDataL2 { sub: SubscriptionId, msg: OrderBookL2Update },
    MarketDataTrade { sub: SubscriptionId, msg: TradeUpdate },
    Execution { account_id: i64, exec: XExecution },
    Position { account_id: i64, market_id: i64, pos: Position },
    Obs { sub: SubscriptionId, snapshot: OrderBookSnapshot },
    OrderResponse(OrderResponse),
    Control(StrategyControl),
}

pub struct StrategyIo<'a> {
    pub env: &'a dyn AppEnvironment,
    pub order_txs: std::collections::HashMap<String, tokio::sync::mpsc::Sender<OrderRequest>>, // symbol -> tx
}

#[async_trait]
pub trait Strategy: Send + Sync {
    type Config: DeserializeOwned + Send + Sync + 'static;

    fn name(&self) -> &'static str;

    /// Declare subscriptions and initial timers; no background tasks here
    async fn configure(&mut self, reg: &mut StrategyRegistrar, ctx: &StrategyContext, cfg: &Self::Config) -> AppResult<()>;

    /// Typed handlers (sync, executed on the strategy task)
    fn on_l2(&mut self, _sub: SubscriptionId, _msg: OrderBookL2Update, _io: &mut StrategyIo) {}
    fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}
    fn on_execution(&mut self, _account_id: i64, _exec: XExecution, _io: &mut StrategyIo) {}
    fn on_position(&mut self, _account_id: i64, _market_id: i64, _pos: Position, _io: &mut StrategyIo) {}
    fn on_obs(&mut self, _sub: SubscriptionId, _snapshot: OrderBookSnapshot, _io: &mut StrategyIo) {}
    fn on_control(&mut self, _c: StrategyControl, _io: &mut StrategyIo) {}
}

pub struct StrategyRegistrar<'a> {
    env: &'a dyn AppEnvironment,
    mailbox_tx: tokio::sync::mpsc::Sender<StrategyMessage>,
    next_id: u32,
    pending_subs: Vec<(SubscriptionId, SubscriptionSpec)>,
    account_params: Vec<BinanceAccountParams>,
}

impl<'a> StrategyRegistrar<'a> {
    pub fn new(env: &'a dyn AppEnvironment, mailbox_tx: tokio::sync::mpsc::Sender<StrategyMessage>) -> Self {
        Self { env, mailbox_tx, next_id: 1, pending_subs: Vec::new(), account_params: Vec::new() }
    }

    pub fn subscribe_l2(&mut self, spec: SubscriptionSpec) -> SubscriptionId {
        let id = SubscriptionId(self.next_id);
        self.next_id += 1;
        self.pending_subs.push((id, spec));
        id
    }

    pub fn subscribe_trades(&mut self, spec: SubscriptionSpec) -> SubscriptionId {
        let id = SubscriptionId(self.next_id);
        self.next_id += 1;
        self.pending_subs.push((id, spec));
        id
    }

    pub fn subscribe_obs(&mut self, spec: SubscriptionSpec) -> SubscriptionId {
        // Keep OBS logical type here; environment will convert to L2 for execution and emit OBS snapshots
        let id = SubscriptionId(self.next_id);
        self.next_id += 1;
        self.pending_subs.push((id, spec));
        id
    }

    pub fn set_timer(&mut self, _every: std::time::Duration) {
        // Placeholder: timers can be implemented by spawning a tokio interval that posts Control::Timer
        let _ = &self.env; // keep for future use
    }

    pub fn subscribe_binance_futures_accounts(&mut self, params: BinanceAccountParams) {
        self.account_params.push(params);
    }

    pub fn take_pending(self) -> (tokio::sync::mpsc::Sender<StrategyMessage>, Vec<(SubscriptionId, SubscriptionSpec)>, Vec<BinanceAccountParams>) {
        (self.mailbox_tx, self.pending_subs, self.account_params)
    }
}

/// Runs a single-threaded strategy with a mailbox and forwards typed updates
pub struct StrategyRunner;

impl StrategyRunner {
    pub async fn run<S: Strategy>(
        mut strategy: S,
        ctx: StrategyContext,
        cfg: S::Config,
    ) -> AppResult<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StrategyMessage>(ctx.env.channel_capacity());
        let mut registrar = StrategyRegistrar::new(ctx.env.as_ref(), tx);
        strategy.configure(&mut registrar, &ctx, &cfg).await?;
        let (tx, subs, accounts) = registrar.take_pending();

        // Build mapping from (stream_type, exchange_id, instrument) -> sub id
        let mut key_to_subid: HashMap<(CoreStreamType, ExchangeId, String), SubscriptionId> = HashMap::new();
        let mut specs: Vec<SubscriptionSpec> = Vec::new();
        for (sid, spec) in subs.into_iter() {
            key_to_subid.insert((spec.stream_type, spec.exchange, spec.instrument.clone()), sid);
            specs.push(spec);
        }

        // Start subscriptions via environment and forward to strategy mailbox
        let mut md_rx = ctx.env.start_subscriptions(specs);
        let forward_tx = tx.clone();
        tokio::spawn(async move {
            while let Some(sd) = md_rx.recv().await {
                match sd {
                    StreamData::L2(msg) => {
                        let key = (CoreStreamType::L2, msg.exchange, msg.ticker.clone());
                        if let Some(&sid) = key_to_subid.get(&key) {
                            let _ = forward_tx.try_send(StrategyMessage::MarketDataL2 { sub: sid, msg });
                        }
                    }
                    StreamData::Trade(msg) => {
                        let key = (CoreStreamType::Trade, msg.exchange, msg.ticker.clone());
                        if let Some(&sid) = key_to_subid.get(&key) {
                            let _ = forward_tx.try_send(StrategyMessage::MarketDataTrade { sub: sid, msg });
                        }
                    }
                    StreamData::Obs(snapshot) => {
                        let key = (CoreStreamType::Obs, snapshot.exchange_id, snapshot.symbol.clone());
                        if let Some(&sid) = key_to_subid.get(&key) {
                            let _ = forward_tx.try_send(StrategyMessage::Obs { sub: sid, snapshot });
                        }
                    }
                }
            }
        });

        // Start account runners and forward executions/positions into strategy mailbox
        let mut order_txs: HashMap<String, tokio::sync::mpsc::Sender<OrderRequest>> = HashMap::new();
        if !accounts.is_empty() {
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

            for params in accounts.into_iter() {
                let adapter = std::sync::Arc::new(BinanceFuturesAccountAdapter::new(
                    params.api_key.clone(),
                    params.secret.clone(),
                    params.symbols.clone(),
                    params.ed25519_key.clone(),
                    params.ed25519_secret.clone(),
                ));
                for symbol in params.symbols.iter().cloned() {
                    let adapter = adapter.clone();
                    let pool = pool.clone();
                    let forward_tx = tx.clone();
                    let account_id = params.account_id;
                    let start_epoch_ts = params.start_epoch_ts;
                    let fee_bps = params.fee_bps;
                    let contract_size = params.contract_size;
                    let market_id = XMarketId::make(ExchangeId::BinanceFutures, &symbol);
                    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<OrderRequest>(1024);
                    order_txs.insert(symbol.clone(), req_tx);
                    let adapter_req = adapter.clone();
                    tokio::spawn(async move {
                        let mut account = AccountState::new(
                            account_id,
                            ExchangeId::BinanceFutures,
                            Box::new(PostgresExecutionsDatastore::new(pool)),
                            adapter,
                        );
                        // Forward exec/pos into strategy mailbox
                        let mut exec_rx = account.subscribe_executions();
                        let mut pos_rx = account.subscribe_positions();
                        let mut resp_rx = account.subscribe_order_responses();
                        let ftx = forward_tx.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    Ok(exec) = exec_rx.recv() => {
                                        let _ = ftx.try_send(StrategyMessage::Execution { account_id, exec });
                                    }
                                    Ok((mid, pos)) = pos_rx.recv() => {
                                        let _ = ftx.try_send(StrategyMessage::Position { account_id, market_id: mid, pos });
                                    }
                                    Ok(resp) = resp_rx.recv() => {
                                        let _ = ftx.try_send(StrategyMessage::OrderResponse(resp));
                                    }
                                    else => break,
                                }
                            }
                        });
                        // Bridge order requests to exchange adapter API directly
                        let ftx2 = forward_tx.clone();
                        tokio::spawn(async move {
                            while let Some(req) = req_rx.recv().await {
                                match adapter_req.send_request(req).await {
                                    Ok(resp) => { let _ = ftx2.try_send(StrategyMessage::OrderResponse(resp)); }
                                    Err(e) => { log::error!("[StrategyRunner] send_request error for {}: {}", market_id, e); }
                                }
                            }
                        });
                        if let Err(e) = account.run_market(market_id, start_epoch_ts, fee_bps, contract_size).await {
                            log::error!("[StrategyRunner] run_market error for {}: {}", symbol, e);
                        }
                    });
                }
            }
        }

        // Single-threaded dispatch loop
        let mut io = StrategyIo { env: ctx.env.as_ref(), order_txs };
        while let Some(m) = rx.recv().await {
            match m {
                StrategyMessage::MarketDataL2 { sub, msg } => strategy.on_l2(sub, msg, &mut io),
                StrategyMessage::MarketDataTrade { sub, msg } => strategy.on_trade(sub, msg, &mut io),
                StrategyMessage::Execution { account_id, exec } => strategy.on_execution(account_id, exec, &mut io),
                StrategyMessage::Position { account_id, market_id, pos } => strategy.on_position(account_id, market_id, pos, &mut io),
                StrategyMessage::Obs { sub, snapshot } => strategy.on_obs(sub, snapshot, &mut io),
                StrategyMessage::OrderResponse(_r) => {},
                StrategyMessage::Control(c) => strategy.on_control(c, &mut io),
            }
        }

        Ok(())
    }
}

// no-op mapper removed; we use canonical ExchangeId directly
