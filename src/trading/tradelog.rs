use crate::xcommons::error::{AppError, Result};
use crate::xcommons::oms::{ExecutionType, OrderResponseStatus, OrderStatus, Side};
use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

#[repr(i16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogEventType {
    RequestOrderNew = 10,
    RequestOrderCancel = 11,
    RequestOrderAmend = 12,
    RequestCancelAll = 13,

    ApiResponse = 20,

    ApiEventExecution = 30,
    ApiEventOrderUpdate = 31,
    ApiEventTradeLite = 32,

    ConnectivityInfo = 90,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeLogEvent {
    pub local_ts_us: i64,
    pub exchange_ts_us: Option<i64>,
    pub event_type: LogEventType,
    pub strategy_id: i64,

    pub req_id: Option<i64>,
    pub cl_ord_id: Option<i64>,
    pub native_ord_id: Option<String>,
    pub market_id: Option<i64>,
    pub account_id: Option<i64>,

    pub exec_type: Option<ExecutionType>,
    pub ord_status: Option<OrderStatus>,
    pub side: Option<Side>,
    pub is_taker: Option<bool>,

    pub last_px: Option<f64>,
    pub last_qty: Option<f64>,
    pub leaves_qty: Option<f64>,
    pub ord_px: Option<f64>,
    pub ord_qty: Option<f64>,
    pub fee: Option<f64>,

    pub response_status: Option<OrderResponseStatus>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,

    pub source: Option<String>,
    pub body: JsonValue,
}

#[derive(Clone)]
pub struct TradeLogHandle {
    tx: mpsc::Sender<TradeLogEvent>,
}

impl TradeLogHandle {
    pub async fn log(&self, mut ev: TradeLogEvent) {
        if ev.local_ts_us == 0 { ev.local_ts_us = chrono::Utc::now().timestamp_micros(); }
        let _ = self.tx.send(ev).await;
    }
}

pub struct TradeLogService {
    rx: mpsc::Receiver<TradeLogEvent>,
    pool: Pool,
    strategy_id: i64,
}

impl TradeLogService {
    pub fn start(pool: Pool, strategy_id: i64) -> TradeLogHandle {
        let (tx, rx) = mpsc::channel::<TradeLogEvent>(8192);
        let svc = Self { rx, pool, strategy_id };
        tokio::spawn(async move { if let Err(e) = svc.run().await { log::error!("[TradeLog] worker error: {}", e); } });
        TradeLogHandle { tx }
    }

    async fn run(mut self) -> Result<()> {
        let mut conn = self.pool.get().await.map_err(|e| AppError::io(format!("pg pool: {}", e)))?;
        // Prepare statement once
        let stmt = conn
            .prepare(
                r#"INSERT INTO tradelog (
                    local_ts_us, exchange_ts_us, event_type,
                    strategy_id,
                    req_id, cl_ord_id, native_ord_id, market_id, account_id,
                    exec_type, ord_status, side, is_taker,
                    last_px, last_qty, leaves_qty, ord_px, ord_qty, fee,
                    response_status, error_code, error_message,
                    source, body
                ) VALUES (
                    $1, $2, $3,
                    $4,
                    $5, $6, $7, $8, $9,
                    $10, $11, $12, $13,
                    $14, $15, $16, $17, $18, $19,
                    $20, $21, $22,
                    $23, $24
                )"#,
            )
            .await
            .map_err(|e| AppError::io(format!("pg prepare tradelog: {}", e)))?;

        while let Some(ev) = self.rx.recv().await {
            // Ensure JSONB parameter uses postgres_types::Json wrapper
            let body_param = postgres_types::Json(ev.body.clone());
            let res = conn
                .execute(
                    &stmt,
                    &[
                        &ev.local_ts_us,
                        &ev.exchange_ts_us,
                        &(ev.event_type as i16),
                        &self.strategy_id,
                        &ev.req_id,
                        &ev.cl_ord_id,
                        &ev.native_ord_id,
                        &ev.market_id,
                        &ev.account_id,
                        &ev.exec_type.map(|v| v as i16),
                        &ev.ord_status.map(|v| v as i16),
                        &ev.side.map(|v| match v { Side::Buy => 1i16, Side::Sell => -1i16, Side::Unknown => 0i16 }),
                        &ev.is_taker,
                        &ev.last_px,
                        &ev.last_qty,
                        &ev.leaves_qty,
                        &ev.ord_px,
                        &ev.ord_qty,
                        &ev.fee,
                        &ev.response_status.map(|v| v as i16),
                        &ev.error_code,
                        &ev.error_message,
                        &ev.source,
                        &body_param,
                    ],
                )
                .await;
            if let Err(e) = res { log::warn!("[TradeLog] insert error: {}", e); }
        }
        Ok(())
    }
}

/// Utilities for building events
pub mod build {
    use super::*;

    pub fn sanitize_credentials(mut v: JsonValue) -> JsonValue {
        match &mut v {
            JsonValue::Object(map) => {
                for key in ["apiKey", "api_key", "secret", "signature", "X-MBX-APIKEY"].iter() {
                    if map.contains_key(*key) { map.insert((*key).to_string(), JsonValue::String("***".to_string())); }
                }
                for (_k, val) in map.iter_mut() { *val = sanitize_credentials(val.take()); }
                JsonValue::Object(map.clone())
            }
            JsonValue::Array(arr) => {
                let clean: Vec<JsonValue> = arr.drain(..).map(sanitize_credentials).collect();
                JsonValue::Array(clean)
            }
            _ => v,
        }
    }

    pub fn event_base(event_type: LogEventType, source: &str, body: JsonValue) -> TradeLogEvent {
        TradeLogEvent {
            local_ts_us: chrono::Utc::now().timestamp_micros(),
            exchange_ts_us: None,
            event_type,
            strategy_id: 0,
            req_id: None,
            cl_ord_id: None,
            native_ord_id: None,
            market_id: None,
            account_id: None,
            exec_type: None,
            ord_status: None,
            side: None,
            is_taker: None,
            last_px: None,
            last_qty: None,
            leaves_qty: None,
            ord_px: None,
            ord_qty: None,
            fee: None,
            response_status: None,
            error_code: None,
            error_message: None,
            source: Some(source.to_string()),
            body,
        }
    }
}


