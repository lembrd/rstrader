use std::collections::{HashMap, VecDeque};
use std::cmp::Ordering;

use crate::xcommons::error::{Result, AppError};
use crate::xcommons::oms::{Side, XExecution, OrderRequest, OrderResponse};
use crate::xcommons::position::Position;
use crate::xcommons::oms::{OrderStatus, OrderMode, TimeInForce};
use crate::xcommons::types::ExchangeId;
use async_trait::async_trait;
use deadpool_postgres::Pool;
use tokio::sync::broadcast;
use std::time::Duration;

/// Storage abstraction for executions with idempotent upsert semantics and ordered range queries
#[async_trait]
pub trait ExecutionsDatastore: Send + Sync {
    /// Persist execution; duplicate keys should be ignored (idempotent)
    async fn upsert(&mut self, exec: &XExecution) -> Result<()>;

    /// Query ordered executions for account and market strictly greater than the given restart point
    async fn load_range(
        &self,
        account_id: i64,
        market_id: i64,
        gt_ts: i64,
        gt_seq: Option<i64>,
    ) -> Result<Vec<XExecution>>;

    /// Compute last applied restart point
    async fn last_point(&self, account_id: i64, market_id: i64) -> Result<Option<(i64, Option<i64>)>>;
}

/// In-memory implementation for prototyping and tests
pub struct InMemoryExecutionsDatastore {
    // key: (account_id, market_id) -> Vec of executions ordered by (timestamp, seq, native_execution_id)
    data: HashMap<(i64, i64), Vec<XExecution>>,
    // idempotency set: (account_id, market_id, native_execution_id, timestamp)
    seen: std::collections::HashSet<(i64, i64, String, i64)>,
}

impl InMemoryExecutionsDatastore {
    pub fn new() -> Self {
        Self { data: HashMap::new(), seen: std::collections::HashSet::new() }
    }
}

impl Default for InMemoryExecutionsDatastore { fn default() -> Self { Self::new() } }

#[async_trait]
impl ExecutionsDatastore for InMemoryExecutionsDatastore {
    async fn upsert(&mut self, exec: &XExecution) -> Result<()> {
        let key = (exec.account_id, exec.market_id, exec.native_execution_id.clone(), exec.timestamp);
        if self.seen.contains(&key) {
            return Ok(());
        }
        self.seen.insert(key);
        let bucket = self.data.entry((exec.account_id, exec.market_id)).or_default();
        bucket.push(exec.clone());
        // Maintain deterministic order
        bucket.sort_by(|a, b| {
            match a.timestamp.cmp(&b.timestamp) {
                Ordering::Equal => match a.native_execution_id.cmp(&b.native_execution_id) {
                    Ordering::Equal => a.rcv_timestamp.cmp(&b.rcv_timestamp),
                    other => other,
                },
                other => other,
            }
        });
        Ok(())
    }

    async fn load_range(&self, account_id: i64, market_id: i64, gt_ts: i64, _gt_seq: Option<i64>) -> Result<Vec<XExecution>> {
        let mut out = Vec::new();
        if let Some(bucket) = self.data.get(&(account_id, market_id)) {
            for e in bucket.iter() {
                if e.timestamp > gt_ts { out.push(e.clone()); }
            }
        }
        Ok(out)
    }

    async fn last_point(&self, account_id: i64, market_id: i64) -> Result<Option<(i64, Option<i64>)>> {
        if let Some(bucket) = self.data.get(&(account_id, market_id)) {
            if let Some(last) = bucket.last() {
                return Ok(Some((last.timestamp, None)));
            }
        }
        Ok(None)
    }
}

/// Postgres-backed executions datastore
pub struct PostgresExecutionsDatastore {
    pool: Pool,
}

impl PostgresExecutionsDatastore {
    pub fn new(pool: Pool) -> Self { Self { pool } }
}

#[async_trait]
impl ExecutionsDatastore for PostgresExecutionsDatastore {
    async fn upsert(&mut self, exec: &XExecution) -> Result<()> {
        // Persist only trade executions; ignore others like cancel
        if exec.exec_type != crate::xcommons::oms::ExecutionType::XTrade {
            return Ok(());
        }
        let conn = self.pool.get().await.map_err(|e| AppError::io(format!("pg pool: {}", e)))?;
        let stmt = r#"
            INSERT INTO executions (account_id, exchange, instrument, xmarket_id, exchange_execution_id, exchange_ts, side, execution_type, qty, price, fee, fee_currency, live_stream, exchange_sequence)
            VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8,
                $9::float8, $10::float8, $11::float8, $12, $13, $14
            )
            ON CONFLICT (account_id, xmarket_id, exchange_execution_id, exchange_ts) DO UPDATE SET
                instrument = EXCLUDED.instrument,
                fee = EXCLUDED.fee,
                fee_currency = EXCLUDED.fee_currency,
                exchange_sequence = EXCLUDED.exchange_sequence
        "#;
        let instrument = exec.metadata.get("symbol").cloned().unwrap_or_else(|| "".to_string());
        let side: i16 = match exec.side { Side::Buy => 1, Side::Sell => -1, Side::Unknown => 0 };
        let exec_type: i32 = exec.exec_type as i32;
        let live_stream = exec.metadata.get("live").map(|v| v == "true").unwrap_or(false);
        let exchange = format!("{}", ExchangeId::BinanceFutures);
        let seq: Option<i64> = exec.native_execution_id.parse::<i64>().ok();
        // Bind TIMESTAMPTZ directly to avoid float rounding issues that can defeat unique constraints
        let ts_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(exec.timestamp)
            .ok_or_else(|| AppError::parse(format!("invalid exec.timestamp micros: {}", exec.timestamp)))?;
        let fee_currency: Option<String> = exec.metadata.get("fee_asset").cloned();
        conn.execute(
            stmt,
            &[
                &exec.account_id,
                &exchange,
                &instrument,
                &exec.market_id,
                &exec.native_execution_id,
                &ts_utc,
                &side,
                &exec_type,
                &exec.last_qty,
                &exec.last_px,
                &exec.fee,
                &fee_currency,
                &live_stream,
                &seq,
            ],
        )
        .await
        .map_err(|e| AppError::io(format!("pg exec: {}", e)))?;
        Ok(())
    }

    async fn load_range(&self, account_id: i64, market_id: i64, gt_ts: i64, _gt_seq: Option<i64>) -> Result<Vec<XExecution>> {
        let conn = self.pool.get().await.map_err(|e| AppError::io(format!("pg pool: {}", e)))?;
        let stmt = r#"
            SELECT exchange_ts, xmarket_id, account_id, side, qty::float8, price::float8, fee::float8, exchange_execution_id
            FROM executions
            WHERE account_id=$1 AND xmarket_id=$2 AND exchange_ts > $3
            ORDER BY exchange_ts ASC
        "#;
        let gt_dt = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(gt_ts)
            .ok_or_else(|| AppError::parse(format!("invalid gt_ts micros: {}", gt_ts)))?;
        let rows = conn.query(stmt, &[&account_id, &market_id, &gt_dt]).await.map_err(|e| AppError::io(format!("pg query: {}", e)))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let timestamp: chrono::DateTime<chrono::Utc> = r.get(0);
            let ts_us = timestamp.timestamp_micros();
            let market_id: i64 = r.get(1);
            let account_id: i64 = r.get(2);
            let side_i: i16 = r.get(3);
            let side = if side_i > 0 { Side::Buy } else if side_i < 0 { Side::Sell } else { Side::Unknown };
            let last_qty: f64 = r.get(4);
            let last_px: f64 = r.get(5);
            let fee: f64 = r.try_get::<usize, Option<f64>>(6).ok().flatten().unwrap_or(0.0);
            let native_execution_id: String = r.get(7);
            // Defaults for fields we don't persist yet
            let exec_type = crate::xcommons::oms::ExecutionType::XTrade;
            let cl_ord_id: i64 = -1;
            let orig_cl_ord_id: i64 = -1;
            let ord_status = OrderStatus::OStatusFilled;
            let leaves_qty: f64 = 0.0;
            let ord_qty: f64 = last_qty;
            let ord_price: f64 = last_px;
            let ord_mode = OrderMode::MLimit;
            let tif = TimeInForce::TifGoodTillCancel;
            let is_taker: bool = true;
            out.push(XExecution { timestamp: ts_us, rcv_timestamp: ts_us, market_id, account_id, exec_type, side, native_ord_id: String::new(), cl_ord_id, orig_cl_ord_id, ord_status, last_qty, last_px, leaves_qty, ord_qty, ord_price, ord_mode, tif, fee, native_execution_id, metadata: Default::default(), is_taker });
        }
        Ok(out)
    }

    async fn last_point(&self, account_id: i64, market_id: i64) -> Result<Option<(i64, Option<i64>)>> {
        let conn = self.pool.get().await.map_err(|e| AppError::io(format!("pg pool: {}", e)))?;
        let stmt = r#"
            SELECT max(exchange_ts) as last_ts
            FROM executions WHERE account_id=$1 AND xmarket_id=$2
        "#;
        let row = conn.query_one(stmt, &[&account_id, &market_id]).await.map_err(|e| AppError::io(format!("pg query: {}", e)))?;
        let last_ts_opt: Option<chrono::DateTime<chrono::Utc>> = row.try_get(0).ok();
        Ok(last_ts_opt.map(|dt| (dt.timestamp_micros(), None)))
    }
}

/// Exchange-specific adapter for fetching historical executions and subscribing to live user stream
#[allow(unused_variables)]
#[async_trait]
pub trait ExchangeAccountAdapter: Send + Sync {
    /// Fetch historical executions strictly after the provided restart point and up to now
    async fn fetch_historical(&self, account_id: i64, market_id: i64, gt_ts: i64, gt_seq: Option<i64>) -> Result<Vec<XExecution>>;

    /// Subscribe to live executions stream; should yield idempotent XExecution items
    async fn subscribe_live(&self, account_id: i64, market_id: i64) -> Result<tokio::sync::mpsc::Receiver<XExecution>>;

    /// Send an order management request (post, cancel, cancel all) and return a unified OrderResponse
    async fn send_request(&self, req: OrderRequest) -> Result<OrderResponse>;
}

#[derive(Clone, Debug)]
pub enum RecoveryMode {
    /// Perform full REST backfill + replay and continue (current behavior)
    Restore,
    /// Perform full REST backfill + replay for safety, then exit process to be restarted by supervisor
    Exit,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RestartPoint {
    pub last_ts: i64,
    pub last_seq: Option<i64>,
}

pub struct AccountState {
    pub account_id: i64,
    pub exchange: crate::xcommons::types::ExchangeId,
    datastore: Box<dyn ExecutionsDatastore>,
    adapter: std::sync::Arc<dyn ExchangeAccountAdapter>,
    // state per market
    positions: HashMap<i64, Position>,
    // buffer for live executions until switch to online
    live_buffer: HashMap<i64, VecDeque<XExecution>>,
    // broadcast channels for external subscribers
    exec_tx: broadcast::Sender<XExecution>,
    pos_tx: broadcast::Sender<(i64, Position)>,
    /// Forward REST order responses to strategy via broadcast
    resp_tx: broadcast::Sender<OrderResponse>,
    /// Recovery policy on user-stream disconnects
    recovery_mode: RecoveryMode,
}

impl AccountState {
    pub fn new(
        account_id: i64,
        exchange: crate::xcommons::types::ExchangeId,
        datastore: Box<dyn ExecutionsDatastore>,
        adapter: std::sync::Arc<dyn ExchangeAccountAdapter>,
        recovery_mode: RecoveryMode,
    ) -> Self {
        Self {
            account_id,
            exchange,
            datastore,
            adapter,
            positions: HashMap::new(),
            live_buffer: HashMap::new(),
            exec_tx: broadcast::channel(1024).0,
            pos_tx: broadcast::channel(1024).0,
            resp_tx: broadcast::channel(1024).0,
            recovery_mode,
        }
    }

    /// Build positions from epoch without switching to online mode. Returns after applying all persisted execs.
    pub async fn build_from_epoch(&mut self, market_id: i64, start_epoch_ts: i64, fee_bps: f64, contract_size: f64) -> Result<()> {
        log::info!("[AccountState] build_from_epoch account={} market_id={} start_epoch_ts(us)={}",
            self.account_id, market_id, start_epoch_ts);
        // Backfill via REST strictly greater than max(last_point, start_epoch_ts)
        if let Some((lp_ts, lp_seq)) = self.datastore.last_point(self.account_id, market_id).await? {
            let gt_ts = lp_ts.max(start_epoch_ts);
            let lp_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(lp_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            let gt_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(gt_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            log::info!(
                "[AccountState] last_point from PG: ts(us)={} ({}) seq={:?}; using gt_ts(us)={} ({}) (clamped by epoch)",
                lp_ts, lp_rfc, lp_seq, gt_ts, gt_rfc
            );
            let hist = self.adapter.fetch_historical(self.account_id, market_id, gt_ts, lp_seq).await?;
            if !hist.is_empty() {
                let first_ts = hist.first().map(|e| e.timestamp).unwrap_or(0);
                let last_ts = hist.last().map(|e| e.timestamp).unwrap_or(0);
                let first_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(first_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                let last_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(last_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                log::info!(
                    "[AccountState] fetched historical execs: count={} first_ts(us)={} ({}) last_ts(us)={} ({})",
                    hist.len(), first_ts, first_rfc, last_ts, last_rfc
                );
            } else {
                log::info!("[AccountState] fetched historical execs: count=0");
            }
            for e in hist.iter() { let _ = self.datastore.upsert(e).await?; }
        }

        // Replay from epoch to build current positions
        let base_ts = start_epoch_ts.saturating_sub(1);
        let base_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(base_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
        let mut execs = self.datastore.load_range(self.account_id, market_id, base_ts, None).await?;
        if !execs.is_empty() {
            let first_ts = execs.first().map(|e| e.timestamp).unwrap_or(0);
            let last_ts = execs.last().map(|e| e.timestamp).unwrap_or(0);
            let first_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(first_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            let last_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(last_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            log::info!(
                "[AccountState] replay from PG: count={} first_ts(us)={} ({}) last_ts(us)={} ({}) base_ts(us)={} ({})",
                execs.len(), first_ts, first_rfc, last_ts, last_rfc, base_ts, base_rfc
            );
        } else {
            log::info!("[AccountState] replay from PG: count=0 base_ts(us)={} ({})", base_ts, base_rfc);
        }
        Self::stable_sort_executions(&mut execs);
        self.apply_executions(market_id, &execs, fee_bps, contract_size)?;
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            log::info!(
                "[AccountState] snapshot market_id={} amount={} avp={} realized={} fees={} trades_count={} volume={} bps={}",
                market_id, snapshot.amount, snapshot.avp, snapshot.realized, snapshot.fees, snapshot.trades_count, snapshot.quote_volume, snapshot.bps()
            );
        }
        Ok(())
    }

    /// Restore state then go online per docs/trade_account.md
    pub async fn run_market(&mut self, market_id: i64, start_epoch_ts: i64, fee_bps: f64, contract_size: f64) -> Result<()> {
        log::info!("[AccountState] run_market account={} market_id={} start_epoch_ts(us)={} fee_bps={} contract_size={}",
            self.account_id, market_id, start_epoch_ts, fee_bps, contract_size);
        // Step 1: subscribe to live; buffer into a local channel
        let mut rx = self.adapter.subscribe_live(self.account_id, market_id).await?;
        let (tx_buf, mut rx_buf) = tokio::sync::mpsc::channel::<XExecution>(1024);
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await { let _ = tx_buf.send(e).await; }
        });

        // Step 2: find restart point and clamp to epoch
        let last_point = self.datastore.last_point(self.account_id, market_id).await?;
        let (gt_ts, gt_seq) = match last_point {
            Some((ts, seq)) => {
                let clamped = ts.max(start_epoch_ts);
                let ts_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                let clamped_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(clamped).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                log::info!(
                    "[AccountState] last_point from PG: ts(us)={} ({}) seq={:?}; using gt_ts(us)={} ({}) (clamped by epoch)",
                    ts, ts_rfc, seq, clamped, clamped_rfc
                );
                (clamped, seq)
            }
            None => (start_epoch_ts, None),
        };

        // Step 3: backfill via REST and persist idempotently
        let hist = self.adapter.fetch_historical(self.account_id, market_id, gt_ts, gt_seq).await?;
        if !hist.is_empty() {
            let first_ts = hist.first().map(|e| e.timestamp).unwrap_or(0);
            let last_ts = hist.last().map(|e| e.timestamp).unwrap_or(0);
            let first_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(first_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            let last_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(last_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            let gt_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(gt_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            log::info!(
                "[AccountState] fetched historical execs: count={} first_ts(us)={} ({}) last_ts(us)={} ({}) from_gt_ts(us)={} ({})",
                hist.len(), first_ts, first_rfc, last_ts, last_rfc, gt_ts, gt_rfc
            );
        } else {
            let gt_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(gt_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            log::info!("[AccountState] fetched historical execs: count=0 from_gt_ts(us)={} ({})", gt_ts, gt_rfc);
        }
        for e in hist.iter() { let _ = self.datastore.upsert(e).await?; }

        // Step 4: replay from datastore to build in-memory state
        // IMPORTANT: Build current positions from epoch. Replay from epoch regardless of last_point
        let base_ts = start_epoch_ts.saturating_sub(1);
        let base_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(base_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
        let mut execs = self.datastore.load_range(self.account_id, market_id, base_ts, None).await?;
        if !execs.is_empty() {
            let first_ts = execs.first().map(|e| e.timestamp).unwrap_or(0);
            let last_ts = execs.last().map(|e| e.timestamp).unwrap_or(0);
            let first_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(first_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            let last_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(last_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
            log::info!(
                "[AccountState] replay from PG: count={} first_ts(us)={} ({}) last_ts(us)={} ({}) base_ts(us)={} ({})",
                execs.len(), first_ts, first_rfc, last_ts, last_rfc, base_ts, base_rfc
            );
        } else {
            log::info!("[AccountState] replay from PG: count=0 base_ts(us)={} ({})", base_ts, base_rfc);
        }
        Self::stable_sort_executions(&mut execs);
        self.apply_executions(market_id, &execs, fee_bps, contract_size)?;
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            log::info!(
                "[AccountState] snapshot market_id={} amount={} avp={} realized={} fees={} trades_count={} volume={} bps={}",
                market_id, snapshot.amount, snapshot.avp, snapshot.realized, snapshot.fees, snapshot.trades_count, snapshot.quote_volume, snapshot.bps()
            );
        }
        // Emit snapshot after initial build so strategy sees current state even if no new execs
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            let _ = self.pos_tx.send((market_id, snapshot));
        }

        // Step 5: switch to online: move buffered items into per-market buffer
        self.live_buffer.entry(market_id).or_default();
        while let Ok(e) = rx_buf.try_recv() { self.live_buffer.get_mut(&market_id).unwrap().push_back(e); }
        let last_applied_ts = execs.last().map(|e| e.timestamp).unwrap_or(start_epoch_ts);
        let last_applied_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(last_applied_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
        log::info!("[AccountState] last_applied_ts(us) after initial replay: {} ({})", last_applied_ts, last_applied_rfc);
        while let Some(front) = self.live_buffer.get(&market_id).and_then(|q| q.front()).cloned() {
            if front.timestamp <= last_applied_ts { self.live_buffer.get_mut(&market_id).unwrap().pop_front(); } else { break; }
        }
        // drain remaining and continue online
        let mut online: Vec<XExecution> = self.live_buffer.get_mut(&market_id).unwrap().drain(..).collect();
        Self::stable_sort_executions(&mut online);
        self.apply_executions(market_id, &online, fee_bps, contract_size)?;
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            log::info!(
                "[AccountState] snapshot market_id={} amount={} avp={} realized={} fees={} trades_count={} volume={} bps={}",
                market_id, snapshot.amount, snapshot.avp, snapshot.realized, snapshot.fees, snapshot.trades_count, snapshot.quote_volume, snapshot.bps()
            );
        }
        // Emit snapshot after switching online
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            let _ = self.pos_tx.send((market_id, snapshot));
        }

        // continue consuming live until channel closes (disconnect)
        while let Some(e) = rx_buf.recv().await {
            let _ = self.datastore.upsert(&e).await?;
            self.apply_executions(market_id, std::slice::from_ref(&e), fee_bps, contract_size)?;
        }

        // Live was disconnected. Perform full restore per docs:
        // - Determine last point
        // - Backfill strictly greater than last point via REST
        // - Replay from epoch to rebuild deterministic state
        let last_point = self.datastore.last_point(self.account_id, market_id).await?;
        if let Some((gt_ts, gt_seq)) = last_point {
            let hist = self.adapter.fetch_historical(self.account_id, market_id, gt_ts, gt_seq).await?;
            if !hist.is_empty() {
                let first_ts = hist.first().map(|e| e.timestamp).unwrap_or(0);
                let last_ts = hist.last().map(|e| e.timestamp).unwrap_or(0);
                log::info!("[AccountState] reconnect fetched historical: count={} first_ts(us)={} last_ts(us)={} from_gt_ts(us)={}",
                    hist.len(), first_ts, last_ts, gt_ts);
            } else {
                log::info!("[AccountState] reconnect fetched historical: count=0 from_gt_ts(us)={}", gt_ts);
            }
            for e in hist.iter() { let _ = self.datastore.upsert(e).await?; }
        }
        let base_ts = start_epoch_ts.saturating_sub(1);
        let mut execs = self.datastore.load_range(self.account_id, market_id, base_ts, None).await?;
        Self::stable_sort_executions(&mut execs);
        // Reset in-memory position then re-apply
        self.positions.insert(market_id, Position::new(fee_bps / 10_000.0, contract_size));
        self.apply_executions(market_id, &execs, fee_bps, contract_size)?;
        // Emit snapshot after full restore
        {
            let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
            let _ = self.pos_tx.send((market_id, snapshot));
        }

        // If configured to exit on disconnect, fail fast after restore
        if let RecoveryMode::Exit = self.recovery_mode {
            log::error!(
                "[AccountState] user stream disconnected; performed restore; exiting process due to recovery_mode=Exit"
            );
            // Small delay to allow logs/metrics to flush
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(1);
        }

        // Re-subscribe in a loop without recursive futures (Restore mode)
        loop {
            // subscribe
            let mut rx = self.adapter.subscribe_live(self.account_id, market_id).await?;
            let (tx_buf, mut rx_buf) = tokio::sync::mpsc::channel::<XExecution>(1024);
            tokio::spawn(async move { while let Some(e) = rx.recv().await { let _ = tx_buf.send(e).await; } });

            // consume until disconnect
            while let Some(e) = rx_buf.recv().await {
                let _ = self.datastore.upsert(&e).await?;
                self.apply_executions(market_id, std::slice::from_ref(&e), fee_bps, contract_size)?;
            }

            // full restore again
            if let Some((gt_ts, gt_seq)) = self.datastore.last_point(self.account_id, market_id).await? {
                let hist = self.adapter.fetch_historical(self.account_id, market_id, gt_ts, gt_seq).await?;
                if !hist.is_empty() {
                    let first_ts = hist.first().map(|e| e.timestamp).unwrap_or(0);
                    let last_ts = hist.last().map(|e| e.timestamp).unwrap_or(0);
                    log::info!("[AccountState] loop restore fetched historical: count={} first_ts(us)={} last_ts(us)={} from_gt_ts(us)={}",
                        hist.len(), first_ts, last_ts, gt_ts);
                } else {
                    log::info!("[AccountState] loop restore fetched historical: count=0 from_gt_ts(us)={}", gt_ts);
                }
                for e in hist.iter() { let _ = self.datastore.upsert(e).await?; }
            }
            let base_ts = start_epoch_ts.saturating_sub(1);
            let mut execs = self.datastore.load_range(self.account_id, market_id, base_ts, None).await?;
            Self::stable_sort_executions(&mut execs);
            self.positions.insert(market_id, Position::new(fee_bps / 10_000.0, contract_size));
            self.apply_executions(market_id, &execs, fee_bps, contract_size)?;
            // Emit snapshot after each restore cycle
            {
                let snapshot = self.ensure_position(market_id, fee_bps, contract_size).clone();
                log::info!(
                    "[AccountState] snapshot market_id={} amount={} avp={} realized={} fees={} trades_count={} volume={} bps={}",
                    market_id, snapshot.amount, snapshot.avp, snapshot.realized, snapshot.fees, snapshot.trades_count, snapshot.quote_volume, snapshot.bps()
                );
                let _ = self.pos_tx.send((market_id, snapshot));
            }
            // loop continues to re-subscribe
        }
    }

    fn stable_sort_executions(execs: &mut Vec<XExecution>) {
        execs.sort_by(|a, b| {
            match a.timestamp.cmp(&b.timestamp) {
                Ordering::Equal => a.native_execution_id.cmp(&b.native_execution_id),
                other => other,
            }
        });
    }

    fn ensure_position(&mut self, market_id: i64, fee_bps: f64, contract_size: f64) -> &mut Position {
        // Default to computed fees from config; will be overridden per-trade if exchange fee is provided
        let fee_percent = fee_bps / 10_000.0;
        self.positions.entry(market_id).or_insert_with(|| Position::new(fee_percent, contract_size))
    }

    fn apply_executions(&mut self, market_id: i64, execs: &[XExecution], fee_bps: f64, contract_size: f64) -> Result<()> {
        for e in execs {
            if !e.exec_type.is_position_change() { continue; }
            let qty_signed = match e.side { Side::Buy => e.last_qty.abs(), Side::Sell => -e.last_qty.abs(), Side::Unknown => 0.0 };
            if qty_signed == 0.0 { continue; }
            // apply into position then drop the mutable borrow before broadcasting
            let snapshot: Position = {
                let pos = self.ensure_position(market_id, fee_bps, contract_size);
                let fee_override = if e.fee != 0.0 { Some(e.fee) } else { None };
                pos.trade_with_fee(qty_signed, e.last_px, fee_override);
                pos.clone()
            };
            // broadcast execution and updated position snapshot (best-effort)
            if log::log_enabled!(log::Level::Debug) {
                log::debug!(
                    "[AccountState] apply exec ts={} mid={} side={:?} qty={} px={} native_id={}",
                    e.timestamp, e.market_id, e.side, e.last_qty, e.last_px, e.native_execution_id
                );
            }
            let _ = self.exec_tx.send(e.clone());
            let _ = self.pos_tx.send((market_id, snapshot));
        }
        Ok(())
    }

    pub fn position(&self, market_id: i64) -> Option<&Position> { self.positions.get(&market_id) }

    pub fn positions_snapshot(&self) -> HashMap<i64, Position> { self.positions.clone() }

    // Subscriptions for external consumers (strategy)
    pub fn subscribe_executions(&self) -> broadcast::Receiver<XExecution> { self.exec_tx.subscribe() }
    pub fn subscribe_positions(&self) -> broadcast::Receiver<(i64, Position)> { self.pos_tx.subscribe() }
    pub fn subscribe_order_responses(&self) -> broadcast::Receiver<OrderResponse> { self.resp_tx.subscribe() }

    pub async fn send_request(&self, req: OrderRequest) -> Result<OrderResponse> {
        let resp = self.adapter.send_request(req).await?;
        let _ = self.resp_tx.send(resp.clone());
        Ok(resp)
    }
}


