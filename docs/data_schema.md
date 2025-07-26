# XTrader L2 Stream Data Schema

This document describes the output data schema for Level 2 (L2) order book streams in XTrader. All L2 data is unified into a consistent format regardless of the source exchange and exported to Parquet files.

## Schema Overview

The L2 stream produces `OrderBookL2Update` records that represent individual order book level changes. Each record contains timing information, exchange metadata, and the specific price level update.

## Field Specifications

### Core Timing Fields

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `timestamp` | `Int64` | No | Exchange-provided timestamp in microseconds UTC. Original timestamp from the exchange when the update occurred. |
| `rcv_timestamp` | `Int64` | No | Local receive timestamp in microseconds UTC. When XTrader received the message from the exchange WebSocket. |

### Exchange & Instrument Metadata

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `exchange` | `Utf8` | No | Exchange identifier. Values: `"BinanceFutures"`, `"OkxSwap"`, `"OkxSpot"` |
| `ticker` | `Utf8` | No | Trading pair symbol (e.g., `"BTCUSDT"`, `"ETHUSDT"`). Preserved per subscription in multi-subscription mode. |

### Sequencing & Grouping

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `seq_id` | `Int64` | No | Local monotonic sequence ID assigned by XTrader. Increments for each processed update. |
| `packet_id` | `Int64` | No | Network packet grouping ID. Groups updates that arrived in the same WebSocket message. |

### Exchange-Specific Update IDs

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `update_id` | `UInt64` | No | Exchange-specific update identifier. For Binance: `lastUpdateId`. For OKX: sequence from update stream. |
| `first_update_id` | `UInt64` | No | First update ID in the event. For Binance: `U` field. For OKX: typically same as `update_id`. |

### Order Book Level Data

| Field | Type | Nullable | Description |
|-------|------|----------|-------------|
| `action` | `Utf8` | No | Order book action type. Values: `"Update"`, `"Delete"` |
| `side` | `Utf8` | No | Order book side. Values: `"Bid"`, `"Ask"` |
| `price` | `Float64` | No | Price level as floating-point number |
| `qty` | `Float64` | No | Quantity at the price level. `0.0` indicates level deletion. |

## Enumeration Values

### L2Action
- `"Update"` (integer value: 1) - Insert or update a price level
- `"Delete"` (integer value: 2) - Remove a price level

### OrderSide  
- `"Bid"` (integer value: 1) - Buy side of the order book
- `"Ask"` (integer value: -1) - Sell side of the order book

### ExchangeId
- `"BinanceFutures"` (integer value: 1) - Binance Futures exchange
- `"OkxSwap"` (integer value: 2) - OKX Perpetual Swaps
- `"OkxSpot"` (integer value: 3) - OKX Spot trading

## Data Interpretation

### Level Updates
- **Insert/Update**: `action="Update"` with `qty > 0` adds or modifies a price level
- **Delete**: `action="Delete"` OR `qty == 0.0` removes a price level from the order book

### Timing Analysis
- **Network Latency**: `rcv_timestamp - timestamp` gives approximate network + processing delay
- **Sequencing**: Use `seq_id` for local ordering, `update_id` for exchange ordering

### Multi-Subscription Data
In multi-subscription mode, data from different subscriptions is distinguishable by:
- `ticker` field contains the actual instrument (not "MULTI")  
- `exchange` field identifies the source
- Each subscription produces independent `seq_id` sequences

## Parquet Storage Details

- **Compression**: SNAPPY compression applied
- **Batch Size**: 1000 records per batch
- **File Format**: Parquet 2.0
- **Flush Interval**: 5 seconds maximum between flushes

## Example Record

```json
{
  "timestamp": 1703123456789123,
  "rcv_timestamp": 1703123456790234,
  "exchange": "OkxSwap", 
  "ticker": "BTCUSDT",
  "seq_id": 12345,
  "packet_id": 8901,
  "update_id": 987654321,
  "first_update_id": 987654321,
  "action": "Update",
  "side": "Bid", 
  "price": 43250.5,
  "qty": 1.25
}
```

This record represents a bid level update for BTCUSDT on OKX Swap, adding 1.25 quantity at price 43250.5.