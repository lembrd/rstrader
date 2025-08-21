use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex::encode as hex_encode;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{StreamExt, SinkExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use base64::{engine::general_purpose, Engine as _};
use ed25519_dalek::{SigningKey, Signer};
use ed25519_dalek::pkcs8::DecodePrivateKey;

use crate::trading::account_state::ExchangeAccountAdapter;
use crate::xcommons::error::{AppError, Result};
use crate::xcommons::oms::{ExecutionType, OrderStatus, Side, TimeInForce, OrderMode, XExecution};
use crate::xcommons::oms::{OrderRequest, OrderResponse, OrderResponseStatus, PostRequest, CancelRequest, CancelAllRequest};
use crate::xcommons::types::ExchangeId;
use crate::xcommons::xmarket_id::XMarketId;
use tokio::time::{sleep, Duration};
use tokio::sync::{mpsc as tmpsc, oneshot, Mutex};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct BinanceFuturesAccountAdapter {
    api_key: String,
    secret: String,
    ws_pubkey: Option<String>,
    ws_secret: Option<String>,
    ws_signing_key: Option<SigningKey>,
    rest: Client,
    symbols: Vec<String>,
    market_to_symbol: HashMap<i64, String>,
    base_url: String,
    ws_base: String,
    ws_api: Mutex<Option<WsApiState>>, // persistent WS-API connection
}

impl BinanceFuturesAccountAdapter {
    pub fn new(api_key: String, secret: String, symbols: Vec<String>, ws_pubkey: Option<String>, ws_secret: Option<String>) -> Self {
        let market_to_symbol = symbols
            .iter()
            .map(|s| (XMarketId::make(ExchangeId::BinanceFutures, s), s.clone()))
            .collect();
        // Pre-parse Ed25519 signing key if provided to avoid per-request overhead
        let ws_signing_key = ws_secret
            .as_ref()
            .and_then(|s| Self::parse_ws_signing_key(s).ok());
        Self {
            api_key,
            secret,
            ws_pubkey,
            ws_secret,
            ws_signing_key,
            rest: Client::new(),
            symbols,
            market_to_symbol,
            base_url: "https://fapi.binance.com".to_string(),
            ws_base: "wss://fstream.binance.com".to_string(),
            ws_api: Mutex::new(None),
        }
    }

    fn parse_ws_signing_key(key_src: &str) -> Result<SigningKey> {
        // Accept PEM PKCS#8, hex, base64 raw 32 bytes, or DER PKCS#8
        if key_src.trim_start().starts_with("-----BEGIN") {
            // Extract base64 between header/footer; tolerate YAML folding/whitespace
            let mut s = key_src.replace('\r', "");
            s = s.replace("-----BEGIN PRIVATE KEY-----", "");
            s = s.replace("-----END PRIVATE KEY-----", "");
            let b64: String = s.chars().filter(|c| !c.is_whitespace()).collect();
            let der = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .map_err(|e| AppError::config(format!("ed25519 pem base64: {}", e)))?;
            match SigningKey::from_pkcs8_der(&der) {
                Ok(sk) => Ok(sk),
                Err(_) => {
                    // Fallback: try to locate seed inside PKCS#8: look for 0x04 0x20 followed by 32-byte seed
                    if let Some(pos) = der.windows(2).position(|w| w == [0x04, 0x20]) {
                        let start = pos + 2;
                        if der.len() >= start + 32 {
                            let seed: [u8; 32] = der[start..start+32].try_into().unwrap();
                            Ok(SigningKey::from_bytes(&seed))
                        } else {
                            Err(AppError::config("ed25519 pkcs8: missing 32-byte seed".to_string()))
                        }
                    } else {
                        Err(AppError::config("ed25519 pkcs8: parse failure".to_string()))
                    }
                }
            }
        } else if key_src.trim().len() >= 64 && key_src.chars().all(|c| c.is_ascii_hexdigit()) {
            let sk_bytes = hex::decode(key_src.trim()).map_err(|e| AppError::config(format!("ed25519 secret hex decode: {}", e)))?;
            let arr: [u8; 32] = sk_bytes.as_slice().try_into().map_err(|_| AppError::config("ed25519 key length".to_string()))?;
            Ok(SigningKey::from_bytes(&arr))
        } else {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(key_src.trim())
                .map_err(|e| AppError::config(format!("ed25519 base64 decode: {}", e)))?;
            if raw.len() == 32 {
                let arr: [u8; 32] = raw.as_slice().try_into().map_err(|_| AppError::config("ed25519 key length".to_string()))?;
                Ok(SigningKey::from_bytes(&arr))
            } else {
                SigningKey::from_pkcs8_der(&raw).map_err(|e| AppError::config(format!("ed25519 pkcs8: {}", e)))
            }
        }
    }

    fn sign_query(&self, query: &str) -> String {
        let mut mac = <Hmac<Sha256>>::new_from_slice(self.secret.as_bytes()).expect("hmac");
        mac.update(query.as_bytes());
        let sig = mac.finalize().into_bytes();
        hex_encode(sig)
    }

    async fn ws_api_request(&self, method: &str, mut params: serde_json::Map<String, serde_json::Value>) -> Result<serde_json::Value> {
        // Sign: sort params and sign with Ed25519 base64; we assume `secret` is PEM-encoded Ed25519 key material
        let timestamp = chrono::Utc::now().timestamp_millis();
        params.insert("timestamp".to_string(), serde_json::Value::from(timestamp));
        // Prefer explicit ed25519 apiKey for WS if provided; otherwise fall back to REST key
        let ws_key = self.ws_pubkey.clone().unwrap_or_else(|| self.api_key.clone());
        params.insert("apiKey".to_string(), serde_json::Value::from(ws_key));
        let mut items: Vec<(String, String)> = params.iter().map(|(k,v)| (k.clone(), v.as_str().unwrap_or(&v.to_string()).to_string())).collect();
        items.sort_by(|a,b| a.0.cmp(&b.0));
        let payload = items.iter().map(|(k,v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
        // Use pre-parsed signing key if available
        let signing_key = self.ws_signing_key.as_ref().ok_or_else(|| AppError::config("ed25519_secret missing in config".to_string()))?;
        let sig = signing_key.sign(payload.as_bytes());
        let sig_b64 = general_purpose::STANDARD.encode(sig.to_bytes());
        params.insert("signature".to_string(), serde_json::Value::from(sig_b64.clone()));

        let req_id = format!("xtrader-{}", crate::xcommons::monoseq::next_id());
        let req = serde_json::json!({
            "id": req_id,
            "method": method,
            "params": params.clone(),
        });

        // Debug sanitized params
        if log::log_enabled!(log::Level::Debug) {
            let mut sanitized = params.clone();
            if let Some(v) = sanitized.get_mut("signature") { *v = serde_json::Value::String("***".to_string()); }
            if let Some(v) = sanitized.get_mut("apiKey") { *v = serde_json::Value::String("***".to_string()); }
            log::debug!("[Binance WS-API] method={} req_id={} params={}", method, req_id, serde_json::to_string(&sanitized).unwrap_or_default());
        }

        // Persistent connection path
        let handle = self.ensure_ws_api().await?;
        let (tx_once, rx_once) = oneshot::channel::<serde_json::Value>();
        {
            let mut map = handle.pending.lock().await;
            map.insert(req_id.clone(), tx_once);
        }
        let send_start_us = chrono::Utc::now().timestamp_micros();
        if let Err(e) = handle.send_tx.send(req.to_string()).await {
            log::warn!("[Binance WS-API] send failed: {} (resetting)", e);
            self.reset_ws_api().await;
            let handle = self.ensure_ws_api().await?;
            let (tx_once2, rx_once2) = oneshot::channel::<serde_json::Value>();
            {
                let mut map = handle.pending.lock().await;
                map.insert(req_id.clone(), tx_once2);
            }
            handle.send_tx.send(req.to_string()).await.map_err(|e| AppError::connection(format!("ws api send: {}", e)))?;
            let v = tokio::time::timeout(Duration::from_secs(7), rx_once2)
                .await
                .map_err(|_| AppError::connection("ws api timeout".to_string()))?
                .map_err(|_| AppError::connection("ws api closed".to_string()))?;
            let rcv_us = chrono::Utc::now().timestamp_micros();
            if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
                prom.set_ws_api_last_rtt_us("BINANCE_FUTURES", method, (rcv_us - send_start_us) as u64);
                prom.inc_ws_api_request_outcome("BINANCE_FUTURES", method, "ok");
            }
            return Self::extract_result(v);
        }
        let v = match tokio::time::timeout(Duration::from_secs(7), rx_once).await {
            Ok(Ok(val)) => val,
            Ok(Err(_)) => {
                if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
                    prom.inc_ws_api_request_outcome("BINANCE_FUTURES", method, "closed");
                }
                return Err(AppError::connection("ws api closed".to_string()));
            }
            Err(_) => {
                if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
                    prom.inc_ws_api_request_outcome("BINANCE_FUTURES", method, "timeout");
                }
                return Err(AppError::connection("ws api timeout".to_string()));
            }
        };
        let rcv_us = chrono::Utc::now().timestamp_micros();
        if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
            prom.set_ws_api_last_rtt_us("BINANCE_FUTURES", method, (rcv_us - send_start_us) as u64);
            prom.inc_ws_api_request_outcome("BINANCE_FUTURES", method, "ok");
        }
        Self::extract_result(v)
    }

    fn extract_result(v: serde_json::Value) -> Result<serde_json::Value> {
        if let Some(status) = v.get("status").and_then(|s| s.as_i64()) {
            if status == 200 { Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null)) }
            else { Err(AppError::connection(format!("ws api error: {}", v))) }
        } else {
            Err(AppError::connection("ws api invalid response".to_string()))
        }
    }

    async fn ensure_ws_api(&self) -> Result<WsApiHandle> {
        if let Some(state) = self.ws_api.lock().await.as_ref() { return Ok(state.handle()); }
        let state = Self::connect_ws_api().await?;
        let handle = state.handle();
        *self.ws_api.lock().await = Some(state);
        Ok(handle)
    }

    async fn reset_ws_api(&self) {
        *self.ws_api.lock().await = None;
    }

    async fn connect_ws_api() -> Result<WsApiState> {
        let ws_url = "wss://ws-fapi.binance.com/ws-fapi/v1";
        let (ws, _) = connect_async(ws_url).await.map_err(|e| AppError::connection(format!("ws api connect: {}", e)))?;
        log::debug!("[Binance WS-API] connected {}", ws_url);
        let (mut write, mut read) = ws.split();
        let (send_tx, mut send_rx) = tmpsc::channel::<String>(1024);
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        // Writer task
        tokio::spawn(async move {
            while let Some(txt) = send_rx.recv().await {
                if let Err(e) = write.send(Message::Text(txt)).await {
                    log::warn!("[Binance WS-API] send error: {}", e);
                    break;
                }
            }
        });
        // Reader task
        tokio::spawn(async move {
            loop {
                match read.next().await {
                    Some(Ok(Message::Text(txt))) => {
                        log::debug!("[Binance WS-API] recv text: {}", txt);
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                            if let Some(id) = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()) {
                                if let Some(tx) = pending_reader.lock().await.remove(&id) { let _ = tx.send(v); }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(b))) => { log::debug!("[Binance WS-API] recv binary: {} bytes", b.len()); }
                    Some(Ok(Message::Ping(_p))) => { /* optional: respond via write if needed */ }
                    Some(Ok(Message::Pong(_p))) => {}
                    Some(Ok(Message::Close(reason))) => { log::warn!("[Binance WS-API] close: {:?}", reason); break; }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => { log::warn!("[Binance WS-API] recv error: {}", e); break; }
                    None => { log::warn!("[Binance WS-API] stream ended"); break; }
                }
            }
        });
        Ok(WsApiState { send_tx, pending })
    }

    async fn post_order(&self, p: &PostRequest) -> Result<OrderResponse> {
        // Use WS API order.place per docs
        let symbol = self.market_to_symbol.get(&p.market_id).ok_or_else(|| AppError::config("unknown market_id".to_string()))?.clone();
        let side = match p.side { Side::Buy => "BUY", Side::Sell => "SELL", Side::Unknown => "BUY" };
        let tif = match p.tif { TimeInForce::TifImmediateOrCancel => "IOC", TimeInForce::TifFillOrKill => "FOK", _ => "GTC" };
        let order_type = match p.ord_mode { OrderMode::MLimit => "LIMIT", OrderMode::MMarket => "MARKET", _ => "LIMIT" };
        let mut params = serde_json::Map::new();
        params.insert("symbol".into(), symbol.clone().into());
        params.insert("side".into(), side.into());
        params.insert("type".into(), order_type.into());
        if order_type == "LIMIT" {
            let tif_final = if p.post_only { "GTX" } else { tif };
            params.insert("timeInForce".into(), tif_final.into());
            params.insert("price".into(), format!("{:.2}", p.price).into());
        }
        // Futures supports reduceOnly flag
        params.insert("reduceOnly".into(), serde_json::Value::Bool(p.reduce_only));
        params.insert("quantity".into(), format!("{:.6}", p.qty).into());
        params.insert("newClientOrderId".into(), crate::xcommons::oms::clordid::format_xcl(p.cl_ord_id).into());
        match self.ws_api_request("order.place", params).await {
            Ok(_v) => {},
            Err(e) => {
                if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
                    prom.inc_ws_api_error_code("BINANCE_FUTURES", "order.place", "error");
                }
                let s = format!("{}", e);
                if s.contains("\"code\":-5022") {
                    let rcv_timestamp = chrono::Utc::now().timestamp_micros();
                    return Ok(OrderResponse { req_id: p.req_id, timestamp: rcv_timestamp, rcv_timestamp, cl_ord_id: Some(p.cl_ord_id), native_ord_id: None, status: OrderResponseStatus::FailedPostOnly, exec: None });
                } else {
                    return Err(e);
                }
            }
        }
        let rcv_timestamp = chrono::Utc::now().timestamp_micros();
        Ok(OrderResponse { req_id: p.req_id, timestamp: rcv_timestamp, rcv_timestamp, cl_ord_id: Some(p.cl_ord_id), native_ord_id: None, status: OrderResponseStatus::Ok, exec: None })
    }

    async fn cancel_order(&self, c: &CancelRequest) -> Result<OrderResponse> {
        let symbol = self.market_to_symbol.get(&c.market_id).ok_or_else(|| AppError::config("unknown market_id".to_string()))?.clone();
        let mut params = serde_json::Map::new();
        params.insert("symbol".into(), symbol.into());
        if let Some(cl) = c.cl_ord_id { params.insert("origClientOrderId".into(), crate::xcommons::oms::clordid::format_xcl(cl).into()); }
        if let Some(native) = c.native_ord_id.clone() { params.insert("orderId".into(), native.into()); }
        match self.ws_api_request("order.cancel", params).await {
            Ok(_v) => {},
            Err(e) => {
                if let Some(prom) = crate::metrics::PROM_EXPORTER.get() {
                    prom.inc_ws_api_error_code("BINANCE_FUTURES", "order.cancel", "error");
                }
                return Err(e);
            }
        }
        let rcv_timestamp = chrono::Utc::now().timestamp_micros();
        Ok(OrderResponse { req_id: c.req_id, timestamp: rcv_timestamp, rcv_timestamp, cl_ord_id: c.cl_ord_id, native_ord_id: None, status: OrderResponseStatus::Ok, exec: None })
    }

    async fn cancel_all(&self, a: &CancelAllRequest) -> Result<OrderResponse> {
        // Per strategy spec: Use REST for cancel-all; WS doesn't support cancelAll
        let symbol = self.market_to_symbol.get(&a.market_id).ok_or_else(|| AppError::config("unknown market_id".to_string()))?.clone();
        let timestamp = chrono::Utc::now().timestamp_millis();
        let query = format!("symbol={}&timestamp={}", symbol, timestamp);
        let sig = self.sign_query(&query);
        let url = format!("{}/fapi/v1/allOpenOrders?{}&signature={}", self.base_url, query, sig);
        let resp = self.rest
            .delete(url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| AppError::io(format!("cancelAll send: {}", e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();
            let txt = String::from_utf8_lossy(&body);
            return Err(AppError::io(format!("cancelAll http {}: {}", status, txt)));
        }
        let rcv_timestamp = chrono::Utc::now().timestamp_micros();
        Ok(OrderResponse { req_id: a.req_id, timestamp: rcv_timestamp, rcv_timestamp, cl_ord_id: None, native_ord_id: None, status: OrderResponseStatus::Ok, exec: None })
    }
}

#[derive(Clone)]
struct WsApiHandle {
    send_tx: tmpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
}

struct WsApiState {
    send_tx: tmpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
}

impl WsApiState {
    fn handle(&self) -> WsApiHandle { WsApiHandle { send_tx: self.send_tx.clone(), pending: self.pending.clone() } }
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
    #[serde(rename = "c")] client_order_id: Option<String>,
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

#[derive(Debug, Deserialize, Clone)]
struct WsTradeLite {
    #[serde(rename = "s")] symbol: String,
    #[serde(rename = "S")] side: String,
    #[serde(rename = "p")] order_price: Option<String>,
    #[serde(rename = "L")] last_price: Option<String>,
    #[serde(rename = "l")] last_qty: String,
    #[serde(rename = "t")] trade_id: i64,
    #[serde(rename = "i")] order_id: i64,
    #[serde(rename = "T")] trade_time_ms: i64,
    #[serde(rename = "m")] is_maker: bool,
}

#[async_trait]
impl ExchangeAccountAdapter for BinanceFuturesAccountAdapter {
    async fn fetch_historical(&self, account_id: i64, market_id: i64, gt_ts: i64, _gt_seq: Option<i64>) -> Result<Vec<XExecution>> {
        let symbol = self.market_to_symbol.get(&market_id).ok_or_else(|| AppError::config(format!("unknown market_id {}", market_id)))?.clone();
        let gt_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(gt_ts).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
        log::info!(
            "[BinanceFuturesAccountAdapter] fetch_historical account_id={} market_id={} symbol={} gt_ts(us)={} ({}) (start_ms={})",
            account_id, market_id, symbol, gt_ts, gt_rfc, (gt_ts/1000)+1
        );
        let mut out: Vec<XExecution> = Vec::new();
        let mut window_start_ms: i64 = (gt_ts / 1000) + 1; // ms, strictly greater
        let day_ms: i64 = 86_400_000;
        let limit = 1000i32;
        let now_ms_all = chrono::Utc::now().timestamp_millis();
        while window_start_ms <= now_ms_all {
            let window_end_ms = (window_start_ms + (7 * day_ms) - 1).min(now_ms_all);
            let mut page_start = window_start_ms;
            loop {
                let timestamp = chrono::Utc::now().timestamp_millis();
                let query = format!("symbol={}&startTime={}&endTime={}&limit={}&timestamp={}", symbol, page_start, window_end_ms, limit, timestamp);
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
                let win_start_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(window_start_ms).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                let win_end_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(window_end_ms).map(|d| d.to_rfc3339()).unwrap_or_else(|| "invalid".to_string());
                log::info!(
                    "[BinanceFuturesAccountAdapter] userTrades page count={} window=[{} ({}) .. {} ({})] page_start={}",
                    trades.len(), window_start_ms, win_start_rfc, window_end_ms, win_end_rfc, page_start
                );
                if trades.is_empty() { break; }
                for tr in trades.iter() {
                    let price: f64 = tr.price.parse().unwrap_or(0.0);
                    let qty: f64 = tr.qty.parse().unwrap_or(0.0);
                    let ts_us = tr.time_ms * 1000;
                    let side = match tr.side.as_str() { "BUY" => Side::Buy, "SELL" => Side::Sell, _ => Side::Unknown };
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
                if trades.len() == limit as usize {
                    page_start = trades.last().map(|t| t.time_ms + 1).unwrap_or(page_start + 1);
                    if page_start <= window_end_ms { continue; }
                }
                break;
            }
            window_start_ms = window_end_ms + 1;
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

            // Spawn keepalive task; cancel on stream termination
            let cancel_token = CancellationToken::new();
            let cancel_guard = cancel_token.clone();
            {
                let rest = rest.clone();
                let base_url = base_url.clone();
                let api_key = api_key.clone();
                let listen_key_clone = listen_key.clone();
                tokio::spawn(async move {
                    let interval = Duration::from_secs(20 * 60);
                    loop {
                        tokio::select! {
                            _ = cancel_guard.cancelled() => { break; }
                            _ = sleep(interval) => {
                                let res = rest
                                    .put(format!("{}/fapi/v1/listenKey", base_url))
                                    .header("X-MBX-APIKEY", &api_key)
                                    .query(&[("listenKey", listen_key_clone.as_str())])
                                    .send()
                                    .await;
                                if let Err(e) = res { log::warn!("[BinanceFuturesAccountAdapter] listenKey keepalive error: {}", e); }
                            }
                        }
                    }
                });
            }

            let ws_url = format!("{}/ws/{}", ws_base, listen_key);
            let Ok((mut ws, _)) = connect_async(&ws_url).await else { return; };
            let mut last_frame_at = chrono::Utc::now().timestamp_millis();
            let idle_timeout_ms: i64 = 180_000; // 180s watchdog
            let mut watchdog = tokio::time::interval(Duration::from_millis(5_000));
            watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut seen_ortu: HashSet<i64> = HashSet::with_capacity(8192);
            let mut pending_tl: HashMap<i64, (XExecution, i64)> = HashMap::with_capacity(8192);
            loop {
                tokio::select! {
                    _ = watchdog.tick() => {
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        if now_ms - last_frame_at > idle_timeout_ms {
                            log::warn!("[BinanceFuturesAccountAdapter] WS idle {}ms > {}; closing to force restart", now_ms - last_frame_at, idle_timeout_ms);
                            let _ = ws.close(None).await;
                            break;
                        }
                    }
                    msg_opt = ws.next() => {
                        let Some(msg) = msg_opt else { log::warn!("[BinanceFuturesAccountAdapter] WS stream ended"); break; };
                        match msg {
                    Ok(Message::Text(txt)) => {
                        last_frame_at = chrono::Utc::now().timestamp_millis();
                        if log::log_enabled!(log::Level::Debug) {
                            log::debug!("[BinanceFuturesAccountAdapter] raw WS: {}", txt);
                        }
                        if let Ok(root) = serde_json::from_str::<WsRoot>(&txt) {
                            if let Some(ev) = root.event_type.as_deref() {
                                if ev == "ORDER_TRADE_UPDATE" {
                                    if let Some(o) = root.order.clone() {
                                        if o.exec_type == "TRADE" && o.symbol == symbol {
                                            // Prefer ORDER_TRADE_UPDATE (has commission fields). De-duplicate by trade_id
                                            if !seen_ortu.insert(o.trade_id) { continue; }
                                            if seen_ortu.len() > 8192 { seen_ortu.clear(); }
                                            // Drop any pending TRADE_LITE placeholder for this trade_id
                                            let _ = pending_tl.remove(&o.trade_id);
                                            let price: f64 = o.last_filled_price.parse().unwrap_or(0.0);
                                            let qty: f64 = o.last_filled_qty.parse().unwrap_or(0.0);
                                            let side = match o.side.as_str() {
                                                "BUY" => Side::Buy,
                                                "SELL" => Side::Sell,
                                                _ => Side::Unknown,
                                            };
                                            let ts_us = o.trade_time_ms * 1000;
                                            let rcv_ts = chrono::Utc::now().timestamp_micros();
                                            let fee: f64 = o.commission.as_deref().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                                            let cl_id_i64 = o.client_order_id.as_deref().and_then(|c| crate::xcommons::oms::clordid::parse_xcl(c)).unwrap_or(-1);
                                            let exec = XExecution {
                                                timestamp: ts_us,
                                                rcv_timestamp: rcv_ts,
                                                market_id,
                                                account_id,
                                                exec_type: ExecutionType::XTrade,
                                                side,
                                                native_ord_id: o.order_id.to_string(),
                                                cl_ord_id: cl_id_i64,
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
                                        } else if o.symbol == symbol && (o.exec_type == "CANCELED" || o.order_status == "CANCELED") {
                                            let ts_us = o.trade_time_ms * 1000;
                                            let rcv_ts = chrono::Utc::now().timestamp_micros();
                                            let side = match o.side.as_str() { "BUY" => Side::Buy, "SELL" => Side::Sell, _ => Side::Unknown };
                                            let cl_id_i64 = o.client_order_id.as_deref().and_then(|c| crate::xcommons::oms::clordid::parse_xcl(c)).unwrap_or(-1);
                                            let exec = XExecution {
                                                timestamp: ts_us,
                                                rcv_timestamp: rcv_ts,
                                                market_id,
                                                account_id,
                                                exec_type: ExecutionType::XOrderCanceled,
                                                side,
                                                native_ord_id: o.order_id.to_string(),
                                                cl_ord_id: cl_id_i64,
                                                orig_cl_ord_id: -1,
                                                ord_status: OrderStatus::OStatusCanceled,
                                                last_qty: 0.0,
                                                last_px: 0.0,
                                                leaves_qty: 0.0,
                                                ord_qty: 0.0,
                                                ord_price: 0.0,
                                                ord_mode: OrderMode::MLimit,
                                                tif: TimeInForce::TifGoodTillCancel,
                                                fee: 0.0,
                                                native_execution_id: format!("cancel:{}:{}", o.order_id, o.trade_time_ms),
                                                metadata: {
                                                    let mut m = std::collections::HashMap::new();
                                                    m.insert("symbol".to_string(), symbol.clone());
                                                    m.insert("live".to_string(), "true".to_string());
                                                    m
                                                },
                                                is_taker: false,
                                            };
                                            let _ = tx.send(exec).await;
                                        }
                                    }
                                } else if ev == "TRADE_LITE" {
                                    if let Ok(tl) = serde_json::from_str::<WsTradeLite>(&txt) {
                                        if tl.symbol == symbol {
                                            // If ORTU already received for this trade, skip TL
                                            if seen_ortu.contains(&tl.trade_id) { continue; }
                                            let price: f64 = tl.last_price.as_deref().and_then(|s| s.parse().ok())
                                                .or_else(|| tl.order_price.as_deref().and_then(|s| s.parse().ok()))
                                                .unwrap_or(0.0);
                                            let qty: f64 = tl.last_qty.parse().unwrap_or(0.0);
                                            let side = match tl.side.as_str() { "BUY" => Side::Buy, "SELL" => Side::Sell, _ => Side::Unknown };
                                            let ts_us = tl.trade_time_ms * 1000;
                                            let rcv_ts = chrono::Utc::now().timestamp_micros();
                                            let exec = XExecution {
                                                timestamp: ts_us,
                                                rcv_timestamp: rcv_ts,
                                                market_id,
                                                account_id,
                                                exec_type: ExecutionType::XTrade,
                                                side,
                                                native_ord_id: tl.order_id.to_string(),
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
                                                fee: 0.0,
                                                native_execution_id: tl.trade_id.to_string(),
                                                metadata: {
                                                    let mut m = std::collections::HashMap::new();
                                                    m.insert("symbol".to_string(), symbol.clone());
                                                    m.insert("live".to_string(), "true".to_string());
                                                    m
                                                },
                                                is_taker: !tl.is_maker,
                                            };
                                            // Buffer TL briefly; if ORTU doesn't arrive, we'll emit it later
                                            let now_ms = chrono::Utc::now().timestamp_millis();
                                            pending_tl.entry(tl.trade_id).or_insert((exec, now_ms));
                                        }
                                    }
                                }
                            }
                        }
                        // Flush any buffered TL older than threshold (no ORTU arrived)
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        let mut to_emit: Vec<i64> = Vec::new();
                        for (tid, (_ex, ts)) in pending_tl.iter() {
                            if now_ms.saturating_sub(*ts) >= 1500 { to_emit.push(*tid); }
                        }
                        for tid in to_emit {
                            if let Some((ex, _)) = pending_tl.remove(&tid) {
                                if log::log_enabled!(log::Level::Debug) { log::debug!("[BinanceFuturesAccountAdapter] emitting buffered TRADE_LITE trade_id={} (no ORTU)", tid); }
                                let _ = tx.send(ex).await;
                            }
                        }
                    }
                    Ok(Message::Binary(_)) => { last_frame_at = chrono::Utc::now().timestamp_millis(); }
                    Ok(Message::Ping(_)) => { last_frame_at = chrono::Utc::now().timestamp_millis(); }
                    Ok(Message::Pong(_)) => { last_frame_at = chrono::Utc::now().timestamp_millis(); }
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
                }
            }
            // Exit: cancel keepalive and close channel so AccountState can perform restore logic
            cancel_token.cancel();
        });

        Ok(rx)
    }

    async fn send_request(&self, req: OrderRequest) -> Result<OrderResponse> {
        match req {
            OrderRequest::Post(p) => self.post_order(&p).await,
            OrderRequest::Cancel(c) => self.cancel_order(&c).await,
            OrderRequest::CancelAll(a) => self.cancel_all(&a).await,
        }
    }
}



