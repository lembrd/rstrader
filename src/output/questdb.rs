use rustc_hash::FxHashMap as HashMap;
use std::time::Instant;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::xcommons::error::{AppError, Result};
use crate::xcommons::types::{OrderBookL2Update, StreamData, TradeUpdate, Metrics, ExchangeId};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_ILP_PORT: u16 = 9009;

const BATCH_SIZE: usize = 1000;
const FLUSH_INTERVAL_MS: u64 = 500; // lower latency than parquet by default

struct QuestDbClient {
    host: String,
    ilp_port: u16,
    stream: Option<TcpStream>,
}

impl QuestDbClient {
    fn new_from_env() -> Self {
        let host = std::env::var("QUESTDB_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
        let ilp_port: u16 = std::env::var("QUESTDB_ILP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_ILP_PORT);
        Self {
            host,
            ilp_port,
            stream: None,
        }
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let addr: SocketAddr = format!("{}:{}", self.host, self.ilp_port)
            .parse()
            .map_err(|e| AppError::connection(format!("Invalid QuestDB addr: {}", e)))?;
        let stream = TcpStream::connect(addr).await.map_err(|e| {
            AppError::connection(format!("Failed to connect to QuestDB ILP at {}: {}", addr, e))
        })?;
        stream.set_nodelay(true).map_err(|e| {
            AppError::connection(format!("Failed to set TCP_NODELAY on ILP stream: {}", e))
        })?;
        self.stream = Some(stream);
        Ok(())
    }

    async fn send_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.ensure_connected().await?;
        let mut attempts = 0u32;
        loop {
            if let Some(stream) = &mut self.stream {
                match stream.write_all(bytes).await {
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        log::warn!("ILP write failed: {}. Reconnecting...", e);
                        self.stream = None;
                    }
                }
            }
            attempts += 1;
            if attempts > 5 {
                return Err(AppError::connection(
                    "Failed to write to QuestDB ILP after retries".to_string(),
                ));
            }
            // Exponential backoff
            let backoff = Duration::from_millis(100 * (1 << (attempts - 1))); // 100,200,400,800,1600
            tokio::time::sleep(backoff).await;
            self.ensure_connected().await?;
        }
    }

    // DDL/schema management is handled externally by scripts; no ensure_tables here.
}

struct StreamWriter {
    l2_buffer: Vec<OrderBookL2Update>,
    trade_buffer: Vec<TradeUpdate>,
    last_activity: Instant,
    metrics: crate::xcommons::types::Metrics,
    stream_type: String,
    exchange: crate::xcommons::types::ExchangeId,
    symbol: String,
}

impl StreamWriter {
    fn new(stream_type: String, exchange: crate::xcommons::types::ExchangeId, symbol: String) -> Self {
        Self {
            l2_buffer: Vec::with_capacity(BATCH_SIZE),
            trade_buffer: Vec::with_capacity(BATCH_SIZE),
            last_activity: Instant::now(),
            metrics: {
                let mut m = crate::xcommons::types::Metrics::new();
                #[cfg(feature = "metrics-hdr")]
                {
                    m.enable_histograms(crate::metrics::HistogramBounds::default());
                }
                m
            },
            stream_type,
            exchange,
            symbol,
        }
    }

    fn buffer_len(&self) -> usize {
        self.l2_buffer.len() + self.trade_buffer.len()
    }

    fn push(&mut self, data: StreamData) {
        match data {
            StreamData::L2(u) => {
                self.l2_buffer.push(u);
                self.metrics.increment_messages_processed();
            }
            StreamData::Trade(t) => {
                self.trade_buffer.push(t);
                self.metrics.increment_messages_processed();
            }
        }
        self.last_activity = Instant::now();
    }

    fn serialize_and_clear(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.buffer_len().max(1)) * 160);
        if !self.l2_buffer.is_empty() {
            for u in self.l2_buffer.drain(..) {
                serialize_l2_ilp(&u, &mut out);
            }
        }
        if !self.trade_buffer.is_empty() {
            for t in self.trade_buffer.drain(..) {
                serialize_trade_ilp(&t, &mut out);
            }
        }
        // Publish metrics snapshot and quantiles (if enabled)
        crate::metrics::publish_stream_metrics(&self.stream_type, self.exchange, &self.symbol, &self.metrics);
        out
    }
}

fn serialize_l2_ilp(u: &OrderBookL2Update, out: &mut Vec<u8>) {
    // measurement + tags
    out.extend_from_slice(b"l2_updates,exchange=");
    out.extend_from_slice(u.exchange.to_string().as_bytes());
    out.extend_from_slice(b",ticker=");
    out.extend_from_slice(u.ticker.as_bytes());
    out.extend_from_slice(b",side=");
    out.extend_from_slice(u.side.to_string().as_bytes());
    out.extend_from_slice(b",action=");
    out.extend_from_slice(u.action.to_string().as_bytes());
    out.extend_from_slice(b" ");

    // fields
    push_i64_field(out, b"seq_id", u.seq_id);
    out.push(b',');
    push_i64_field(out, b"packet_id", u.packet_id);
    out.push(b',');
    push_f64_field(out, b"price", u.price);
    out.push(b',');
    push_f64_field(out, b"qty", u.qty);
    out.push(b',');
    push_i64_field(out, b"update_id", u.update_id);
    out.push(b',');
    push_i64_field(out, b"first_update_id", u.first_update_id);
    out.push(b',');
    // rcv_ts as integer microseconds (QuestDB TIMESTAMP expects micros for field casting)
    push_i64_field(out, b"rcv_ts", u.rcv_timestamp);
    out.push(b' ');

    // timestamp (ns)
    let ts_ns = micros_to_nanos(u.timestamp);
    out.extend_from_slice(ts_ns.to_string().as_bytes());
    out.push(b'\n');
}

fn serialize_trade_ilp(t: &TradeUpdate, out: &mut Vec<u8>) {
    out.extend_from_slice(b"trades,exchange=");
    out.extend_from_slice(t.exchange.to_string().as_bytes());
    out.extend_from_slice(b",ticker=");
    out.extend_from_slice(t.ticker.as_bytes());
    out.extend_from_slice(b",side=");
    out.extend_from_slice(t.side.to_string().as_bytes());
    out.extend_from_slice(b" ");

    push_i64_field(out, b"seq_id", t.seq_id);
    out.push(b',');
    push_i64_field(out, b"packet_id", t.packet_id);
    out.push(b',');
    push_string_field(out, b"trade_id", &t.trade_id);
    out.push(b',');
    push_string_field(out, b"order_id", t.order_id.as_deref().unwrap_or(""));
    out.push(b',');
    push_f64_field(out, b"price", t.price);
    out.push(b',');
    push_f64_field(out, b"qty", t.qty);
    out.push(b',');
    // rcv_ts as integer microseconds
    push_i64_field(out, b"rcv_ts", t.rcv_timestamp);
    out.push(b' ');

    let ts_ns = micros_to_nanos(t.timestamp);
    out.extend_from_slice(ts_ns.to_string().as_bytes());
    out.push(b'\n');
}

fn micros_to_nanos(us: i64) -> i64 { us.saturating_mul(1000) }

fn push_i64_field(out: &mut Vec<u8>, key: &[u8], val: i64) {
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(val.to_string().as_bytes());
    out.push(b'i');
}

fn push_f64_field(out: &mut Vec<u8>, key: &[u8], val: f64) {
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(ryu::Buffer::new().format(val).as_bytes());
}

// bool field helper unused after schema change; keep for potential future use

fn escape_string(s: &str) -> String {
    // Minimal ILP escaping for string field: escape quotes and spaces/commas
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            ' ' => out.push_str("\\ "),
            ',' => out.push_str("\\,"),
            '=' => out.push_str("\\="),
            _ => out.push(ch),
        }
    }
    out
}

fn push_string_field(out: &mut Vec<u8>, key: &[u8], val: &str) {
    out.extend_from_slice(key);
    out.push(b'=');
    out.push(b'"');
    out.extend_from_slice(escape_string(val).as_bytes());
    out.push(b'"');
}

pub struct MultiStreamQuestDbSink {
    client: QuestDbClient,
    writers: HashMap<String, StreamWriter>,
}

impl MultiStreamQuestDbSink {
    pub fn new() -> Self {
        Self {
            client: QuestDbClient::new_from_env(),
            writers: HashMap::default(),
        }
    }

    pub async fn initialize(&mut self) -> Result<()> { Ok(()) }

    pub async fn write_stream_data(&mut self, data: StreamData) -> Result<()> {
        let (_, _, exchange, symbol, _, _) = data.common_fields();
        let stream_type = data.stream_type();
        let key = format!("{}_{}_{}", stream_type, exchange, symbol);
        let writer = self.writers.entry(key).or_insert_with(|| StreamWriter::new(stream_type.to_string(), exchange, symbol.to_string()));
        writer.push(data);
        // Flush if either buffer meets threshold
        if writer.buffer_len() >= BATCH_SIZE {
            let buf = writer.serialize_and_clear();
            if !buf.is_empty() {
                self.client.send_bytes(&buf).await?;
            }
        }
        Ok(())
    }

    pub async fn flush_all(&mut self) -> Result<()> {
        for writer in self.writers.values_mut() {
            let buf = writer.serialize_and_clear();
            if !buf.is_empty() {
                self.client.send_bytes(&buf).await?;
            }
        }
        // Publish lightweight totals to global metrics exporters
        let mut total = crate::xcommons::types::Metrics::new();
        for w in self.writers.values() {
            // We don't keep per-writer metrics here; emit zeros conservatively
            let _ = w; // placeholder in case of future per-writer metrics
        }
        crate::metrics::update_global(&total);
        Ok(())
    }

    fn prune_idle_writers(&mut self, max_idle: Duration) {
        let now = Instant::now();
        self.writers.retain(|key, w| {
            let idle = now.duration_since(w.last_activity) > max_idle;
            let empty = w.buffer_len() == 0;
            let keep = !(idle && empty);
            if !keep {
                log::info!("Pruning idle QuestDB writer: {}", key);
            }
            keep
        });
    }
}

pub async fn run_multi_stream_questdb_sink(
    mut rx: mpsc::Receiver<StreamData>,
) -> Result<()> {
    let mut sink = MultiStreamQuestDbSink::new();
    sink.initialize().await?;

    let mut flush_interval =
        tokio::time::interval(tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS));

    log::info!("Starting Multi-Stream QuestDB sink");

    loop {
        tokio::select! {
            data = rx.recv() => {
                match data {
                    Some(stream_data) => {
                        if let Err(e) = sink.write_stream_data(stream_data).await {
                            log::error!("Failed to write stream data to QuestDB: {}", e);
                            return Err(e);
                        }
                    }
                    None => {
                        log::info!("Multi-stream QuestDB sink channel closed, flushing and shutting down");
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                // Periodic flush; avoid spam when nothing buffered
                let writer_count = sink.writers.len();
                if writer_count > 0 {
                    let mut _total_buffered = 0usize;
                    for w in sink.writers.values() {
                        _total_buffered += w.buffer_len();
                    }
                    // Silence flush tick logs entirely to avoid spam
                }
                if let Err(e) = sink.flush_all().await {
                    log::error!("Failed to flush QuestDB batches: {}", e);
                    return Err(e);
                }
                // Prune idle writers after flushing (e.g., idle for > 2 minutes)
                sink.prune_idle_writers(Duration::from_secs(120));
            }
        }
    }

    sink.flush_all().await?;
    log::info!("Multi-stream QuestDB sink shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xcommons::types::{ExchangeId, L2Action};
    use crate::xcommons::oms::Side;

    #[tokio::test]
    async fn test_ilp_serialize_nonempty() {
        let u = OrderBookL2Update {
            timestamp: 1,
            rcv_timestamp: 2,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            update_id: 10,
            first_update_id: 9,
            action: L2Action::Update,
            side: Side::Buy,
            price: 10.0,
            qty: 1.0,
        };
        let mut w = StreamWriter::new("L2".to_string(), ExchangeId::BinanceFutures, "BTCUSDT".to_string());
        w.push(StreamData::L2(u));
        let buf = w.serialize_and_clear();
        assert!(!buf.is_empty());
    }

    #[tokio::test]
    async fn test_ilp_serialize_trade_nonempty() {
        let t = TradeUpdate {
            timestamp: 1,
            rcv_timestamp: 2,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            trade_id: "T1".to_string(),
            order_id: Some("O1".to_string()),
            side: Side::Buy,
            price: 10.0,
            qty: 1.0,
        };
        let mut w = StreamWriter::new("TRADES".to_string(), ExchangeId::BinanceFutures, "BTCUSDT".to_string());
        w.push(StreamData::Trade(t));
        let buf = w.serialize_and_clear();
        assert!(!buf.is_empty());
    }
}


