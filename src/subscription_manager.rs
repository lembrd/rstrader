use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};
use crate::cli::SubscriptionSpec;
use crate::types::OrderBookL2Update;
use crate::pipeline::{UnifiedExchangeHandler, UnifiedHandlerFactory};
use crate::exchanges::okx::InstrumentRegistry;

/// Manager for handling multiple concurrent subscriptions using unified handlers
pub struct SubscriptionManager {
    subscriptions: Vec<SubscriptionSpec>,
    handler_handles: Vec<JoinHandle<Result<()>>>,
    cancellation_tokens: Vec<CancellationToken>,
    verbose: bool,
}

impl SubscriptionManager {
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
        processed_tx: mpsc::Sender<OrderBookL2Update>,
        okx_swap_registry: Option<Arc<InstrumentRegistry>>,
        okx_spot_registry: Option<Arc<InstrumentRegistry>>,
    ) -> Result<()> {
        for (index, subscription) in self.subscriptions.iter().enumerate() {
            // Create cancellation token for this handler
            let cancellation_token = CancellationToken::new();
            
            // Create unified handler for this subscription
            let handler = UnifiedHandlerFactory::create_handler(
                subscription.clone(),
                index,
                self.verbose,
                okx_swap_registry.clone(),
                okx_spot_registry.clone(),
                cancellation_token.clone(),
            )?;

            // Spawn single unified task instead of dual tasks
            let output_tx = processed_tx.clone();
            let handler_handle = tokio::spawn(async move {
                handler.run(output_tx).await
            });

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
        use crate::cli::Exchange;
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