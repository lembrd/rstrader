use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::mpsc;

use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};

use crate::error::{AppError, Result};
use crate::output::{
    file_manager::FileManager,
    record_batch::{L2RecordBatchFactory, RecordBatchFactory, TradeRecordBatchFactory},
    schema::{get_schema_for_stream_type, L2SchemaFactory, SchemaFactory, TradeSchemaFactory},
};
use crate::types::{ExchangeId, Metrics, OrderBookL2Update, StreamData, TradeUpdate};

const BATCH_SIZE: usize = 1000;
const FLUSH_INTERVAL_MS: u64 = 5000;

/// Stream-specific writer for handling different data types
struct StreamWriter {
    writer: Option<ArrowWriter<std::fs::File>>,
    l2_buffer: Vec<OrderBookL2Update>,
    trade_buffer: Vec<TradeUpdate>,
    current_file_path: Option<PathBuf>,
    stream_type: String,
    exchange: ExchangeId,
    symbol: String,
    metrics: Metrics,
}

impl StreamWriter {
    fn new(stream_type: String, exchange: ExchangeId, symbol: String) -> Self {
        Self {
            writer: None,
            l2_buffer: Vec::with_capacity(BATCH_SIZE),
            trade_buffer: Vec::with_capacity(BATCH_SIZE),
            current_file_path: None,
            stream_type,
            exchange,
            symbol,
            metrics: Metrics::new(),
        }
    }

    async fn write_stream_data(&mut self, data: StreamData, file_manager: &mut FileManager) -> Result<()> {
        match data {
            StreamData::L2(update) => {
                self.l2_buffer.push(update);
                self.metrics.increment_messages_processed();

                if self.l2_buffer.len() >= BATCH_SIZE {
                    self.flush_l2_batch(file_manager).await?;
                }
            }
            StreamData::Trade(update) => {
                self.trade_buffer.push(update);
                self.metrics.increment_messages_processed();

                if self.trade_buffer.len() >= BATCH_SIZE {
                    self.flush_trade_batch(file_manager).await?;
                }
            }
        }
        Ok(())
    }

    async fn flush_l2_batch(&mut self, file_manager: &mut FileManager) -> Result<()> {
        if self.l2_buffer.is_empty() {
            return Ok(());
        }

        self.ensure_writer_initialized(file_manager)?;

        let schema = L2SchemaFactory::create_schema();
        let batch = L2RecordBatchFactory::create_record_batch(schema, &self.l2_buffer)?;
        
        let writer = self.writer.as_mut().unwrap();
        // Prevent blocking the async scheduler by running blocking parquet write on a blocking thread
        tokio::task::block_in_place(|| {
            writer
                .write(&batch)
                .map_err(|e| AppError::io(format!("Failed to write L2 batch: {}", e)))
        })?;

        let batch_size = self.l2_buffer.len();
        self.l2_buffer.clear();
        self.metrics.increment_batches_written();

        log::debug!("Wrote L2 batch of {} records to Parquet", batch_size);
        Ok(())
    }

    async fn flush_trade_batch(&mut self, file_manager: &mut FileManager) -> Result<()> {
        if self.trade_buffer.is_empty() {
            return Ok(());
        }

        self.ensure_writer_initialized(file_manager)?;

        let schema = TradeSchemaFactory::create_schema();
        let batch = TradeRecordBatchFactory::create_record_batch(schema, &self.trade_buffer)?;
        
        let writer = self.writer.as_mut().unwrap();
        tokio::task::block_in_place(|| {
            writer
                .write(&batch)
                .map_err(|e| AppError::io(format!("Failed to write Trade batch: {}", e)))
        })?;

        let batch_size = self.trade_buffer.len();
        self.trade_buffer.clear();
        self.metrics.increment_batches_written();

        log::debug!("Wrote Trade batch of {} records to Parquet", batch_size);
        Ok(())
    }

    fn ensure_writer_initialized(&mut self, file_manager: &mut FileManager) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let file_path = file_manager.generate_file_path(&self.stream_type, self.exchange, &self.symbol);

        // File create and writer construction are blocking; use block_in_place to avoid starving runtime
        let writer = tokio::task::block_in_place(|| {
            let file = std::fs::File::create(&file_path)
                .map_err(|e| AppError::io(format!("Failed to create output file: {}", e)))?;

            let schema = get_schema_for_stream_type(&self.stream_type);
            let props = WriterProperties::builder()
                .set_compression(parquet::basic::Compression::SNAPPY)
                .set_writer_version(parquet::file::properties::WriterVersion::PARQUET_2_0)
                .build();

            ArrowWriter::try_new(file, schema, Some(props))
                .map_err(|e| AppError::io(format!("Failed to create Parquet writer: {}", e)))
        })?;

        self.writer = Some(writer);
        self.current_file_path = Some(file_path.clone());
        
        log::info!(
            "Initialized {} writer for {}: {}",
            self.stream_type,
            self.symbol,
            file_path.display()
        );
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        // Flush any remaining data
        if !self.l2_buffer.is_empty() {
            // We can't call flush_l2_batch without file_manager, so we create a dummy one
            // In practice, this should be called from the parent with proper file_manager
            log::warn!("Closing stream writer with unflushed L2 data");
        }
        if !self.trade_buffer.is_empty() {
            log::warn!("Closing stream writer with unflushed Trade data");
        }

        if let Some(writer) = self.writer.take() {
            let writer = writer;
            tokio::task::block_in_place(|| {
                writer
                    .close()
                    .map_err(|e| AppError::io(format!("Failed to close Parquet writer: {}", e)))
            })?;
            
            if let Some(path) = &self.current_file_path {
                log::info!("Closed {} writer for: {}", self.stream_type, path.display());
            }
        }

        log::info!("{} stream metrics: {}", self.stream_type, self.metrics);
        Ok(())
    }

    #[allow(dead_code)]
    fn get_metrics(&self) -> &Metrics {
        &self.metrics
    }
}

/// Multi-stream Parquet sink that handles different stream types
pub struct MultiStreamParquetSink {
    file_manager: FileManager,
    writers: HashMap<String, StreamWriter>,
}

impl MultiStreamParquetSink {
    pub fn new(output_directory: PathBuf) -> Result<Self> {
        let mut file_manager = FileManager::new(output_directory)?;
        file_manager.initialize_sequences_from_existing_files()?;

        Ok(Self {
            file_manager,
            writers: HashMap::new(),
        })
    }

    pub async fn write_stream_data(&mut self, data: StreamData) -> Result<()> {
        let (_, _, exchange, symbol, _, _) = data.common_fields();
        let stream_type = data.stream_type();
        
        let key = format!("{}_{}_{}_{}", stream_type, exchange, symbol, "");
        
        let writer = self.writers.entry(key).or_insert_with(|| {
            StreamWriter::new(stream_type.to_string(), exchange, symbol.to_string())
        });

        writer.write_stream_data(data, &mut self.file_manager).await
    }

    pub async fn flush_all(&mut self) -> Result<()> {
        for writer in self.writers.values_mut() {
            // Flush both L2 and Trade buffers
            writer.flush_l2_batch(&mut self.file_manager).await?;
            writer.flush_trade_batch(&mut self.file_manager).await?;
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.flush_all().await?;

        for (key, mut writer) in self.writers.drain() {
            log::debug!("Closing writer: {}", key);
            writer.close().await?;
        }

        log::info!("Multi-stream Parquet sink shutdown complete");
        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_metrics(&self) -> HashMap<String, &Metrics> {
        self.writers
            .iter()
            .map(|(key, writer)| (key.clone(), writer.get_metrics()))
            .collect()
    }
}

/// Run the multi-stream parquet sink task
pub async fn run_multi_stream_parquet_sink(
    mut rx: mpsc::Receiver<StreamData>,
    output_directory: PathBuf,
) -> Result<()> {
    let mut sink = MultiStreamParquetSink::new(output_directory)?;
    let mut flush_interval =
        tokio::time::interval(tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS));

    log::info!("Starting Multi-Stream Parquet sink");

    loop {
        tokio::select! {
            data = rx.recv() => {
                match data {
                    Some(stream_data) => {
                        if let Err(e) = sink.write_stream_data(stream_data).await {
                            log::error!("Failed to write stream data to Parquet: {}", e);
                            return Err(e);
                        }
                    }
                    None => {
                        log::info!("Multi-stream Parquet sink channel closed, flushing and shutting down");
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                if let Err(e) = sink.flush_all().await {
                    log::error!("Failed to flush Parquet batches: {}", e);
                    return Err(e);
                }
            }
        }
    }

    sink.close().await?;
    log::info!("Multi-stream Parquet sink shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{L2Action, OrderSide, TradeSide};
    use tempfile::tempdir;

    #[test]
    fn test_multi_stream_sink_creation() {
        let temp_dir = tempdir().unwrap();
        let sink = MultiStreamParquetSink::new(temp_dir.path().to_path_buf()).unwrap();
        assert_eq!(sink.writers.len(), 0);
    }

    #[tokio::test]
    async fn test_write_l2_data() {
        let temp_dir = tempdir().unwrap();
        let mut sink = MultiStreamParquetSink::new(temp_dir.path().to_path_buf()).unwrap();

        let l2_update = OrderBookL2Update {
            timestamp: 1640995200000000,
            rcv_timestamp: 1640995200001000,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            update_id: 123,
            first_update_id: 122,
            action: L2Action::Update,
            side: OrderSide::Bid,
            price: 50000.0,
            qty: 1.5,
        };

        let stream_data = StreamData::L2(l2_update);
        assert!(sink.write_stream_data(stream_data).await.is_ok());
        assert_eq!(sink.writers.len(), 1);
    }

    #[tokio::test]
    async fn test_write_trade_data() {
        let temp_dir = tempdir().unwrap();
        let mut sink = MultiStreamParquetSink::new(temp_dir.path().to_path_buf()).unwrap();

        let trade_update = TradeUpdate {
            timestamp: 1640995200000000,
            rcv_timestamp: 1640995200001000,
            exchange: ExchangeId::BinanceFutures,
            ticker: "BTCUSDT".to_string(),
            seq_id: 1,
            packet_id: 1,
            trade_id: "T123456".to_string(),
            order_id: Some("O789".to_string()),
            side: TradeSide::Buy,
            price: 50000.0,
            qty: 0.5,
        };

        let stream_data = StreamData::Trade(trade_update);
        assert!(sink.write_stream_data(stream_data).await.is_ok());
        assert_eq!(sink.writers.len(), 1);
    }
}