## XTrader

High-performance, low-latency cryptocurrency market data collector written in Rust. It connects to multiple exchanges, consumes real-time L2 order book and trade streams, normalizes messages into a unified schema, and writes them to Parquet files for analytics and research.

### What it does
- Collects real-time market data from multiple exchanges
  - Exchanges: BINANCE_FUTURES, OKX_SWAP, OKX_SPOT, DERIBIT
  - Streams: L2 (order book deltas), TRADES
- Normalizes all messages to a unified data model (consistent fields across exchanges)
- Writes data per-stream into Parquet files (SNAPPY-compressed, Parquet v2.0) or to QuestDB (ILP TCP)
- Reports detailed performance metrics (parse/transform/code latency, throughput, packet stats)
- Scales across many simultaneous subscriptions using a unified, low-overhead async handler

## Quick start

### Prerequisites
- Rust toolchain (stable) and Cargo
- Network access to exchange WebSocket/REST endpoints
- Optional (only if using Deribit): set environment variables (see below)

### Build
```bash
cargo build --release
```

### Run
Provide one or more subscriptions and an output directory. Subscriptions are comma-separated and use the format:

- stream_type:exchange@instrument
- Examples:
  - L2:OKX_SWAP@BTCUSDT
  - TRADES:BINANCE_FUTURES@ETHUSDT

Supported stream types: `L2`, `TRADES`

Supported exchanges: `BINANCE_FUTURES`, `OKX_SWAP`, `OKX_SPOT`, `DERIBIT`

```bash
RUST_LOG=info \
  cargo run --release -- \
  --subscriptions "L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT" \
  --output-directory ./data/out \
  --verbose
```

Optional flags:
- `--shutdown-after <seconds>`: exit gracefully after N seconds (useful for testing).

### Deribit configuration (optional)
If you subscribe to Deribit streams, set credentials via environment variables or a `.env` file:

```bash
export DERIBIT_CLIENT_ID=your_client_id
export DERIBIT_CLIENT_SECRET=your_client_secret
# Optional (defaults to true):
export DERIBIT_USE_TESTNET=true
```

The app loads `.env` automatically if present.

## CLI reference
```text
xtrader \
  --subscriptions <SUBS> \
  --output-directory <DIR> \
  [--verbose] \
  [--shutdown-after <SECONDS>] \
  [--sink <parquet|questdb>]

Where <SUBS> is a comma-separated list in the form: STREAM:EXCHANGE@INSTRUMENT
Examples:
  L2:OKX_SWAP@BTCUSDT
  TRADES:BINANCE_FUTURES@ETHUSDT
  L2:OKX_SPOT@ADAUSDT,TRADES:DERIBIT@BTC-PERPETUAL
```

Notes:
- Instruments are passed in the exchange’s common notation (e.g., BTCUSDT). The OKX connector converts to OKX’s `BASE-QUOTE[-SWAP]` internally. Deribit uses native names like `BTC-PERPETUAL`.
- The output directory is created if missing and must be writable (only relevant when `--sink parquet`).

## QuestDB sink

XTrader can write directly to QuestDB using ILP over TCP. Schema management is externalized and handled by the provided control script with SQL files in `sql/`.

- Enable via CLI: `--sink questdb`
- ILP connection env vars (optional):
  - `QUESTDB_HOST` (default `127.0.0.1`)
  - `QUESTDB_ILP_PORT` (default `9009`)
- Schema: initialize via `scripts/questdb_ctl.sh init` which applies SQL from `sql/*.sql` (see section below). The Rust code does not create/alter tables.
- Timestamps are written in nanoseconds; internal microseconds are multiplied by 1000 on write.

### Local/remote QuestDB control script
Use the provided control script to manage a local QuestDB instance (Docker) or initialize schema on any instance reachable via HTTP SQL:

```bash
# Start local QuestDB (Docker)
chmod +x scripts/questdb_ctl.sh
./scripts/questdb_ctl.sh start

# Initialize schema (local or remote)
./scripts/questdb_ctl.sh init --host 127.0.0.1 --http-port 9000

# Check status / Stop
./scripts/questdb_ctl.sh status
./scripts/questdb_ctl.sh stop
```

When started locally, this exposes:
- Web console / HTTP SQL: `http://localhost:9000`
- ILP TCP: `localhost:9009`
- Postgres wire: `localhost:8812`
- ILP HTTP: `localhost:9003`

### Running the app with QuestDB sink
```bash
RUST_LOG=info \
  cargo run --release -- \
  --subscriptions "L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT" \
  --sink questdb \
  --shutdown-after 30 \
  --verbose
```

## Output
- Files are written under the provided output directory using the pattern:
  - `{STREAM}_{EXCHANGE}_{SYMBOL}_{SEQUENCE}.parquet` (e.g., `L2_BINANCE_FUTURES_BTCUSDT_000001.parquet`)
- Each file contains Arrow/Parquet batches of 1000 rows by default; flush interval ~5s
- Compression: SNAPPY; Parquet writer version: 2.0
- QuestDB: rows inserted via ILP batching; periodic flush ~500ms; idempotent with WAL + dedup keys (as defined by SQL in `sql/`). Note: `is_buyer_maker` is omitted since it's redundant with trade `side`.

## Data model
XTrader uses a unified schema for all exchanges.

- L2 updates (one row per level change):
  - Common: `timestamp`, `rcv_timestamp`, `exchange`, `ticker`, `seq_id`, `packet_id`
  - L2-specific: `action` (UPDATE/DELETE), `side` (BID/ASK), `price`, `qty`, `update_id`, `first_update_id`
- Trade updates:
  - Common: `timestamp`, `rcv_timestamp`, `exchange`, `ticker`, `seq_id`, `packet_id`
  - Trade-specific: `trade_id`, `order_id?`, `side` (BUY/SELL/UNKNOWN), `price`, `qty`

See `docs/data_schema.md` for the detailed schema.

## Architecture overview

### Entry points
- `src/main.rs`: application lifecycle (env, logging, argument parsing, orchestration)
- `src/cli.rs`: CLI types/validation, subscription parsing (`STREAM:EXCHANGE@INSTRUMENT`)

### Orchestration
- `src/subscription_manager.rs`: spawns one task per subscription and manages lifecycle/shutdown
- `src/pipeline/unified_handler.rs`: UnifiedExchangeHandler combines connector + processor into a single async task to minimize overhead

### Exchange connectors and processors
- Connectors (WebSocket/REST + exchange-specific protocol):
  - `src/exchanges/binance.rs`
  - `src/exchanges/okx.rs`
  - `src/exchanges/deribit.rs`
- Processors implement `ExchangeProcessor` to transform raw messages to the unified data types with detailed performance metrics
  - Factory: `ProcessorFactory` in `src/exchanges/mod.rs`

### Pipeline and types
- `src/types.rs`: unified data structs (`OrderBookL2Update`, `TradeUpdate`, `StreamData`), enums, time helpers, and `Metrics`
- `src/pipeline/processor.rs`: generic `StreamProcessor` and `MetricsReporter` (used by unified handler)

### Output subsystem
- `src/output/multi_stream_sink.rs`: receives unified `StreamData` across all subscriptions and writes Parquet per stream
- `src/output/file_manager.rs`: file naming, sequence tracking, and directory handling
- `src/output/schema.rs` & `src/output/record_batch.rs`: Arrow schemas and batch builders for L2/Trades
- `src/output/questdb.rs`: ILP TCP batching sink (schema managed externally via SQL files)

## Logging & metrics
- Logging via `env_logger`; set `RUST_LOG` (e.g., `RUST_LOG=info`)
- `--verbose` enables periodic metrics logs per subscription: message counts, latencies (parse/transform/code vs overall), packet stats, throughput

## Examples
- Collect OKX SWAP L2 for BTCUSDT into `./data/out`:
```bash
RUST_LOG=info cargo run --release -- \
  --subscriptions "L2:OKX_SWAP@BTCUSDT" \
  --output-directory ./data/out \
  --verbose
```

- Collect Binance Futures trades for ETHUSDT for 30 seconds:
```bash
RUST_LOG=info cargo run --release -- \
  --subscriptions "TRADES:BINANCE_FUTURES@ETHUSDT" \
  --output-directory ./data/out \
  --shutdown-after 30 \
  --verbose
```

- Collect Binance Futures trades for ETHUSDT for 30 seconds into QuestDB:
```bash
RUST_LOG=info cargo run --release -- \
  --subscriptions "TRADES:BINANCE_FUTURES@ETHUSDT" \
  --sink questdb \
  --shutdown-after 30 \
  --verbose
```

## Integration tests (QuestDB)
We provide an integration test that writes to a local QuestDB and verifies ingestion via HTTP SQL.

-- Start QuestDB locally:
```bash
chmod +x scripts/questdb_ctl.sh
./scripts/questdb_ctl.sh start
./scripts/questdb_ctl.sh init --host 127.0.0.1 --http-port 9000
```

- Run the test:
```bash
RUN_QUESTDB_TESTS=1 cargo test --test questdb_integration -- --nocapture
```

- What it does:
  - Assumes schema is initialized via the script and `sql/*.sql`
  - Sends a small ILP TCP batch with one L2 and one trade
  - Queries QuestDB via HTTP SQL and asserts the results

## Development
- Run tests:
```bash
cargo test
```
- Useful docs:
  - `docs/OVERVIEW.md`
  - `docs/data_schema.md`
  - `docs/BINANCE_API_RESEARCH_REPORT.md`
  - `PERFORMANCE_OPTIMIZATIONS_IMPLEMENTED.md`

## Notes & caveats
- Deribit: requires credentials via env vars; testnet enabled by default (`DERIBIT_USE_TESTNET=true`)
- OKX: instrument metadata is cached for correct unit normalization (e.g., contract multiplier); registries are auto-initialized when needed
- Binance L2 synchronization uses REST snapshot + WebSocket deltas and validates sequence continuity

---

If you encounter issues, enable debug logs (`RUST_LOG=debug`) and open an issue with logs and the command you ran.
