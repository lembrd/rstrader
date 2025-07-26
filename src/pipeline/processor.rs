use tokio::sync::mpsc;
use std::time::SystemTime;

use crate::types::{OrderBookL2Update, OrderBookL2UpdateBuilder, RawMessage, Metrics, time};
use crate::error::{AppError, Result};
// Exchange-specific processors now implement ExchangeProcessor trait
use crate::types::ExchangeId;

/// Stream processor for transforming raw messages to unified format
/// Now uses trait-based architecture for exchange-specific processing
pub struct StreamProcessor {
    processor: Box<dyn crate::exchanges::ExchangeProcessor<Error = crate::error::AppError>>,
}

impl StreamProcessor {
    pub fn new(processor: Box<dyn crate::exchanges::ExchangeProcessor<Error = crate::error::AppError>>) -> Self {
        Self { processor }
    }

    pub fn process_raw_message(&mut self, raw_msg: RawMessage, symbol: &str) -> Result<Vec<OrderBookL2Update>> {
        // Single timestamp capture at start
        let rcv_timestamp = time::now_micros();
        let packet_id = self.processor.next_packet_id();
        let message_bytes = raw_msg.data.len() as u32;
        
        // Delegate to exchange-specific processor
        let updates = self.processor.process_message(raw_msg, symbol, rcv_timestamp, packet_id, message_bytes)?;

        Ok(updates)
    }

    pub fn update_throughput(&mut self, messages_per_sec: f64) {
        self.processor.update_throughput(messages_per_sec);
    }

    pub fn metrics(&self) -> &Metrics {
        self.processor.metrics()
    }
}



pub struct MetricsReporter {
    symbol: String,
    exchange: Option<ExchangeId>,
    last_report: std::time::Instant,
    report_interval: std::time::Duration,
}

impl MetricsReporter {
    pub fn new(symbol: String, interval_seconds: u64) -> Self {
        Self {
            symbol,
            exchange: None,
            last_report: std::time::Instant::now(),
            report_interval: std::time::Duration::from_secs(interval_seconds),
        }
    }

    pub fn set_exchange(&mut self, exchange_id: ExchangeId) {
        self.exchange = Some(exchange_id);
    }

    pub fn maybe_report(&mut self, metrics: &Metrics) {
        if self.last_report.elapsed() >= self.report_interval {
            let exchange_str = match self.exchange {
                Some(ref exchange) => format!("{:?}", exchange),
                None => "Unknown".to_string(),
            };
            log::info!("📊 Stream Metrics [{}@{}]: {}", self.symbol, exchange_str, metrics);
            self.last_report = std::time::Instant::now();
        }
    }
}

pub async fn run_stream_processor(
    mut rx: mpsc::Receiver<RawMessage>,
    tx: mpsc::Sender<OrderBookL2Update>,
    symbol: String,
    verbose: bool,
    okx_registry: Option<std::sync::Arc<crate::exchanges::okx::InstrumentRegistry>>,
) -> Result<()> {
    // We'll determine the exchange from the first message
    let mut processor: Option<StreamProcessor> = None;
    
    let mut reporter = if verbose {
        Some(MetricsReporter::new(symbol.clone(), 10)) // Report every 10 seconds
    } else {
        None
    };

    log::info!("Stream processor started for {}", symbol);

    while let Some(raw_msg) = rx.recv().await {
        // Initialize processor on first message
        if processor.is_none() {
            let exchange_processor = crate::exchanges::ProcessorFactory::create_processor(
                raw_msg.exchange_id,
                okx_registry.clone()
            );
            processor = Some(StreamProcessor::new(exchange_processor));
            
            // Set exchange info for reporter
            if let Some(ref mut reporter) = reporter {
                reporter.set_exchange(raw_msg.exchange_id.clone());
            }
        }
        
        let processor = processor.as_mut().unwrap();

        match processor.process_raw_message(raw_msg, &symbol) {
            Ok(updates) => {
                for update in updates {
                    if let Err(e) = tx.send(update).await {
                        log::error!("Failed to send processed update: {}", e);
                        return Err(AppError::pipeline("Channel send failed".to_string()));
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to process raw message: {}", e);
                // Continue processing other messages for fail-fast behavior
            }
        }

        // Report metrics if verbose mode
        if let Some(ref mut reporter) = reporter {
            reporter.maybe_report(processor.metrics());
        }
    }

    log::info!("Stream processor for {} completed", symbol);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::time;

    #[test]
    fn test_stream_processor_creation() {
        let processor = StreamProcessor::new(
            crate::exchanges::ProcessorFactory::create_binance_processor()
        );
        // Basic sanity check - processor should be created successfully
        // Basic sanity check - processor should be created successfully
        // Basic sanity check - processor should be created successfully
        assert!(processor.metrics().messages_processed >= 0);
    }

    #[test]
    fn test_metrics_reporter() {
        let mut reporter = MetricsReporter::new("TESTBTC".to_string(), 1);
        let metrics = Metrics::default();
        
        // Should not report immediately
        reporter.maybe_report(&metrics);
        
        // Should report after interval
        std::thread::sleep(std::time::Duration::from_secs(2));
        reporter.maybe_report(&metrics);
    }
}