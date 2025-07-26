# Market Data Collector: Technical Specification & Project Overview

Market Data Collector is a low-latency, high-performance system designed to collect and process real-time cryptocurrency
market data from multiple exchanges. The system will utilize Rust for performance-critical components and Python for
orchestration and business logic.

## Goals & Key Features

Low Latency: Optimized for minimal message handling latency.

Multi-Exchange Support: Binance (Spot, Futures), Deribit, OKX, Bybit.

Multiple Feed Types: Orderbook L2, BBO, Trades, Tickers.

Multiple Instruments: Handling multiple tickers per exchange.

Stream Latency Arbitrage: Multiple connections per exchange/IP, prioritizing lowest-latency streams.

Robust Error Handling: Exponential backoff, rate limit handling, critical error detection, and alerts via Telegram.

Real-Time Metrics: Tracking throughput, latency, uptime, and error rates.

Docker Packaging: Entire project delivered as Docker images.


## Data Schema

###  Common Fields (All Feed Tables)

Each feed type (orderbook, trade, ticker, etc.) is written to a **dedicated table** with these fields as the minimum schema:

| Field         | Type     | Description                                                                                                   |
|---------------|----------|---------------------------------------------------------------------------------------------------------------|
| timestamp     | int64    | Exchange-provided event timestamp, in microseconds (UTC)                                                      |
| rcv_timestamp | int64    | Local receive timestamp in microseconds (UTC)                                                                 |
| exchange      | enum     | Exchange code (e.g., BINANCE_SPOT, OKX)                                                                       |
| ticker        | string   | Instrument symbol as reported by the exchange                                                                 |
| seq_id        | int64    | Local, monotonically increasing sequence ID per row (to keep order if rows have the same timestamp)           |
| packet_id     | int64    | Local, monotonically increasing sequence for the network packet (to understand that messages arrived at once) |


> **Notes:**
> - All tables MUST include these fields.
> - Both `timestamp` and `rcv_timestamp` are always filled; `seq_id` and `packet_id` enforce local ordering and packet grouping.
> - `exchange` is stored as an enum/integer, never as free-form text.

---

### Feed-Specific Tables

Each feed type is written to its own table, extending the **common fields** above with its own unified, explicit fields.

#### Example: `orderbook_l2`

| Field            | Type  | Description                      |
|------------------|-------|----------------------------------|
| ...common fields |       | See above                        |
| action           | enum  | `NEW` / `UPDATE` / `DELETE`      |
| side             | int   | `BID`=1 / `ASK`=-1 / `UNKNOWN`=0 |
| price            | float | Price                            |
| qty              | float | Size/quantity at level           |

#### Example: `trades`

| Field            | Type  | Description                       |
|------------------|-------|-----------------------------------|
| ...common fields |       | See above                         |
| side             | int   | `BUY`=1 / `SELL`=-1 / `UNKNOWN`=0 |
| price            | float | Price                             |
| qty              | float | Size/quantity at level            |



### Todos:
Check Binance Futures API. (usd margined)

Write parser in rust
Write converter to unified format in rust
Make first implementation for specific exchange/feed worker
Make first implementation for data writer (parquet file for now)

I want standalone application that takes instrument name, exchange, target_file and start to read data, and send data to target.
It should be very flexible stream processing pipeline. Well separated components. For example it should be easy to add batching support,
or change target.

Prepare text document with detailed implementation plan first:
- describe binance schema
- describe key components
- describe overal architecture
- add metrics measure latency of our code parts


Current code you can remove, It was toy project, i tried to make small working feed parser for deribit by myself.