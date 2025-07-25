use std::path::PathBuf;
use std::sync::Arc;
use arrow::{
    array::{ArrayRef, Float64Array, Int64Array, StringArray, UInt64Array},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::ArrowWriter,
    file::properties::WriterProperties,
};
use tokio::sync::mpsc;
use crate::{
    types::{OrderBookL2Update, Metrics},
    error::{AppError, Result},
};

const BATCH_SIZE: usize = 1000;
const FLUSH_INTERVAL_MS: u64 = 5000;

pub struct ParquetSink {
    writer: Option<ArrowWriter<std::fs::File>>,
    schema: Arc<Schema>,
    buffer: Vec<OrderBookL2Update>,
    output_path: PathBuf,
    metrics: Metrics,
}

impl ParquetSink {
    pub fn new(output_path: PathBuf) -> Result<Self> {
        let schema = Self::create_schema();
        
        Ok(Self {
            writer: None,
            schema,
            buffer: Vec::with_capacity(BATCH_SIZE),
            output_path,
            metrics: Metrics::new(),
        })
    }

    fn create_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("rcv_timestamp", DataType::Int64, false),
            Field::new("exchange", DataType::Utf8, false),
            Field::new("ticker", DataType::Utf8, false),
            Field::new("seq_id", DataType::Int64, false),
            Field::new("packet_id", DataType::Int64, false),
            Field::new("action", DataType::Utf8, false),
            Field::new("side", DataType::Utf8, false),
            Field::new("price", DataType::Float64, false),
            Field::new("qty", DataType::Float64, false),
            Field::new("update_id", DataType::UInt64, false),
            Field::new("first_update_id", DataType::UInt64, false),
        ]))
    }

    fn initialize_writer(&mut self) -> Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let file = std::fs::File::create(&self.output_path)
            .map_err(|e| AppError::io(format!("Failed to create output file: {}", e)))?;

        let props = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            .set_writer_version(parquet::file::properties::WriterVersion::PARQUET_2_0)
            .build();

        let writer = ArrowWriter::try_new(file, self.schema.clone(), Some(props))
            .map_err(|e| AppError::io(format!("Failed to create Parquet writer: {}", e)))?;

        self.writer = Some(writer);
        log::info!("Initialized Parquet writer for: {}", self.output_path.display());
        Ok(())
    }

    pub async fn write_update(&mut self, update: OrderBookL2Update) -> Result<()> {
        self.buffer.push(update);
        self.metrics.increment_messages_processed();

        if self.buffer.len() >= BATCH_SIZE {
            self.flush_batch().await?;
        }

        Ok(())
    }

    pub async fn flush_batch(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        self.initialize_writer()?;

        let batch = self.create_record_batch()?;
        let writer = self.writer.as_mut().unwrap();

        writer.write(&batch)
            .map_err(|e| AppError::io(format!("Failed to write batch: {}", e)))?;

        let batch_size = self.buffer.len();
        self.buffer.clear();
        self.metrics.increment_batches_written();

        log::debug!("Wrote batch of {} records to Parquet", batch_size);
        Ok(())
    }

    fn create_record_batch(&self) -> Result<RecordBatch> {
        let _len = self.buffer.len();

        let timestamp_array = Int64Array::from(
            self.buffer.iter().map(|u| u.timestamp).collect::<Vec<_>>()
        );
        let rcv_timestamp_array = Int64Array::from(
            self.buffer.iter().map(|u| u.rcv_timestamp).collect::<Vec<_>>()
        );
        let exchange_array = StringArray::from(
            self.buffer.iter().map(|u| u.exchange.to_string()).collect::<Vec<_>>()
        );
        let ticker_array = StringArray::from(
            self.buffer.iter().map(|u| u.ticker.clone()).collect::<Vec<_>>()
        );
        let seq_id_array = Int64Array::from(
            self.buffer.iter().map(|u| u.seq_id).collect::<Vec<_>>()
        );
        let packet_id_array = Int64Array::from(
            self.buffer.iter().map(|u| u.packet_id).collect::<Vec<_>>()
        );
        let action_array = StringArray::from(
            self.buffer.iter().map(|u| u.action.to_string()).collect::<Vec<_>>()
        );
        let side_array = StringArray::from(
            self.buffer.iter().map(|u| u.side.to_string()).collect::<Vec<_>>()
        );
        let price_array = Float64Array::from(
            self.buffer.iter().map(|u| u.price).collect::<Vec<_>>()
        );
        let qty_array = Float64Array::from(
            self.buffer.iter().map(|u| u.qty).collect::<Vec<_>>()
        );
        let update_id_array = UInt64Array::from(
            self.buffer.iter().map(|u| u.update_id as u64).collect::<Vec<_>>()
        );
        let first_update_id_array = UInt64Array::from(
            self.buffer.iter().map(|u| u.first_update_id as u64).collect::<Vec<_>>()
        );

        let arrays: Vec<ArrayRef> = vec![
            Arc::new(timestamp_array),
            Arc::new(rcv_timestamp_array),
            Arc::new(exchange_array),
            Arc::new(ticker_array),
            Arc::new(seq_id_array),
            Arc::new(packet_id_array),
            Arc::new(action_array),
            Arc::new(side_array),
            Arc::new(price_array),
            Arc::new(qty_array),
            Arc::new(update_id_array),
            Arc::new(first_update_id_array),
        ];

        RecordBatch::try_new(self.schema.clone(), arrays)
            .map_err(|e| AppError::parse(format!("Failed to create record batch: {}", e)))
    }

    pub async fn close(&mut self) -> Result<()> {
        if !self.buffer.is_empty() {
            self.flush_batch().await?;
        }

        if let Some(mut writer) = self.writer.take() {
            writer.close()
                .map_err(|e| AppError::io(format!("Failed to close Parquet writer: {}", e)))?;
            log::info!("Closed Parquet writer for: {}", self.output_path.display());
        }

        log::info!("Parquet sink metrics: {}", self.metrics);
        Ok(())
    }

    pub fn get_metrics(&self) -> &Metrics {
        &self.metrics
    }
}

pub async fn run_parquet_sink(
    mut rx: mpsc::Receiver<OrderBookL2Update>,
    output_path: PathBuf,
) -> Result<()> {
    let mut sink = ParquetSink::new(output_path)?;
    let mut flush_interval = tokio::time::interval(
        tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS)
    );

    log::info!("Starting Parquet sink");

    loop {
        tokio::select! {
            update = rx.recv() => {
                match update {
                    Some(update) => {
                        if let Err(e) = sink.write_update(update).await {
                            log::error!("Failed to write update to Parquet: {}", e);
                            return Err(e);
                        }
                    }
                    None => {
                        log::info!("Parquet sink channel closed, flushing and shutting down");
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                if let Err(e) = sink.flush_batch().await {
                    log::error!("Failed to flush Parquet batch: {}", e);
                    return Err(e);
                }
            }
        }
    }

    sink.close().await?;
    log::info!("Parquet sink shutdown complete");
    Ok(())
}