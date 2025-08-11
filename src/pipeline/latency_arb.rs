#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio::time::{sleep, Duration, Instant};

use crate::cli::{Exchange, SubscriptionSpec};
use crate::error::{AppError, Result};
use crate::exchanges::{ExchangeConnector, ProcessorFactory};
use crate::exchanges::okx::InstrumentRegistry;
use crate::types::{RawMessage, StreamData};
use crate::pipeline::processor::MetricsReporter;

/// Per-subscription arbitration configuration
#[derive(Debug, Clone, Copy, Default)]
pub struct ArbConfig {
    /// Max concurrent connections for this subscription
    pub max_connections: usize,
    /// Ramp-up delay between sequential connection attempts (millis)
    pub ramp_delay_ms: u64,
}

/// Minimal skeleton for latency arbitration group
pub struct LatencyArbGroup {
    pub config: ArbConfig,
}

impl LatencyArbGroup {
    pub fn new(config: ArbConfig) -> Self {
        Self { config }
    }

    /// Accept a raw message from a connector (placeholder API to be wired later)
    pub fn on_raw_message(&mut self, _connector_id: usize, _raw: RawMessage) {
        // TODO: implement arbitration logic
    }
}

/// Spawn an arbitrated trades handler for a single subscription
/// - Creates up to max_connections connectors
/// - Ramps them up with a fixed delay between starts
/// - Merges RawMessages, processes via a single processor, dedupes trades by trade_id, and forwards earliest arrivals
pub async fn run_arbitrated_trades(
    subscription: SubscriptionSpec,
    index: usize,
    verbose: bool,
    okx_swap_registry: Option<Arc<InstrumentRegistry>>,
    okx_spot_registry: Option<Arc<InstrumentRegistry>>,
    output_tx: mpsc::Sender<StreamData>,
    cancel: CancellationToken,
) -> Result<()> {
    const ARB_REPORT_INTERVAL_SECS: u64 = 5;
    let max_conn = subscription.max_connections.unwrap_or(2).max(1);

    log::info!(
        "[arb][{}][{:?}@{}] starting TRADES arbitration with max_connections={}",
        index, subscription.exchange, subscription.instrument, max_conn
    );

    // Build processor once
    let processor = match subscription.exchange {
        Exchange::BinanceFutures => ProcessorFactory::create_binance_processor(),
        Exchange::OkxSwap => ProcessorFactory::create_processor(
            crate::types::ExchangeId::OkxSwap,
            okx_swap_registry.clone(),
        ),
        Exchange::OkxSpot => ProcessorFactory::create_processor(
            crate::types::ExchangeId::OkxSpot,
            okx_spot_registry.clone(),
        ),
        Exchange::Deribit => ProcessorFactory::create_deribit_processor(),
    };

    let mut processor = processor;

    // Metrics reporter (matches unified handler behavior)
    let mut reporter = if verbose { Some(MetricsReporter::new(subscription.instrument.clone(), 10)) } else { None };
    if let Some(ref mut rep) = reporter {
        let ex = match subscription.exchange { Exchange::BinanceFutures => crate::types::ExchangeId::BinanceFutures, Exchange::OkxSwap => crate::types::ExchangeId::OkxSwap, Exchange::OkxSpot => crate::types::ExchangeId::OkxSpot, Exchange::Deribit => crate::types::ExchangeId::Deribit };
        rep.set_exchange(ex);
        rep.set_stream_type("TRADES");
    }

    // Channels from connector readers to arb
    let (raw_tx, mut raw_rx) = mpsc::channel::<(usize, RawMessage)>(10000);
    // Channel to receive connector peer IPs
    let (ip_tx, mut ip_rx) = mpsc::channel::<(usize, String)>(max_conn * 2);
    // Channel to receive connector peer IPs
    let (ip_tx, mut ip_rx) = mpsc::channel::<(usize, String)>(max_conn * 2);
    // Channel to receive connector peer IPs
    let (ip_tx, mut ip_rx) = mpsc::channel::<(usize, String)>(max_conn * 2);

    // Spawn connectors with ramp-up
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let exchange = subscription.exchange.clone();
    let instrument = subscription.instrument.clone();

    for conn_id in 0..max_conn {
        let connector: Box<dyn ExchangeConnector<Error = AppError>> = match exchange {
            Exchange::BinanceFutures => Box::new(crate::exchanges::binance::BinanceFuturesConnector::new()),
            Exchange::OkxSwap => {
                let reg = okx_swap_registry.clone().ok_or_else(|| AppError::connection("OKX SWAP registry not initialized".to_string()))?;
                Box::new(crate::exchanges::okx::OkxConnector::new_swap(reg))
            }
            Exchange::OkxSpot => {
                let reg = okx_spot_registry.clone().ok_or_else(|| AppError::connection("OKX SPOT registry not initialized".to_string()))?;
                Box::new(crate::exchanges::okx::OkxConnector::new_spot(reg))
            }
            Exchange::Deribit => {
                let cfg = crate::exchanges::deribit::DeribitConfig::from_env().map_err(|e| AppError::connection(format!("Failed to load Deribit config: {}", e)))?;
                Box::new(crate::exchanges::deribit::DeribitConnector::new(cfg))
            }
        };

        let symbol = instrument.clone();
        let tx = raw_tx.clone();
        let verbose_local = verbose;

        let cancel_child = cancel.child_token();
        let handle = tokio::spawn(async move {
            if verbose_local {
                log::info!("[arb][{}] connecting trades conn={} {}@{}", index, conn_id, format!("{:?}", exchange), symbol);
            }

            let mut local_conn = connector;
            if let Err(e) = local_conn.connect().await {
                log::error!("[arb] connect failed conn={}: {}", conn_id, e);
                return;
            }
            if let Err(e) = local_conn.subscribe_trades(&symbol).await {
                log::error!("[arb] subscribe trades failed conn={}: {}", conn_id, e);
                return;
            }
            if verbose_local { log::info!("[arb][{}] connected trades conn={} {}@{}", index, conn_id, format!("{:?}", exchange), symbol); }

            loop {
                tokio::select! {
                    _ = cancel_child.cancelled() => {
                        let _ = local_conn.disconnect().await; break;
                    }
                    msg = local_conn.next_message() => {
                        match msg {
                            Ok(Some(raw)) => { if tx.send((conn_id, raw)).await.is_err() { break; } }
                            Ok(None) => { if verbose_local { log::info!("[arb] disconnected trades conn={}", conn_id); } break; }
                            Err(e) => { log::error!("[arb] next_message error conn={}: {}", conn_id, e); break; }
                        }
                    }
                }
            }
        });
        handles.push(handle);

        // Ramp-up spacing
        if conn_id + 1 < max_conn {
            sleep(Duration::from_secs(1)).await;
        }
    }

    drop(raw_tx); // keep arb alive while any tasks still hold a clone

    // Dedup set for trades (exchange+symbol+trade_id)
    let mut seen_trades: HashSet<(crate::types::ExchangeId, String, String)> = HashSet::new();

    // Simple per-connector accounting
    let mut wins: HashMap<usize, u64> = HashMap::new();
    let mut msgs: HashMap<usize, u64> = HashMap::new();
    let mut last_report = Instant::now() - Duration::from_secs(ARB_REPORT_INTERVAL_SECS);

    loop {
        let next = tokio::select! { _ = cancel.cancelled() => None, msg = raw_rx.recv() => msg };
        let Some((conn_id, raw_msg)) = next else { break; };
        if verbose { log::debug!("[arb][L2][{}] recv from conn={}", index, conn_id); }
        // Process through processor (unified path) as Trade stream
        let rcv_timestamp = crate::types::time::now_micros();
        let packet_id = processor.next_packet_id();
        let message_bytes = raw_msg.data.len() as u32;
        let stream_data = processor.process_unified_message(
            raw_msg,
            &subscription.instrument,
            rcv_timestamp,
            packet_id,
            message_bytes,
            crate::types::StreamType::Trade,
        )?;

        for data in stream_data {
            if let StreamData::Trade(t) = &data {
                let key = (t.exchange, t.ticker.clone(), t.trade_id.clone());
                if !seen_trades.insert(key) {
                    continue; // duplicate from slower connector
                }
                *wins.entry(conn_id).or_insert(0) += 1;
            }

            // Forward downstream
            if let Err(e) = output_tx.send(data).await {
                return Err(AppError::pipeline(format!("Output channel send failed: {}", e)));
            }
        }

        *msgs.entry(conn_id).or_insert(0) += 1;

        // Report metrics periodically
        if let Some(ref mut rep) = reporter { rep.maybe_report(processor.metrics()); }

        if verbose && last_report.elapsed() >= Duration::from_secs(ARB_REPORT_INTERVAL_SECS) {
            // Emit per-connector message/win counts for all connectors
            let ex = format!("{:?}", exchange);
            for id in 0..max_conn {
                let m = msgs.get(&id).copied().unwrap_or(0);
                let w = wins.get(&id).copied().unwrap_or(0);
                log::info!("[arb][{}][{}@{}] conn={} msgs={} wins={}", index, ex, instrument, id, m, w);
            }
            last_report = Instant::now();
        }
    }
    // Stop all connector tasks
        for h in handles { h.abort(); }
    if verbose {
        let ex = format!("{:?}", exchange);
        let total: u64 = wins.values().copied().sum();
        log::info!(
            "[arb][{}][{}@{}] final summary: total_msgs={:?} total_wins={} per_conn_wins={:?}",
            index, ex, instrument, msgs.values().sum::<u64>(), total, wins
        );
    }
    Ok(())
}

/// Spawn an arbitrated L2 handler for a single subscription with exchange-specific acceptance rules
pub async fn run_arbitrated_l2(
    subscription: SubscriptionSpec,
    index: usize,
    verbose: bool,
    okx_swap_registry: Option<Arc<InstrumentRegistry>>,
    okx_spot_registry: Option<Arc<InstrumentRegistry>>,
    output_tx: mpsc::Sender<StreamData>,
    cancel: CancellationToken,
) -> Result<()> {
    const ARB_REPORT_INTERVAL_SECS: u64 = 5;
    let max_conn = subscription.max_connections.unwrap_or(2).max(1);

    log::info!(
        "[arb][L2][{}][{:?}@{}] starting L2 arbitration with max_connections={}",
        index, subscription.exchange, subscription.instrument, max_conn
    );

    // Build processor once
    let processor = match subscription.exchange {
        Exchange::BinanceFutures => ProcessorFactory::create_binance_processor(),
        Exchange::OkxSwap => ProcessorFactory::create_processor(
            crate::types::ExchangeId::OkxSwap,
            okx_swap_registry.clone(),
        ),
        Exchange::OkxSpot => ProcessorFactory::create_processor(
            crate::types::ExchangeId::OkxSpot,
            okx_spot_registry.clone(),
        ),
        Exchange::Deribit => ProcessorFactory::create_deribit_processor(),
    };
    let mut processor = processor;

    // Metrics reporter for L2
    let mut reporter = if verbose { Some(MetricsReporter::new(subscription.instrument.clone(), 10)) } else { None };
    if let Some(ref mut rep) = reporter {
        let ex = match subscription.exchange { Exchange::BinanceFutures => crate::types::ExchangeId::BinanceFutures, Exchange::OkxSwap => crate::types::ExchangeId::OkxSwap, Exchange::OkxSpot => crate::types::ExchangeId::OkxSpot, Exchange::Deribit => crate::types::ExchangeId::Deribit };
        rep.set_exchange(ex);
        rep.set_stream_type("L2");
    }

    let (raw_tx, mut raw_rx) = mpsc::channel::<(usize, RawMessage)>(10000);
    // Channel to receive connector peer IPs
    let (ip_tx, mut ip_rx) = mpsc::channel::<(usize, String)>(max_conn * 2);

    let instrument = subscription.instrument.clone();

    // Spawn connectors with subscribe + optional snapshot/sync for Binance
    let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut conn_tokens: Vec<CancellationToken> = Vec::new();
    for conn_id in 0..max_conn {
        let exchange = subscription.exchange;
        let tx = raw_tx.clone();
        let ip_tx_inner = ip_tx.clone();
        let symbol = instrument.clone();
        let cancel_child = cancel.child_token();
        let cancel_child_for_task = cancel_child.clone();
        let handle = tokio::spawn(async move {
            // Create connector
            let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match exchange {
                Exchange::BinanceFutures => Box::new(crate::exchanges::binance::BinanceFuturesConnector::new()),
                Exchange::OkxSwap => {
                    let reg = Arc::new(InstrumentRegistry::new());
                    Box::new(crate::exchanges::okx::OkxConnector::new_swap(reg))
                }
                Exchange::OkxSpot => {
                    let reg = Arc::new(InstrumentRegistry::new());
                    Box::new(crate::exchanges::okx::OkxConnector::new_spot(reg))
                }
                Exchange::Deribit => {
                    let cfg = match crate::exchanges::deribit::DeribitConfig::from_env() {
                        Ok(c) => c,
                        Err(e) => {
                            log::error!("[arb] deribit config error conn={}: {}", conn_id, e);
                            return;
                        }
                    };
                    Box::new(crate::exchanges::deribit::DeribitConnector::new(cfg))
                }
            };

            if let Err(e) = connector.connect().await {
                log::error!("[arb][L2] connect failed conn={}: {}", conn_id, e);
                return;
            }
            if let Err(e) = connector.subscribe_l2(&symbol).await {
                log::error!("[arb][L2] subscribe L2 failed conn={}: {}", conn_id, e);
                return;
            }

            // Binance requires snapshot reconciliation to ensure continuity
            if let Exchange::BinanceFutures = exchange {
                match connector.get_snapshot(&symbol).await {
                    Ok(snapshot) => {
                        if let Err(e) = connector.prepare_l2_sync(&snapshot) {
                            log::error!("[arb][L2] prepare_l2_sync failed conn={}: {}", conn_id, e);
                        }
                        if let Some(replay) = connector.take_l2_replay_messages() {
                            for rm in replay {
                                if tx.send((conn_id, rm)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => log::error!("[arb][L2] snapshot failed conn={}: {}", conn_id, e),
                }
            }

            if verbose { log::info!("[arb][L2] connected conn={} {:?}@{}", conn_id, exchange, symbol); }
            if let Some(ip) = connector.peer_addr() { let _ = ip_tx_inner.send((conn_id, ip.to_string())).await; }

            loop {
                tokio::select! {
                    _ = cancel_child_for_task.cancelled() => { let _ = connector.disconnect().await; break; }
                    msg = connector.next_message() => {
                        match msg {
                            Ok(Some(raw)) => { if tx.send((conn_id, raw)).await.is_err() { break; } }
                            Ok(None) => { if verbose { log::info!("[arb][L2] disconnected conn={}", conn_id); } break; }
                            Err(e) => { log::error!("[arb][L2] next_message error conn={}: {}", conn_id, e); break; }
                        }
                    }
                }
            }
        });
        handles.push(handle);
        conn_tokens.push(cancel_child);
        if conn_id + 1 < max_conn {
            sleep(Duration::from_secs(1)).await;
        }
    }

    // Keep a clone for rotation spawns; drop the original sender
    let raw_tx_for_rotation = raw_tx.clone();
    drop(raw_tx);

    // First-arrival arbitration (no continuity gating): dedupe by event key
    let mut seen_events: HashSet<(i64, i64)> = HashSet::new(); // (first_update_id, update_id)
    let mut first_wins: HashMap<usize, u64> = HashMap::new(); // cumulative
    let mut first_wins_window: HashMap<usize, u64> = HashMap::new(); // per-rotation window
    let mut msgs: HashMap<usize, u64> = HashMap::new();
    let mut last_report = Instant::now() - Duration::from_secs(ARB_REPORT_INTERVAL_SECS);
    let mut rotation_interval = tokio::time::interval(Duration::from_secs(60));
    let mut msgs_window: HashMap<usize, u64> = HashMap::new();
    let mut conn_ip: HashMap<usize, String> = HashMap::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => { break; }
            _ = rotation_interval.tick() => {
            // Rotation tick: pick losing connector by window wins and rotate it
            let mut loser_id: Option<usize> = None;
            let mut loser_wins = u64::MAX;
            for id in 0..max_conn {
                let w = first_wins_window.get(&id).copied().unwrap_or(0);
                if w < loser_wins { loser_wins = w; loser_id = Some(id); }
            }
            if let Some(id) = loser_id {
                log::info!("[arb][L2][{}] rotating conn={} (window_first={})", index, id, loser_wins);
                // cancel and abort
                if let Some(tok) = conn_tokens.get(id) { tok.cancel(); }
                if let Some(h) = handles.get(id) { h.abort(); }
                // spawn replacement
                let exchange = subscription.exchange;
                    let tx = raw_tx_for_rotation.clone();
                    let ip_tx2 = ip_tx.clone();
                let symbol = instrument.clone();
                let cancel_child = cancel.child_token();
                let cancel_child_for_task = cancel_child.clone();
                let handle = tokio::spawn(async move {
                    let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match exchange {
                        Exchange::BinanceFutures => Box::new(crate::exchanges::binance::BinanceFuturesConnector::new()),
                        Exchange::OkxSwap => {
                            let reg = Arc::new(InstrumentRegistry::new());
                            Box::new(crate::exchanges::okx::OkxConnector::new_swap(reg))
                        }
                        Exchange::OkxSpot => {
                            let reg = Arc::new(InstrumentRegistry::new());
                            Box::new(crate::exchanges::okx::OkxConnector::new_spot(reg))
                        }
                        Exchange::Deribit => {
                            let cfg = match crate::exchanges::deribit::DeribitConfig::from_env() { Ok(c)=>c, Err(e)=>{ log::error!("[arb] deribit config error conn={}: {}", id, e); return; } };
                            Box::new(crate::exchanges::deribit::DeribitConnector::new(cfg))
                        }
                    };
                    if let Err(e) = connector.connect().await { log::error!("[arb][L2] connect failed conn={}: {}", id, e); return; }
                    if let Err(e) = connector.subscribe_l2(&symbol).await { log::error!("[arb][L2] subscribe L2 failed conn={}: {}", id, e); return; }
                    if let Exchange::BinanceFutures = exchange {
                        match connector.get_snapshot(&symbol).await {
                            Ok(snapshot) => {
                                if let Err(e) = connector.prepare_l2_sync(&snapshot) { log::error!("[arb][L2] prepare_l2_sync failed conn={}: {}", id, e); }
                                if let Some(replay) = connector.take_l2_replay_messages() { for rm in replay { if tx.send((id, rm)).await.is_err() { return; } } }
                            }
                            Err(e) => log::error!("[arb][L2] snapshot failed conn={}: {}", id, e),
                        }
                    }
                    if log::log_enabled!(log::Level::Info) { log::info!("[arb][L2] connected conn={} {:?}@{}", id, exchange, symbol); }
                    if let Some(ip) = connector.peer_addr() { let _ = ip_tx2.send((id, ip.to_string())).await; }
                    loop {
                        tokio::select! {
                            _ = cancel_child_for_task.cancelled() => { let _ = connector.disconnect().await; break; }
                            msg = connector.next_message() => {
                                match msg {
                                    Ok(Some(raw)) => { if tx.send((id, raw)).await.is_err() { break; } }
                                    Ok(None) => { if log::log_enabled!(log::Level::Info) { log::info!("[arb][L2] disconnected conn={}", id); } break; }
                                    Err(e) => { log::error!("[arb][L2] next_message error conn={}: {}", id, e); break; }
                                }
                            }
                        }
                    }
                });
                if let Some(hslot) = handles.get_mut(id) { *hslot = handle; }
                if let Some(tslot) = conn_tokens.get_mut(id) { *tslot = cancel_child; }
                // reset window counters for ALL connections to avoid bias
                first_wins_window.clear();
                msgs_window.clear();
                for cid in 0..max_conn {
                    first_wins_window.insert(cid, 0);
                    msgs_window.insert(cid, 0);
                }
            }
            }
            msg = raw_rx.recv() => {
                let Some((conn_id, raw_msg)) = msg else { break; };
                // drain any ip updates
                while let Ok((id, ip)) = ip_rx.try_recv() { conn_ip.insert(id, ip); }
                let rcv_timestamp = crate::types::time::now_micros();
                let packet_id = processor.next_packet_id();
                let message_bytes = raw_msg.data.len() as u32;
                let stream_data = processor.process_unified_message(
                    raw_msg,
                    &subscription.instrument,
                    rcv_timestamp,
                    packet_id,
                    message_bytes,
                    crate::types::StreamType::L2,
                )?;

        // First-arrival dedupe: forward first event only; drop duplicates from other connection
        if let Some(StreamData::L2(first)) = stream_data.first() {
            let event_key = (first.first_update_id, first.update_id);
            if seen_events.insert(event_key) {
                *first_wins.entry(conn_id).or_insert(0) += 1;
                *first_wins_window.entry(conn_id).or_insert(0) += 1;
                for data in stream_data {
                    if let Err(e) = output_tx.send(data).await {
                        return Err(AppError::pipeline(format!("Output channel send failed: {}", e)));
                    }
                }
            }
        }

                // Count message per conn for visibility
                *msgs.entry(conn_id).or_insert(0) += 1;
                *msgs_window.entry(conn_id).or_insert(0) += 1;

        // Report metrics periodically
        if let Some(ref mut rep) = reporter { rep.maybe_report(processor.metrics()); }

                if verbose && last_report.elapsed() >= Duration::from_secs(ARB_REPORT_INTERVAL_SECS) {
                    let ex = format!("{:?}", subscription.exchange);
                    for id in 0..max_conn {
                        let m = msgs.get(&id).copied().unwrap_or(0);
                        let mw = msgs_window.get(&id).copied().unwrap_or(0);
                        let fw = first_wins.get(&id).copied().unwrap_or(0);
                        let fww = first_wins_window.get(&id).copied().unwrap_or(0);
                let ip = conn_ip.get(&id).map(|s| s.as_str()).unwrap_or("N/A");
                        log::info!(
                            "[arb][L2][{}][{}@{}] conn={} ip={} msgs={} (win={}) first={} (win={})",
                            index, ex, subscription.instrument, id, ip, m, mw, fw, fww
                        );
                    }
                    last_report = Instant::now();
                }
            }
        }
    }

    for h in handles { h.abort(); }
    if verbose {
        let ex = format!("{:?}", subscription.exchange);
        log::info!(
            "[arb][L2][{}][{}@{}] final summary: total_msgs={} total_first={} per_conn_first={:?}",
            index, ex, subscription.instrument,
            msgs.values().sum::<u64>(),
            first_wins.values().copied().sum::<u64>(),
            first_wins
        );
    }
    Ok(())
}


