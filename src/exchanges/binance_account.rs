use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex::encode as hex_encode;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;

use crate::trading::account_state::ExchangeAccountAdapter;
use crate::xcommons::error::{AppError, Result};
use crate::xcommons::oms::{ExecutionType, OrderStatus, Side, TimeInForce, OrderMode, XExecution};
use crate::xcommons::types::ExchangeId;
use crate::xcommons::xmarket_id::XMarketId;
use tokio::time::{sleep, Duration};

pub struct BinanceFuturesAccountAdapter {
    api_key: String,
    secret: String,
    rest: Client,
    symbols: Vec<String>,
    market_to_symbol: HashMap<i64, String>,
    base_url: String,
    ws_base: String,
}

impl BinanceFuturesAccountAdapter {
    pub fn new(api_key: String, secret: String, symbols: Vec<String>) -> Self {
        let market_to_symbol = symbols
            .iter()
            .map(|s| (XMarketId::make(ExchangeId::BinanceFutures, s), s.clone()))
            .collect();
        Self {
            api_key,
            secret,
            rest: Client::new(),
            symbols,
            market_to_symbol,
            base_url: "https://fapi.binance.com".to_string(),
            ws_base: "wss://fstream.binance.com".to_string(),
        }
    }

    fn sign_query(&self, query: &str) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(self.secret.as_bytes()).expect("hmac");
        mac.update(query.as_bytes());
        let sig = mac.finalize().into_bytes();
        hex_encode(sig)
    }
}

#[derive(Debug, Deserialize, Clone)]
struct UserTrade {
    #[serde(rename = "symbol")] symbol: String,
    #[serde(rename = "id")] id: i64,
    #[serde(rename = "orderId")] order_id: i64,
    #[serde(rename = "price")] price: String,
    #[serde(rename = "qty")] qty: String,
    #[serde(rename = "quoteQty")] quote_qty: String,
    #[serde(rename = "realizedPnl")] realized_pnl: String,
    #[serde(rename = "commission")] commission: Option<String>,
    #[serde(rename = "commissionAsset")] commission_asset: Option<String>,
    #[serde(rename = "side")] side: String,
    #[serde(rename = "buyer")] buyer: bool,
    #[serde(rename = "maker")] maker: bool,
    #[serde(rename = "time")] time_ms: i64,
}

#[derive(Debug, Deserialize)]
struct ListenKeyResp { #[serde(rename = "listenKey")] listen_key: String }

#[derive(Debug, Deserialize)]
struct WsRoot {
    #[serde(rename = "e")] event_type: Option<String>,
    #[serde(rename = "E")] event_time: Option<i64>,
    #[serde(rename = "o")] order: Option<WsOrderUpdate>,
}

#[derive(Debug, Deserialize, Clone)]
struct WsOrderUpdate {
    #[serde(rename = "s")] symbol: String,
    #[serde(rename = "i")] order_id: i64,
    #[serde(rename = "S")] side: String,
    #[serde(rename = "o")] order_type: String,
    #[serde(rename = "f")] tif: String,
    #[serde(rename = "x")] exec_type: String,
    #[serde(rename = "X")] order_status: String,
    #[serde(rename = "l")] last_filled_qty: String,
    #[serde(rename = "L")] last_filled_price: String,
    #[serde(rename = "t")] trade_id: i64,
    #[serde(rename = "T")] trade_time_ms: i64,
    #[serde(rename = "m")] is_maker: bool,
    #[serde(rename = "n")] commission: Option<String>,
    #[serde(rename = "N")] commission_asset: Option<String>,
}

#[async_trait]
impl ExchangeAccountAdapter for BinanceFuturesAccountAdapter {
    async fn fetch_historical(&self, account_id: i64, market_id: i64, gt_ts: i64, _gt_seq: Option<i64>) -> Result<Vec<XExecution>> {
        let symbol = self.market_to_symbol.get(&market_id).ok_or_else(|| AppError::config(format!("unknown market_id {}", market_id)))?.clone();
        log::info!(
            "[BinanceFuturesAccountAdapter] fetch_historical account_id={} market_id={} symbol={} gt_ts(us)={} (start_ms={})",
            account_id, market_id, symbol, gt_ts, (gt_ts/1000)+1
        );
        let mut out: Vec<XExecution> = Vec::new();
        let mut start_time_ms: i64 = (gt_ts / 1000) + 1; // convert us → ms, strictly greater
        let mut attempted_recent_fallback = false;
        let limit = 1000i32;
        loop {
            let timestamp = chrono::Utc::now().timestamp_millis();
            let query = format!("symbol={}&startTime={}&limit={}&timestamp={}", symbol, start_time_ms, limit, timestamp);
            let sig = self.sign_query(&query);
            let url = format!("{}/fapi/v1/userTrades?{}&signature={}", self.base_url, query, sig);
            log::debug!("[BinanceFuturesAccountAdapter] userTrades url: {}", url.replace(&self.api_key, "***"));
            let resp = self.rest.get(url)
                .header("X-MBX-APIKEY", &self.api_key)
                .send().await.map_err(|e| AppError::io(format!("binance userTrades: {}", e)))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.bytes().await.unwrap_or_default();
                let txt = String::from_utf8_lossy(&body);
                return Err(AppError::io(format!("userTrades http {}: {}", status, txt)));
            }
            let text = resp.text().await.map_err(|e| AppError::io(format!("userTrades text: {}", e)))?;
            if log::log_enabled!(log::Level::Debug) {
                log::debug!("[BinanceFuturesAccountAdapter] userTrades raw: {}", text);
            }
            let trades: Vec<UserTrade> = serde_json::from_str(&text).map_err(|e| AppError::parse(format!("userTrades json: {}", e)))?;
            log::info!("[BinanceFuturesAccountAdapter] userTrades page count={} start_ms={}", trades.len(), start_time_ms);
            if trades.is_empty() {
                if !attempted_recent_fallback {
                    attempted_recent_fallback = true;
                    // Fallback: pull recent day to sanity-check if API returns anything
                    let recent = timestamp - 86_400_000; // 1 day
                    log::warn!("[BinanceFuturesAccountAdapter] userTrades empty for start_ms={}; retrying with recent 1d start_ms={}", start_time_ms, recent);
                    start_time_ms = recent;
                    continue;
                }
                break;
            }
            for tr in trades.iter() {
                // Convert to XExecution
                let price: f64 = tr.price.parse().unwrap_or(0.0);
                let qty: f64 = tr.qty.parse().unwrap_or(0.0);
                let ts_us = tr.time_ms * 1000;
                let side = match tr.side.as_str() { "BUY" => Side::Buy, "SELL" => Side::Sell, _ => Side::Unknown };
                // Fee: prefer commission (taker/maker fee or rebate). Binance returns negative commission for rebates.
                let fee: f64 = tr.commission.as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let exec = XExecution {
                    timestamp: ts_us,
                    rcv_timestamp: ts_us,
                    market_id,
                    account_id,
                    exec_type: ExecutionType::XTrade,
                    side,
                    native_ord_id: tr.order_id.to_string(),
                    cl_ord_id: -1,
                    orig_cl_ord_id: -1,
                    ord_status: OrderStatus::OStatusFilled,
                    last_qty: qty,
                    last_px: price,
                    leaves_qty: 0.0,
                    ord_qty: qty,
                    ord_price: price,
                    ord_mode: OrderMode::MLimit,
                    tif: TimeInForce::TifGoodTillCancel,
                    fee,
                    native_execution_id: tr.id.to_string(),
                    metadata: {
                        let mut m = std::collections::HashMap::new();
                        m.insert("symbol".to_string(), symbol.clone());
                        if let Some(asset) = tr.commission_asset.clone() { m.insert("fee_asset".to_string(), asset); }
                        m
                    },
                    is_taker: !tr.maker,
                };
                out.push(exec);
            }
            // next page: use time of last trade + 1 ms
            start_time_ms = trades.last().map(|t| t.time_ms + 1).unwrap_or(start_time_ms + 1);
            if trades.len() < limit as usize { break; }
        }
        Ok(out)
    }

    async fn subscribe_live(&self, account_id: i64, market_id: i64) -> Result<mpsc::Receiver<XExecution>> {
        let symbol = self
            .market_to_symbol
            .get(&market_id)
            .ok_or_else(|| AppError::config(format!("unknown market_id {}", market_id)))?
            .clone();

        let (tx, rx) = mpsc::channel::<XExecution>(2048);

        // Clone needed state for background task
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let ws_base = self.ws_base.clone();
        let rest = self.rest.clone();

        tokio::spawn(async move {
            // Create listenKey
            let resp = rest
                .post(format!("{}/fapi/v1/listenKey", base_url))
                .header("X-MBX-APIKEY", &api_key)
                .send()
                .await;
            let listen_key = match resp {
                Ok(resp) if resp.status().is_success() => match resp.json::<ListenKeyResp>().await {
                    Ok(j) => j.listen_key,
                    Err(_) => return,
                },
                _ => return,
            };
            log::info!("[BinanceFuturesAccountAdapter] obtained listenKey");

            // Spawn keepalive task; if it fails silently, WS may close later
            {
                let rest = rest.clone();
                let base_url = base_url.clone();
                let api_key = api_key.clone();
                let listen_key_clone = listen_key.clone();
                tokio::spawn(async move {
                    let interval = Duration::from_secs(20 * 60);
                    loop {
                        sleep(interval).await;
                        let _ = rest
                            .put(format!("{}/fapi/v1/listenKey", base_url))
                            .header("X-MBX-APIKEY", &api_key)
                            .query(&[("listenKey", listen_key_clone.as_str())])
                            .send()
                            .await;
                    }
                });
            }

            let ws_url = format!("{}/ws/{}", ws_base, listen_key);
            let Ok((mut ws, _)) = connect_async(&ws_url).await else { return; };
            while let Some(msg) = ws.next().await {
                match msg {
                    Ok(Message::Text(txt)) => {
                        if log::log_enabled!(log::Level::Debug) {
                            log::debug!("[BinanceFuturesAccountAdapter] raw WS: {}", txt);
                        }
                        if let Ok(root) = serde_json::from_str::<WsRoot>(&txt) {
                            if let Some(ev) = root.event_type.as_deref() {
                                if ev == "ORDER_TRADE_UPDATE" {
                                    if let Some(o) = root.order.clone() {
                                        if o.exec_type == "TRADE" && o.symbol == symbol {
                                            let price: f64 = o.last_filled_price.parse().unwrap_or(0.0);
                                            let qty: f64 = o.last_filled_qty.parse().unwrap_or(0.0);
                                            let side = match o.side.as_str() {
                                                "BUY" => Side::Buy,
                                                "SELL" => Side::Sell,
                                                _ => Side::Unknown,
                                            };
                                            let ts_us = o.trade_time_ms * 1000;
                                            let fee: f64 = o.commission.as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                                            let exec = XExecution {
                                                timestamp: ts_us,
                                                rcv_timestamp: ts_us,
                                                market_id,
                                                account_id,
                                                exec_type: ExecutionType::XTrade,
                                                side,
                                                native_ord_id: o.order_id.to_string(),
                                                cl_ord_id: -1,
                                                orig_cl_ord_id: -1,
                                                ord_status: match o.order_status.as_str() {
                                                    "FILLED" => OrderStatus::OStatusFilled,
                                                    "PARTIALLY_FILLED" => OrderStatus::OStatusPartiallyFilled,
                                                    _ => OrderStatus::OStatusReplaced,
                                                },
                                                last_qty: qty,
                                                last_px: price,
                                                leaves_qty: 0.0,
                                                ord_qty: qty,
                                                ord_price: price,
                                                ord_mode: OrderMode::MLimit,
                                                tif: TimeInForce::TifGoodTillCancel,
                                                 fee,
                                                native_execution_id: o.trade_id.to_string(),
                                                metadata: {
                                                    let mut m = std::collections::HashMap::new();
                                                    m.insert("symbol".to_string(), symbol.clone());
                                                    m.insert("live".to_string(), "true".to_string());
                                                     if let Some(asset) = o.commission_asset.clone() { m.insert("fee_asset".to_string(), asset); }
                                                    m
                                                },
                                                is_taker: !o.is_maker,
                                            };
                                             if log::log_enabled!(log::Level::Debug) {
                                                 log::debug!("[BinanceFuturesAccountAdapter] mapped exec: {:?}", exec);
                                             }
                                             let _ = tx.send(exec).await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(Message::Binary(_)) => {}
                    Ok(Message::Ping(_)) => {}
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Frame(_)) => {}
                    Ok(Message::Close(reason)) => {
                        log::warn!("[BinanceFuturesAccountAdapter] WS close: {:?}", reason);
                        break
                    },
                    Err(e) => {
                        log::warn!("[BinanceFuturesAccountAdapter] WS error: {}", e);
                        break
                    },
                }
            }
            // Exit: channel closes and AccountState can perform full restore + resubscribe
        });

        Ok(rx)
    }
}


