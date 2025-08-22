//
use crate::xcommons::types::ExchangeId;
use crate::xcommons::types::Metrics;
//
// /// Stream processor for transforming raw messages to unified format
// /// Now uses trait-based architecture for exchange-specific processing
// pub struct StreamProcessor {
//     processor:
//         Box<dyn crate::exchanges::ExchangeProcessor<Error = crate::xcommons::error::AppError>>,
// }
//
// impl StreamProcessor {
//     pub fn new(
//         processor: Box<
//             dyn crate::exchanges::ExchangeProcessor<Error = crate::xcommons::error::AppError>,
//         >,
//     ) -> Self {
//         Self { processor }
//     }
//
//     pub fn process_raw_message(
//         &mut self,
//         raw_msg: RawMessage,
//         symbol: &str,
//     ) -> Result<Vec<OrderBookL2Update>> {
//         // Single timestamp capture at start
//
//         let packet_id = self.processor.next_packet_id();
//         let message_bytes = raw_msg.data.len() as u32;
//
//         // Delegate to exchange-specific processor
//         let updates = self
//             .processor
//             .process_message(raw_msg, symbol, packet_id, message_bytes)?;
//
//         Ok(updates)
//     }
//
//     pub fn update_throughput(&mut self, messages_per_sec: f64) {
//         self.processor.update_throughput(messages_per_sec);
//     }
//
//     pub fn metrics(&self) -> &Metrics {
//         self.processor.metrics()
//     }
// }

pub struct MetricsReporter {
    symbol: String,
    stream_type: Option<String>,
    exchange: Option<ExchangeId>,
    last_report: std::time::Instant,
    report_interval: std::time::Duration,
}

impl MetricsReporter {
    pub fn new(symbol: String, interval_seconds: u64) -> Self {
        Self {
            symbol,
            stream_type: None,
            exchange: None,
            last_report: std::time::Instant::now(),
            report_interval: std::time::Duration::from_secs(interval_seconds),
        }
    }

    pub fn set_exchange(&mut self, exchange_id: ExchangeId) {
        self.exchange = Some(exchange_id);
    }

    pub fn set_stream_type(&mut self, stream_type: &str) {
        self.stream_type = Some(stream_type.to_string());
    }

    pub fn maybe_report(&mut self, metrics: &Metrics) {
        if self.last_report.elapsed() >= self.report_interval {
            let exchange_str = match self.exchange {
                Some(ref exchange) => format!("{:?}", exchange),
                None => "Unknown".to_string(),
            };
            let stream_type_str = match self.stream_type {
                Some(ref stream_type) => format!("{}", stream_type),
                None => "Unknown".to_string(),
            };
            log::info!(
                "📊 Stream Metrics [{}@{}:{}]: {}",
                self.symbol,
                exchange_str,
                stream_type_str,
                metrics
            );

            // Publish updated snapshot and per-stream latencies via universal helper (sink-agnostic)
            let exchange_lbl = match self.exchange {
                Some(e) => e,
                None => ExchangeId::BinanceFutures,
            };
            let stream_lbl = self
                .stream_type
                .clone()
                .unwrap_or_else(|| "Unknown".to_string());
            crate::metrics::publish_stream_metrics(
                &stream_lbl,
                exchange_lbl,
                &self.symbol,
                metrics,
            );
            self.last_report = std::time::Instant::now();
        }
    }
}
//
// pub async fn run_stream_processor(
//     mut rx: mpsc::Receiver<RawMessage>,
//     tx: mpsc::Sender<OrderBookL2Update>,
//     symbol: String,
//     verbose: bool,
//     okx_registry: Option<std::sync::Arc<crate::exchanges::okx::InstrumentRegistry>>,
// ) -> Result<()> {
//     // We'll determine the exchange from the first message
//     let mut processor: Option<StreamProcessor> = None;
//
//     let mut reporter = if verbose {
//         Some(MetricsReporter::new(symbol.clone(), 10)) // Report every 10 seconds
//     } else {
//         None
//     };
//
//     log::info!("Stream processor started for {}", symbol);
//
//     while let Some(raw_msg) = rx.recv().await {
//         // Initialize processor on first message
//         if processor.is_none() {
//             let exchange_processor = crate::exchanges::ProcessorFactory::create_processor(
//                 raw_msg.exchange_id,
//                 okx_registry.clone(),
//             );
//             processor = Some(StreamProcessor::new(exchange_processor));
//
//             // Set exchange info for reporter
//             if let Some(ref mut reporter) = reporter {
//                 reporter.set_exchange(raw_msg.exchange_id.clone());
//             }
//         }
//
//         let processor = processor.as_mut().unwrap();
//
//         match processor.process_raw_message(raw_msg, &symbol) {
//             Ok(updates) => {
//                 for update in updates {
//                     if let Err(e) = tx.send(update).await {
//                         log::error!("Failed to send processed update: {}", e);
//                         return Err(AppError::pipeline("Channel send failed".to_string()));
//                     }
//                 }
//             }
//             Err(e) => {
//                 log::error!("Failed to process raw message: {}", e);
//                 // Continue processing other messages for fail-fast behavior
//             }
//         }
//
//         // Report metrics if verbose mode
//         if let Some(ref mut reporter) = reporter {
//             reporter.maybe_report(processor.metrics());
//         }
//     }
//
//     log::info!("Stream processor for {} completed", symbol);
//     Ok(())
// }
