use arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use std::sync::Arc;

use crate::xcommons::error::{AppError, Result};
use crate::xcommons::types::{OrderBookL2Update, TradeUpdate};

/// Trait for creating Arrow record batches from different data types
pub trait RecordBatchFactory<T> {
    fn create_record_batch(schema: Arc<Schema>, data: &[T]) -> Result<RecordBatch>;
}

/// Record batch factory for L2 order book updates
pub struct L2RecordBatchFactory;

impl RecordBatchFactory<OrderBookL2Update> for L2RecordBatchFactory {
    fn create_record_batch(schema: Arc<Schema>, data: &[OrderBookL2Update]) -> Result<RecordBatch> {
        if data.is_empty() {
            return Err(AppError::parse("Cannot create record batch from empty data".to_string()));
        }

        let timestamp_array = Int64Array::from(data.iter().map(|u| u.timestamp).collect::<Vec<_>>());
        let rcv_timestamp_array = Int64Array::from(
            data.iter()
                .map(|u| u.rcv_timestamp)
                .collect::<Vec<_>>(),
        );
        let exchange_array = StringArray::from(
            data.iter()
                .map(|u| u.exchange.to_string())
                .collect::<Vec<_>>(),
        );
        let ticker_array = StringArray::from(
            data.iter()
                .map(|u| u.ticker.clone())
                .collect::<Vec<_>>(),
        );
        let seq_id_array = Int64Array::from(data.iter().map(|u| u.seq_id).collect::<Vec<_>>());
        let packet_id_array = Int64Array::from(data.iter().map(|u| u.packet_id).collect::<Vec<_>>());
        let action_array = StringArray::from(
            data.iter()
                .map(|u| u.action.to_string())
                .collect::<Vec<_>>(),
        );
        let side_array = StringArray::from(
            data.iter()
                .map(|u| u.side.to_string())
                .collect::<Vec<_>>(),
        );
        let price_array = Float64Array::from(data.iter().map(|u| u.price).collect::<Vec<_>>());
        let qty_array = Float64Array::from(data.iter().map(|u| u.qty).collect::<Vec<_>>());
        let update_id_array = UInt64Array::from(
            data.iter()
                .map(|u| u.update_id as u64)
                .collect::<Vec<_>>(),
        );
        let first_update_id_array = UInt64Array::from(
            data.iter()
                .map(|u| u.first_update_id as u64)
                .collect::<Vec<_>>(),
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

        RecordBatch::try_new(schema, arrays)
            .map_err(|e| AppError::parse(format!("Failed to create L2 record batch: {}", e)))
    }
}

/// Record batch factory for trade updates
pub struct TradeRecordBatchFactory;

impl RecordBatchFactory<TradeUpdate> for TradeRecordBatchFactory {
    fn create_record_batch(schema: Arc<Schema>, data: &[TradeUpdate]) -> Result<RecordBatch> {
        if data.is_empty() {
            return Err(AppError::parse("Cannot create record batch from empty data".to_string()));
        }

        let timestamp_array = Int64Array::from(data.iter().map(|u| u.timestamp).collect::<Vec<_>>());
        let rcv_timestamp_array = Int64Array::from(
            data.iter()
                .map(|u| u.rcv_timestamp)
                .collect::<Vec<_>>(),
        );
        let exchange_array = StringArray::from(
            data.iter()
                .map(|u| u.exchange.to_string())
                .collect::<Vec<_>>(),
        );
        let ticker_array = StringArray::from(
            data.iter()
                .map(|u| u.ticker.clone())
                .collect::<Vec<_>>(),
        );
        let seq_id_array = Int64Array::from(data.iter().map(|u| u.seq_id).collect::<Vec<_>>());
        let packet_id_array = Int64Array::from(data.iter().map(|u| u.packet_id).collect::<Vec<_>>());
        let trade_id_array = StringArray::from(
            data.iter()
                .map(|u| u.trade_id.clone())
                .collect::<Vec<_>>(),
        );
        let order_id_array = StringArray::from(
            data.iter()
                .map(|u| u.order_id.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
        );
        let side_array = StringArray::from(
            data.iter()
                .map(|u| u.side.to_string())
                .collect::<Vec<_>>(),
        );
        let price_array = Float64Array::from(data.iter().map(|u| u.price).collect::<Vec<_>>());
        let qty_array = Float64Array::from(data.iter().map(|u| u.qty).collect::<Vec<_>>());
        // is_buyer_maker removed from TradeUpdate

        let arrays: Vec<ArrayRef> = vec![
            Arc::new(timestamp_array),
            Arc::new(rcv_timestamp_array),
            Arc::new(exchange_array),
            Arc::new(ticker_array),
            Arc::new(seq_id_array),
            Arc::new(packet_id_array),
            Arc::new(trade_id_array),
            Arc::new(order_id_array),
            Arc::new(side_array),
            Arc::new(price_array),
            Arc::new(qty_array),
        ];

        RecordBatch::try_new(schema, arrays)
            .map_err(|e| AppError::parse(format!("Failed to create Trade record batch: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::schema::{L2SchemaFactory, SchemaFactory, TradeSchemaFactory};
    use crate::xcommons::types::{ExchangeId, L2Action};
    use crate::xcommons::oms::Side;

    #[test]
    fn test_l2_record_batch_creation() {
        let schema = L2SchemaFactory::create_schema();
        let updates = vec![
            OrderBookL2Update {
                timestamp: 1640995200000000,
                rcv_timestamp: 1640995200001000,
                exchange: ExchangeId::BinanceFutures,
                ticker: "BTCUSDT".to_string(),
                seq_id: 1,
                packet_id: 1,
                update_id: 123,
                first_update_id: 122,
                action: L2Action::Update,
                side: Side::Buy,
                price: 50000.0,
                qty: 1.5,
            },
        ];

        let batch = L2RecordBatchFactory::create_record_batch(schema, &updates).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 12);
    }

    #[test]
    fn test_trade_record_batch_creation() {
        let schema = TradeSchemaFactory::create_schema();
        let updates = vec![
            TradeUpdate {
                timestamp: 1640995200000000,
                rcv_timestamp: 1640995200001000,
                exchange: ExchangeId::BinanceFutures,
                ticker: "BTCUSDT".to_string(),
                seq_id: 1,
                packet_id: 1,
                trade_id: "T123456".to_string(),
                order_id: Some("O789".to_string()),
                side: Side::Buy,
                price: 50000.0,
                qty: 0.5,
            },
        ];

        let batch = TradeRecordBatchFactory::create_record_batch(schema, &updates).unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 11);
    }

    #[test]
    fn test_empty_data_error() {
        let schema = L2SchemaFactory::create_schema();
        let updates: Vec<OrderBookL2Update> = vec![];
        
        let result = L2RecordBatchFactory::create_record_batch(schema, &updates);
        assert!(result.is_err());
    }
}