### QuestDB Sink: Low-latency batched write design and implementation plan

This plan introduces a new sink that writes L2 and Trades streams to QuestDB with minimal latency, predictable batching, and clean architecture. It also covers schema design, connection method selection, local dev tooling, and integration testing.

## Goals and constraints
- Minimize end-to-end latency and tail jitter under sustained throughput.
- Preserve current abstractions: stream-type agnostic batching, per-stream buffers, and periodic flushing (similar to `MultiStreamParquetSink`).
- Idempotent ingestion when possible (retries/replays must not create duplicates).
- Operational simplicity: containerized QuestDB, health checks, fast local spin-up.

## Chosen ingestion method
- Protocol: QuestDB ILP (InfluxDB Line Protocol) over TCP on port 9009 for ingestion.
  - Rationale: highest-throughput ingestion path; supports backpressure via TCP; simple line-oriented serialization; low overhead.
- Control plane: HTTP SQL (`/exec`) on port 9000 for DDL (CREATE TABLE), health checks and test verification.
  - Rationale: easy to bootstrap schemas, verify row counts, and run lightweight queries.

Notes
- ILP timestamp unit is nanoseconds. Our internal timestamps are in microseconds. Multiply by 1_000 when emitting ILP timestamps.
- Pre-create tables with WAL enabled and, where reasonable, dedup upsert keys for idempotency.

## Table schemas per stream

General rules
- One designated `TIMESTAMP` column named `ts` for partitioning and ordering (exchange timestamp). Partition by `DAY`. WAL on by default.
- Use `SYMBOL` type for low-cardinality strings (e.g., `exchange`, `ticker`, `side`, `action`).
- Keep high-cardinality values (e.g., `trade_id`, `order_id`) as `STRING` to avoid symbol map pressure.
- Store receive timestamp `rcv_ts` as `TIMESTAMP` (ingested as LONG ns via ILP; QuestDB coerces LONG→TIMESTAMP when column type is TIMESTAMP).

L2 updates
```
CREATE TABLE IF NOT EXISTS l2_updates (
  ts TIMESTAMP,
  rcv_ts TIMESTAMP,
  exchange SYMBOL,
  ticker SYMBOL,
  side SYMBOL,      -- BID/ASK
  action SYMBOL,    -- UPDATE/DELETE
  seq_id LONG,
  packet_id LONG,
  price DOUBLE,
  qty DOUBLE,
  update_id LONG,
  first_update_id LONG
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, exchange, ticker, side, price, update_id);
```

Trades
```
CREATE TABLE IF NOT EXISTS trades (
  ts TIMESTAMP,
  rcv_ts TIMESTAMP,
  exchange SYMBOL,
  ticker SYMBOL,
  side SYMBOL,          -- BUY/SELL (direction of taker)
  seq_id LONG,
  packet_id LONG,
  trade_id STRING,
  order_id STRING,
  price DOUBLE,
  qty DOUBLE,
  -- is_buyer_maker removed; redundant with side
) TIMESTAMP(ts) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(ts, exchange, ticker, trade_id);
```

Dedup keys rationale
- L2: we avoid floats in keys when possible, but include `price` to distinguish concurrent price-level updates with same `update_id`. If a future unique event id becomes available, replace keys accordingly.
- Trades: `trade_id` is a natural unique key with `(ts, exchange, ticker)` context.

## ILP line encoding
L2 line (tags first → SYMBOL; fields next → numeric/string/bool; trailing ns timestamp):
```
l2_updates,exchange=BINANCE_FUTURES,ticker=BTCUSDT,side=BID,action=UPDATE \
seq_id=1i,packet_id=1i,price=50000.0,qty=1.5,update_id=123i,first_update_id=122i,rcv_ts=1640995200001000000i 1640995200000000000
```

Trades line:
```
trades,exchange=BINANCE_FUTURES,ticker=BTCUSDT,side=BUY \
seq_id=1i,packet_id=1i,trade_id="T123456",order_id="O789",price=50000.0,qty=0.5,rcv_ts=1640995200001000000i 1640995200000000000
```

## Sink architecture

New module: `src/output/questdb.rs`
- `MultiStreamQuestDbSink` mirrors `MultiStreamParquetSink` but flushes to QuestDB via ILP.
- `QuestDbWriter` per stream key `(stream_type, exchange, symbol)`:
  - Buffers: `Vec<OrderBookL2Update>` and `Vec<TradeUpdate>`.
  - Thresholds: `BATCH_SIZE` (e.g., 1_000) and `FLUSH_INTERVAL_MS` (e.g., 500–1000ms for lower latency than Parquet; tunable via env).
  - On flush: serialize buffered updates into ILP lines (one buffer type at a time to reduce interleaving), write to a long-lived `tokio::net::TcpStream` to `host:9009`.
  - On connect failure: exponential backoff and retry; keep buffers intact until acknowledged write completes.
  - Optional: add a small bounded `mpsc` channel for offloading serialization to a blocking task if CPU pressure observed.

Initialization
- On sink startup, ensure tables exist by issuing `CREATE TABLE IF NOT EXISTS ...` over HTTP SQL (configurable toggle). This avoids ILP auto-create surprises and enables WAL + dedup up front.

Backpressure and reliability
- Keep one persistent TCP connection per sink instance (shared across writers) using a small async write lock.
- If write fails mid-batch, reconnect and retry the entire batch (idempotent thanks to dedup keys). Use a max retry count and surface errors.

Observability
- Emit metrics per writer: last/avg flush size, flush duration, bytes sent, reconnect count, last error, queue depth.

Config surface
- `QUESTDB_HOST` (default `127.0.0.1`), `QUESTDB_ILP_PORT` (default `9009`), `QUESTDB_HTTP_PORT` (default `9000`).
- `QUESTDB_BATCH_SIZE`, `QUESTDB_FLUSH_INTERVAL_MS`.
- `QUESTDB_ENABLE_DDL` (default true) to auto-create tables.

## Integration test strategy
- Startup: external script `scripts/start_questdb.sh` runs QuestDB in Docker and exposes ports 9000/9009/8812/9003.
- Test flow (Rust, `tests/questdb_integration.rs`):
  1. Skip unless `RUN_QUESTDB_TESTS=1` (to avoid CI failures without Docker).
  2. Health check `GET http://localhost:9000/exec?query=select 1`.
  3. Issue DDL for `l2_updates` and `trades` (as above) via HTTP SQL.
  4. Send a small ILP batch over TCP with a few L2 and Trades rows.
  5. Sleep briefly (WAL apply), then query counts and specific values via HTTP SQL; assert matches.

## Implementation steps
1) Add `src/output/questdb.rs` with `MultiStreamQuestDbSink` scaffold (implement buffer, ILP serialization, TCP write, retry, config).
2) Add `scripts/start_questdb.sh` for local spin-up.
3) Add `tests/questdb_integration.rs` that sets up tables, inserts ILP rows, asserts read-backs.
4) Extend `README.md` quickstart section for QuestDB sink and tests.
5) Future: wire CLI flag `--sink questdb` to choose between Parquet and QuestDB sinks; share `StreamData` path.

## Performance considerations
- Use a single `Vec<u8>` buffer per flush to build ILP payload; write once per flush to minimize syscalls.
- Keep tag strings interned or preformatted where possible (e.g., `exchange`, `ticker`, `side`, `action`).
- Tune batch size based on observed latency/throughput; start with 500–1_000 rows per flush and 500ms timer.
- Avoid blocking on DNS by using pre-resolved `SocketAddr` if needed.

## Risks and mitigations
- Float in dedup keys (L2): may cause near-duplicates if price rounding occurs across sources. Mitigate by rounding price to tick size before writing (future improvement).
- WAL backpressure: monitor HTTP `/exec` latency and ingestion lag; expose reconnect counters.
- Type coercion: ensure `rcv_ts` long nanoseconds match TIMESTAMP columns (covered by DDL + unit tests).


