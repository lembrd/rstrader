use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

// No direct dependency on stream types here; keep schema independent of data structs

/// Trait for creating Arrow schemas for different stream types
pub trait SchemaFactory {
    fn create_schema() -> Arc<Schema>;
    fn schema_name() -> &'static str;
}

/// Schema factory for L2 order book updates
pub struct L2SchemaFactory;

impl SchemaFactory for L2SchemaFactory {
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

    fn schema_name() -> &'static str {
        "L2OrderBook"
    }
}

/// Schema factory for trade updates
pub struct TradeSchemaFactory;

impl SchemaFactory for TradeSchemaFactory {
    fn create_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::Int64, false),
            Field::new("rcv_timestamp", DataType::Int64, false),
            Field::new("exchange", DataType::Utf8, false),
            Field::new("ticker", DataType::Utf8, false),
            Field::new("seq_id", DataType::Int64, false),
            Field::new("packet_id", DataType::Int64, false),
            Field::new("trade_id", DataType::Utf8, false),
            Field::new("order_id", DataType::Utf8, true), // Nullable
            Field::new("side", DataType::Utf8, false),
            Field::new("price", DataType::Float64, false),
            Field::new("qty", DataType::Float64, false),
        ]))
    }

    fn schema_name() -> &'static str {
        "Trades"
    }
}

/// Helper function to get schema for specific stream type
pub fn get_schema_for_stream_type(stream_type: &str) -> Arc<Schema> {
    match stream_type {
        "L2" => L2SchemaFactory::create_schema(),
        "TRADES" => TradeSchemaFactory::create_schema(),
        _ => panic!("Unsupported stream type: {}", stream_type),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l2_schema_creation() {
        let schema = L2SchemaFactory::create_schema();
        assert_eq!(schema.fields().len(), 12);
        assert_eq!(L2SchemaFactory::schema_name(), "L2OrderBook");
        
        // Check required fields exist
        assert!(schema.field_with_name("timestamp").is_ok());
        assert!(schema.field_with_name("price").is_ok());
        assert!(schema.field_with_name("qty").is_ok());
    }

    #[test]
    fn test_trade_schema_creation() {
        let schema = TradeSchemaFactory::create_schema();
        assert_eq!(schema.fields().len(), 11);
        assert_eq!(TradeSchemaFactory::schema_name(), "Trades");
        
        // Check required fields exist
        assert!(schema.field_with_name("trade_id").is_ok());
        assert!(schema.field_with_name("side").is_ok());
        
        // Check nullable field
        let order_id_field = schema.field_with_name("order_id").unwrap();
        assert!(order_id_field.is_nullable());
    }

    #[test]
    fn test_get_schema_for_stream_type() {
        let l2_schema = get_schema_for_stream_type("L2");
        let trade_schema = get_schema_for_stream_type("TRADES");
        
        assert_eq!(l2_schema.fields().len(), 12);
        assert_eq!(trade_schema.fields().len(), 11);
        
        // Schemas should be different
        assert_ne!(l2_schema.field(6).name(), trade_schema.field(6).name());
    }

    #[test]
    #[should_panic(expected = "Unsupported stream type")]
    fn test_unsupported_stream_type() {
        get_schema_for_stream_type("UNKNOWN");
    }
}