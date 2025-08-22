#![allow(dead_code)]
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::xcommons::error::{AppError, Result};
use crate::xcommons::types::SubscriptionSpec;
use crate::xcommons::types::StreamData;
use crate::md::UnifiedHandlerFactory;
use crate::md::latency_arb;
use crate::exchanges::okx::InstrumentRegistry;
use crate::xcommons::types::OrderBookSnapshot;
use crate::md::orderbook::OrderBookManager;

/// Manager for handling multiple concurrent subscriptions using unified handlers
pub struct SubscriptionManager {
    subscriptions: Vec<SubscriptionSpec>,
    handler_handles: Vec<JoinHandle<Result<()>>>,
    cancellation_tokens: Vec<CancellationToken>,
    verbose: bool,
}

impl SubscriptionManager {
    fn spawn_obs_adapter(
        instrument: String,
        mut l2_rx: mpsc::Receiver<StreamData>,
        forward: mpsc::Sender<StreamData>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let mut ob = OrderBookManager::new(instrument);
            while let Some(data) = l2_rx.recv().await {
                if let StreamData::L2(u) = data {
                    let _ = ob.apply_update(&u);
                    let bids = ob.top_bids(10);
                    let asks = ob.top_asks(10);
                    let snapshot = OrderBookSnapshot {
                        market_id: u.market_id,
                        last_update_id: u.update_id,
                        timestamp: u.timestamp,
                        rcv_timestamp: u.rcv_timestamp,
                        sequence: u.seq_id,
                        bids,
                        asks,
                    };
                    if let Err(e) = forward.send(StreamData::Obs(snapshot)).await {
                        log::error!("OBS forward failed: {}", e);
                        break;
                    }
                }
            }
            Ok(())
        })
    }
    pub fn new(subscriptions: Vec<SubscriptionSpec>, verbose: bool) -> Self {
        Self {
            subscriptions,
            handler_handles: Vec::new(),
            cancellation_tokens: Vec::new(),
            verbose,
        }
    }

    /// Spawn unified handlers for all subscriptions
    /// This replaces the dual task spawning pattern with single tasks per subscription
    pub async fn spawn_all_subscriptions(
        &mut self,
        processed_tx: mpsc::Sender<StreamData>,
        okx_swap_registry: Option<Arc<InstrumentRegistry>>,
        okx_spot_registry: Option<Arc<InstrumentRegistry>>,
    ) -> Result<()> {
        for (index, subscription) in self.subscriptions.clone().into_iter().enumerate() {
            // Create cancellation token for this handler
            let cancellation_token = CancellationToken::new();
            
            // If TRADES with arbitration config (max_connections set), use arbitrated runner
            let output_tx = processed_tx.clone();
            // OBS piggybacks on L2 for execution; normalize here to ensure arb path works
            let mut effective_sub = subscription.clone();
            if effective_sub.stream_type == crate::xcommons::types::StreamType::Obs {
                effective_sub.stream_type = crate::xcommons::types::StreamType::L2;
            }
            let handler_handle = if subscription.stream_type == crate::xcommons::types::StreamType::Obs
                && effective_sub.max_connections.unwrap_or(1) > 1
            {
                // OBS with arbitration: run L2 arb into local channel, adapt to snapshots, forward OBS
                let (l2_tx, l2_rx) = mpsc::channel::<StreamData>(10_000);
                let sub_clone = effective_sub.clone();
                let okx_swap = okx_swap_registry.clone();
                let okx_spot = okx_spot_registry.clone();
                let verbose = self.verbose;
                let arb_token = cancellation_token.clone();
                let forward = processed_tx.clone();
                let _arb_guard = tokio::spawn(async move {
                    let _ = latency_arb::run_arbitrated_l2(
                        sub_clone,
                        index,
                        verbose,
                        okx_swap,
                        okx_spot,
                        l2_tx,
                        arb_token,
                    ).await;
                });
                let _obs_handle: tokio::task::JoinHandle<Result<()>> = Self::spawn_obs_adapter(
                    effective_sub.instrument.clone(),
                    l2_rx,
                    forward,
                );
                // Return a harmless handle type-compatible with others
                tokio::spawn(async move { Ok(()) })
            } else if effective_sub.stream_type == crate::xcommons::types::StreamType::Trade
                && subscription.max_connections.unwrap_or(1) > 1
            {
                let sub_clone = effective_sub.clone();
                let okx_swap = okx_swap_registry.clone();
                let okx_spot = okx_spot_registry.clone();
                let verbose = self.verbose;
                let arb_token = cancellation_token.clone();
                tokio::spawn(async move {
                    latency_arb::run_arbitrated_trades(
                        sub_clone,
                        index,
                        verbose,
                        okx_swap,
                        okx_spot,
                        output_tx,
                        arb_token,
                    )
                    .await
                })
            } else if subscription.stream_type == crate::xcommons::types::StreamType::Obs
                && !(effective_sub.max_connections.unwrap_or(1) > 1)
            {
                // OBS without arbitration: run unified L2 handler into local channel, adapt to snapshot
                let (l2_tx, l2_rx) = mpsc::channel::<StreamData>(10_000);
                let handler = UnifiedHandlerFactory::create_handler(
                    effective_sub.clone(),
                    index,
                    self.verbose,
                    okx_swap_registry.clone(),
                    okx_spot_registry.clone(),
                    cancellation_token.clone(),
                )?;
                let forward = processed_tx.clone();
                let _l2_guard = tokio::spawn(async move { let _ = handler.run(l2_tx).await; });
                Self::spawn_obs_adapter(
                    effective_sub.instrument.clone(),
                    l2_rx,
                    forward,
                )
            } else if effective_sub.stream_type == crate::xcommons::types::StreamType::L2
                && subscription.max_connections.unwrap_or(1) > 1
            {
                let sub_clone = effective_sub.clone();
                let okx_swap = okx_swap_registry.clone();
                let okx_spot = okx_spot_registry.clone();
                let verbose = self.verbose;
                let arb_token = cancellation_token.clone();
                tokio::spawn(async move {
                    latency_arb::run_arbitrated_l2(
                        sub_clone,
                        index,
                        verbose,
                        okx_swap,
                        okx_spot,
                        output_tx,
                        arb_token,
                    )
                    .await
                })
            } else {
                // Create unified handler for this subscription
                let handler = UnifiedHandlerFactory::create_handler(
                    effective_sub.clone(),
                    index,
                    self.verbose,
                    okx_swap_registry.clone(),
                    okx_spot_registry.clone(),
                    cancellation_token.clone(),
                )?;
                // Spawn single unified task instead of dual tasks
                tokio::spawn(async move { handler.run(output_tx).await })
            };

            self.handler_handles.push(handler_handle);
            self.cancellation_tokens.push(cancellation_token);
        }

        log::info!(
            "Successfully spawned {} unified handlers (eliminated {} redundant tasks)",
            self.subscriptions.len(),
            self.subscriptions.len() // We eliminated one task per subscription
        );
        
        Ok(())
    }

    /// Wait for any handler to complete (usually due to error or shutdown)
    pub async fn wait_for_any_completion(&mut self) -> Result<()> {
        if self.handler_handles.is_empty() {
            return Ok(());
        }

        // Wait for first handler to complete
        let (result, _index, remaining) =
            futures_util::future::select_all(self.handler_handles.drain(..)).await;

        // Store remaining handles back
        self.handler_handles = remaining;

        match result {
            Ok(Ok(_)) => {
                log::info!("One unified handler completed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                log::error!("Unified handler failed: {}", e);
                Err(e)
            }
            Err(e) => {
                log::error!("Unified handler task panicked: {}", e);
                Err(AppError::internal(format!(
                    "Unified handler task failed: {}",
                    e
                )))
            }
        }
    }

    /// Gracefully shutdown all handlers
    pub async fn shutdown(&mut self) {
        log::info!(
            "Shutting down {} unified handler tasks",
            self.handler_handles.len()
        );

        // Cancel all handlers gracefully
        for token in &self.cancellation_tokens {
            token.cancel();
        }

        // Proactively abort all running handler tasks (covers arbitrated runners without tokens)
        for handle in &self.handler_handles {
            handle.abort();
        }

        // Wait for all handles to complete with timeout
        let timeout_duration = tokio::time::Duration::from_secs(5);
        for handle in self.handler_handles.drain(..) {
            if let Err(_) = tokio::time::timeout(timeout_duration, handle).await {
                log::warn!("Handler did not shutdown within timeout, it may have been aborted");
            }
        }
        
        self.cancellation_tokens.clear();
    }

    /// Get subscription count for metrics
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.len()
    }
}

/// Registry factory for initializing exchange-specific registries
pub struct RegistryFactory;

impl RegistryFactory {
    /// Initialize OKX registries based on subscription requirements
    pub async fn initialize_okx_registries(
        subscriptions: &[SubscriptionSpec],
    ) -> Result<(Option<Arc<InstrumentRegistry>>, Option<Arc<InstrumentRegistry>>)> {
        use crate::xcommons::types::ExchangeId as Exchange;
        use reqwest::Client;

        // Check if we need each registry type
        let needs_swap = subscriptions
            .iter()
            .any(|s| matches!(s.exchange, Exchange::OkxSwap));
        let needs_spot = subscriptions
            .iter()
            .any(|s| matches!(s.exchange, Exchange::OkxSpot));

        let mut okx_swap_registry = None;
        let mut okx_spot_registry = None;

        if needs_swap {
            log::info!("Initializing OKX SWAP instrument registry");
            let client = Client::new();
            let registry = InstrumentRegistry::initialize_with_instruments(
                &client,
                "https://www.okx.com",
                "SWAP",
            )
            .await
            .map_err(|e| {
                AppError::connection(format!("Failed to initialize SWAP registry: {}", e))
            })?;
            okx_swap_registry = Some(Arc::new(registry));
        }

        if needs_spot {
            log::info!("Initializing OKX SPOT instrument registry");
            let client = Client::new();
            let registry = InstrumentRegistry::initialize_with_instruments(
                &client,
                "https://www.okx.com",
                "SPOT",
            )
            .await
            .map_err(|e| {
                AppError::connection(format!("Failed to initialize SPOT registry: {}", e))
            })?;
            okx_spot_registry = Some(Arc::new(registry));
        }

        Ok((okx_swap_registry, okx_spot_registry))
    }
}