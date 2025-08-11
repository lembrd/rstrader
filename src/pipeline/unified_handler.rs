use std::sync::Arc;
// (no-op) placeholder removed; keep imports minimal in hot path
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};
use crate::exchanges::{ExchangeConnector, ExchangeProcessor};
use crate::types::{RawMessage, ExchangeId, StreamData};
use crate::cli::{StreamType, SubscriptionSpec};

/// Unified exchange handler that combines connector and processor into a single async task
/// This eliminates the overhead of dual task spawning and inter-task communication
pub struct UnifiedExchangeHandler {
    connector: Box<dyn ExchangeConnector<Error = AppError>>,
    processor: Box<dyn ExchangeProcessor<Error = AppError>>,
    subscription: SubscriptionSpec,
    index: usize,
    verbose: bool,
    cancellation_token: CancellationToken,
}

impl UnifiedExchangeHandler {
    /// Create a new unified handler with connector and processor
    pub fn new(
        connector: Box<dyn ExchangeConnector<Error = AppError>>,
        processor: Box<dyn ExchangeProcessor<Error = AppError>>,
        subscription: SubscriptionSpec,
        index: usize,
        verbose: bool,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            connector,
            processor,
            subscription,
            index,
            verbose,
            cancellation_token,
        }
    }

    /// Run the unified handler - combines both connector and processor logic
    /// This eliminates the need for intermediate channels and async task overhead
    pub async fn run(
        mut self,
        output_tx: mpsc::Sender<StreamData>,
    ) -> Result<()> {
        log::info!(
            "[{}] Starting UnifiedExchangeHandler for {:?}@{}",
            self.index,
            self.subscription.exchange,
            self.subscription.instrument
        );

        // Initialize metrics reporter if verbose mode
        let mut reporter = if self.verbose {
            Some(crate::pipeline::processor::MetricsReporter::new(
                self.subscription.instrument.clone(),
                10,
            ))
        } else {
            None
        };

        // Connect to exchange
        self.connector.connect().await.map_err(|e| {
            AppError::connection(format!(
                "Failed to connect to {:?}: {}",
                self.subscription.exchange, e
            ))
        })?;

        // Validate symbol
        self.connector
            .validate_symbol(&self.subscription.instrument)
            .await
            .map_err(|e| {
                AppError::validation(format!(
                    "Invalid symbol {} on {:?}: {}",
                    self.subscription.instrument, self.subscription.exchange, e
                ))
            })?;

        // Snapshot handling moved into L2 stream runner to enforce subscribe-first, then snapshot reconciliation.

        // Set exchange info for metrics reporter
        if let Some(ref mut reporter) = reporter {
            reporter.set_exchange(self.get_exchange_id());
            let stream_type_str = match self.subscription.stream_type {
                crate::cli::StreamType::L2 => "L2",
                crate::cli::StreamType::Trades => "TRADES",
            };
            reporter.set_stream_type(stream_type_str);
        }

        // Main processing loop - direct processing without intermediate channels
        match self.subscription.stream_type {
            StreamType::L2 => self.run_l2_stream(output_tx, reporter).await,
            StreamType::Trades => self.run_trade_stream(output_tx, reporter).await,
        }
    }

    /// Run L2 depth stream with direct processing
    async fn run_l2_stream(
        &mut self,
        output_tx: mpsc::Sender<StreamData>,
        mut reporter: Option<crate::pipeline::processor::MetricsReporter>,
    ) -> Result<()> {
        // Subscribe FIRST to begin buffering updates on the connector side
        self.connector
            .subscribe_l2(&self.subscription.instrument)
            .await
            .map_err(|e| AppError::stream(format!("Failed to subscribe to L2 stream: {}", e)))?;

        // Then fetch snapshot and let connector prepare sync (reconciliation)
        let snapshot = self
            .connector
            .get_snapshot(&self.subscription.instrument)
            .await
            .map_err(|e| AppError::data(format!(
                "Failed to get snapshot for {} on {:?}: {}",
                self.subscription.instrument, self.subscription.exchange, e
            )))?;

        // Give connector a chance to prepare internal sync state and reconcile buffered deltas
        self.connector
            .prepare_l2_sync(&snapshot)
            .map_err(|e| AppError::data(format!("Failed to prepare L2 sync: {}", e)))?;

        // If connector exposes replayable buffered messages, process them now to ensure continuity
        if let Some(replay_msgs) = self.connector.take_l2_replay_messages() {
            for raw in replay_msgs {
                match self.process_raw_message(raw).await {
                    Ok(updates) => {
                        for update in updates {
                            if let Err(e) = output_tx.send(update).await {
                                log::error!("Failed to send replayed update: {}", e);
                                return Err(AppError::pipeline("Output channel send failed".to_string()));
                            }
                        }
                    }
                    Err(e) => log::error!("Failed to process replayed message: {}", e),
                }
            }
        }

        // Direct message processing loop with cancellation support
        loop {
            tokio::select! {
                message_result = self.connector.next_message() => {
                    match message_result {
                        Ok(Some(raw_msg)) => {
                            // Process message directly without channel overhead
                            match self.process_raw_message(raw_msg).await {
                                Ok(updates) => {
                                    // Send updates directly to output
                                    for update in updates {
                                        if let Err(e) = output_tx.send(update).await {
                                            log::error!("Failed to send processed update: {}", e);
                                            return Err(AppError::pipeline("Output channel send failed".to_string()));
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to process message: {}", e);
                                    // Continue processing other messages for resilience
                                }
                            }

                            // Report metrics if verbose mode
                            if let Some(ref mut reporter) = reporter {
                                reporter.maybe_report(self.processor.metrics());
                            }
                        }
                        Ok(None) => {
                            log::info!("Stream ended for {}", self.subscription.instrument);
                            break;
                        }
                        Err(e) => {
                            log::error!("Connector error: {}", e);
                            return Err(AppError::stream(format!("Connector failed: {}", e)));
                        }
                    }
                }
                _ = self.cancellation_token.cancelled() => {
                    log::info!("Received cancellation signal for {}", self.subscription.instrument);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Run trade stream with direct processing
    async fn run_trade_stream(
        &mut self,
        output_tx: mpsc::Sender<StreamData>,
        mut reporter: Option<crate::pipeline::processor::MetricsReporter>,
    ) -> Result<()> {
        // Subscribe to trades stream using the dedicated trades subscription method
        self.connector
            .subscribe_trades(&self.subscription.instrument)
            .await
            .map_err(|e| {
                AppError::stream(format!(
                    "Failed to subscribe to trade stream: {}",
                    e
                ))
            })?;

        // Direct message processing loop with cancellation support
        loop {
            tokio::select! {
                message_result = self.connector.next_message() => {
                    match message_result {
                        Ok(Some(raw_msg)) => {
                            // Process message directly
                            match self.process_raw_message(raw_msg).await {
                                Ok(updates) => {
                                    for update in updates {
                                        if let Err(e) = output_tx.send(update).await {
                                            log::error!("Failed to send processed update: {}", e);
                                            return Err(AppError::pipeline("Output channel send failed".to_string()));
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to process message: {}", e);
                                }
                            }

                            // Report metrics
                            if let Some(ref mut reporter) = reporter {
                                reporter.maybe_report(self.processor.metrics());
                            }
                        }
                        Ok(None) => {
                            log::info!("Stream ended for {}", self.subscription.instrument);
                            break;
                        }
                        Err(e) => {
                            log::error!("Connector error: {}", e);
                            return Err(AppError::stream(format!("Connector failed: {}", e)));
                        }
                    }
                }
                _ = self.cancellation_token.cancelled() => {
                    log::info!("Received cancellation signal for {}", self.subscription.instrument);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process raw message directly using the exchange processor
    async fn process_raw_message(&mut self, raw_msg: RawMessage) -> Result<Vec<StreamData>> {
        let rcv_timestamp = crate::types::time::now_micros();
        let packet_id = self.processor.next_packet_id();
        let message_bytes = raw_msg.data.len() as u32;

        let stream_data = self.processor.process_unified_message(
            raw_msg,
            &self.subscription.instrument,
            rcv_timestamp,
            packet_id,
            message_bytes,
            match self.subscription.stream_type {
                crate::cli::StreamType::L2 => crate::types::StreamType::L2,
                crate::cli::StreamType::Trades => crate::types::StreamType::Trade,
            },
        )?;
        
        Ok(stream_data)
    }

    /// Get exchange ID for this handler
    fn get_exchange_id(&self) -> ExchangeId {
        match self.subscription.exchange {
            crate::cli::Exchange::BinanceFutures => ExchangeId::BinanceFutures,
            crate::cli::Exchange::OkxSwap => ExchangeId::OkxSwap,
            crate::cli::Exchange::OkxSpot => ExchangeId::OkxSpot,
            crate::cli::Exchange::Deribit => ExchangeId::Deribit,
        }
    }
}

/// Factory for creating unified exchange handlers
pub struct UnifiedHandlerFactory;

impl UnifiedHandlerFactory {
    /// Create a unified handler for a subscription with appropriate connector and processor
    pub fn create_handler(
        subscription: SubscriptionSpec,
        index: usize,
        verbose: bool,
        okx_swap_registry: Option<Arc<crate::exchanges::okx::InstrumentRegistry>>,
        okx_spot_registry: Option<Arc<crate::exchanges::okx::InstrumentRegistry>>,
        cancellation_token: CancellationToken,
    ) -> Result<UnifiedExchangeHandler> {
        use crate::cli::Exchange;
        use crate::exchanges::binance::BinanceFuturesConnector;
        use crate::exchanges::okx::OkxConnector;
        use crate::exchanges::deribit::{DeribitConnector, DeribitConfig};
        use crate::exchanges::ProcessorFactory;

        // Create connector based on exchange type
        let connector: Box<dyn ExchangeConnector<Error = AppError>> = match subscription.exchange {
            Exchange::BinanceFutures => {
                log::info!("[{}] Creating Binance Futures connector", index);
                Box::new(BinanceFuturesConnector::new())
            }
            Exchange::OkxSwap => {
                log::info!("[{}] Creating OKX SWAP connector", index);
                let registry = okx_swap_registry.clone().ok_or_else(|| {
                    AppError::connection("OKX SWAP registry not initialized".to_string())
                })?;
                Box::new(OkxConnector::new_swap(registry))
            }
            Exchange::OkxSpot => {
                log::info!("[{}] Creating OKX SPOT connector", index);
                let registry = okx_spot_registry.clone().ok_or_else(|| {
                    AppError::connection("OKX SPOT registry not initialized".to_string())
                })?;
                Box::new(OkxConnector::new_spot(registry))
            }
            Exchange::Deribit => {
                log::info!("[{}] Creating Deribit connector", index);
                let config = DeribitConfig::from_env().map_err(|e| {
                    AppError::connection(format!("Failed to load Deribit config: {}", e))
                })?;
                Box::new(DeribitConnector::new(config))
            }
        };

        // Create processor based on exchange type
        let processor = match subscription.exchange {
            Exchange::BinanceFutures => ProcessorFactory::create_binance_processor(),
            Exchange::OkxSwap => {
                ProcessorFactory::create_processor(
                    crate::types::ExchangeId::OkxSwap,
                    okx_swap_registry
                )
            }
            Exchange::OkxSpot => {
                ProcessorFactory::create_processor(
                    crate::types::ExchangeId::OkxSpot,
                    okx_spot_registry
                )
            }
            Exchange::Deribit => ProcessorFactory::create_deribit_processor(),
        };

        Ok(UnifiedExchangeHandler::new(
            connector,
            processor,
            subscription,
            index,
            verbose,
            cancellation_token,
        ))
    }
}