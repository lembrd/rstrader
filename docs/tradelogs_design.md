## Trade logs design

This document defines a unified, low-latency audit log of all trading-related events persisted to Postgres. The goal is to capture every request/response and exchange-pushed event with enough structure to query quickly and enough raw payload to debug precisely.

### Scope and principles
- **Coverage**: requests (post/cancel/amend/cancel-all), responses, and exchange-pushed events (executions, order updates, trade-lite, etc.).
- **Structure-first, payload-also**: top-level typed columns for fast filters and a `body` JSONB that MUST contain the native raw JSON exactly as it was sent or received (no wrapping or augmentation), with one exception for HTTP requests described below. The raw JSON in `body` is authoritative for debugging.
- **Monotonic local timestamp**: all events include a local microsecond timestamp; exchange timestamps are optional.
- **Non-blocking ingestion**: producers never block; a background worker batches and persists.

## Event taxonomy (LogEventType)
Use a small, stable integer enum to classify events. Values are chosen to allow future grouping by ranges.

- **10 request_order_new**: strategy/account posts a new order (`xcommons::oms::PostRequest`).
- **11 request_order_cancel**: cancel single order (`CancelRequest`).
- **12 request_order_amend**: amend existing order (`AmendRequest`).
- **13 request_cancel_all**: cancel all open orders in a market (`CancelAllRequest`).
- **20 api_response**: REST/WS response to a request (`OrderResponse`) – success or error.
- **30 api_event_execution**: execution report (`XExecution`) from exchange stream.
- **31 api_event_order_update**: order status update without a trade (e.g., pending/new/replaced/canceled/rejected).
- **32 api_event_trade_lite**: lightweight trade fill if an exchange emits a minimal variant.
- **90 connectivity_info**: disconnect/reconnect/resubscribe events (optional but recommended for incident timelines).

These map directly to unified types in `src/xcommons/oms.rs` where applicable: `XExecution`, `OrderResponse`, `PostRequest`, `CancelRequest`, `AmendRequest`, and `CancelAllRequest`.

## Postgres schema
Create a single table for now. Store microseconds as BIGINT for fidelity and expose derived `timestamptz` generated columns for convenience. We will optimize schema/indexing later.

### DDL (Flyway)
Create a new migration `sql/pg/V2__create_tradelog.sql`:

```sql
-- Main table
CREATE TABLE IF NOT EXISTS tradelog (
    id                BIGSERIAL PRIMARY KEY,

    -- local and exchange times in microseconds since epoch (UTC)
    local_ts_us       BIGINT NOT NULL,
    exchange_ts_us    BIGINT,

    -- readable timestamps generated from *_us values
    local_ts          TIMESTAMPTZ GENERATED ALWAYS AS (to_timestamp(local_ts_us / 1e6)) STORED,
    exchange_ts       TIMESTAMPTZ GENERATED ALWAYS AS (to_timestamp(exchange_ts_us / 1e6)) STORED,

    -- classification
    event_type        SMALLINT NOT NULL,

    -- identifiers and fast filters
    req_id            BIGINT,
    cl_ord_id         BIGINT,
    native_ord_id     TEXT,
    market_id         BIGINT,
    account_id        BIGINT,

    -- execution/order attributes (optional depending on event)
    exec_type         SMALLINT,
    ord_status        SMALLINT,
    side              SMALLINT,
    is_taker          BOOLEAN,

    last_px           NUMERIC(38, 18),
    last_qty          NUMERIC(38, 18),
    leaves_qty        NUMERIC(38, 18),
    ord_px            NUMERIC(38, 18),
    ord_qty           NUMERIC(38, 18),
    fee               NUMERIC(38, 18),

    -- responses
    response_status   SMALLINT,
    error_code        TEXT,
    error_message     TEXT,

    -- origin/source (e.g. 'account_state', 'exchange_ws', 'rest')
    source            TEXT,

    -- native raw JSON payload (exact JSON object as sent/received)
    body              JSONB NOT NULL DEFAULT '{}'::jsonb
);

-- Minimal index for now; we'll add more later as needed
CREATE INDEX IF NOT EXISTS tradelog_local_ts_idx ON tradelog (local_ts);
```

Notes:
- `NUMERIC(38, 18)` follows our market data precision elsewhere. Adjust if a specific venue needs more/less.
- Further schema/index optimizations will be added later based on load testing.

### Body (raw payload) requirements

- `body` stores the exact native JSON message as sent or received from the venue.
- Exception — HTTP requests: if a request is made via HTTP and the native request is not a single JSON object (e.g., method + query string + optional body), we MUST encode it into a JSON object ourselves with the following minimal schema:

```json
{
  "transport": "http",
  "method": "GET|POST|DELETE|PUT",
  "path": "/venue/endpoint",
  "query": { "param": "value" },
  "body": { }
}
```

  - Include all query string parameters under `query`.
  - Include the request body under `body` if present (as JSON object if JSON, otherwise as a string); omit if none.
  - Exclude sensitive authentication material: do not store API keys, secrets, or signatures (e.g., `X-MBX-APIKEY`, `apiKey`, `secret`, `signature`).
  - Responses are stored as native JSON as received.

## Mapping from unified types

- **PostRequest → request_order_new (10)**
  - Populate: `req_id`, `cl_ord_id`, `market_id`, `account_id`, `side`, `ord_qty`, `ord_px` (if present).
  - `body`: native raw JSON request as sent to the venue/API; if sent over HTTP, encode per the HTTP exception schema above.

- **CancelRequest → request_order_cancel (11)**
  - Populate: `req_id`, `cl_ord_id` and/or `native_ord_id`, `market_id`, `account_id`.
  - `body`: native raw JSON cancel request; if sent over HTTP, encode per the HTTP exception schema above.

- **AmendRequest → request_order_amend (12)**
  - Populate: `req_id`, `cl_ord_id`/`native_ord_id`, `market_id`, `account_id`, `ord_px`, `ord_qty` if provided.
  - `body`: native raw JSON amend request; if sent over HTTP, encode per the HTTP exception schema above.

- **CancelAllRequest → request_cancel_all (13)**
  - Populate: `req_id`, `market_id`, `account_id`.
  - `body`: native raw JSON cancel-all request; if sent over HTTP, encode per the HTTP exception schema above.

- **OrderResponse → api_response (20)**
  - Populate: `req_id`, `cl_ord_id`/`native_ord_id`, `response_status`, plus `exec_*` columns if the response includes an `exec`.
  - `body`: native raw JSON response (REST/WS) as received from the venue.

- **XExecution → api_event_execution (30)**
  - Populate: `exec_type`, `ord_status`, `side`, `is_taker`, `last_px`, `last_qty`, `leaves_qty`, `ord_px`, `ord_qty`, `fee`, `market_id`, `account_id`, `cl_ord_id`, `native_ord_id`.
  - `body`: native raw JSON websocket event from the venue.

- **Order updates/trade-lite (31, 32)**
  - Populate fields as available; keep the authoritative native JSON payload in `body` for venue-specific details.

All timestamps: `local_ts_us` is taken at the point of event creation/receipt; `exchange_ts_us` from the venue payload when present.

## Ingestion architecture

- **Producer**: trading components (primarily `src/trading/account_state/mod.rs`) create `TradeLogEvent` structs and push to a bounded, lock-free queue (e.g., `crossbeam_channel::Sender` with a capacity tuned to absorb bursts). Dropping oldest vs. backpressure is a config toggle; default is backpressure-free by dropping and emitting a metric.
- **Worker**: a `TradeLog` async task reads, batches (e.g., size 1000 or 50ms), and persists in a single transaction per batch.
  - Preferred path: `COPY` via `tokio-postgres` COPY IN for maximum throughput.
  - Fallback: prepared `INSERT ... VALUES` with statement caching.
  - On failure: write the batch to a local JSONL spill file and retry with exponential backoff.
- **Metrics**: export batch sizes, queue depth, dropped events, insert latency, and error counts.

## Retention and partition rotation

- **Default retention**: 90 days of hot data. Detach older partitions and archive to object storage.
- **Archival format**: optional export to Parquet using our existing `output::parquet` infrastructure, keyed by day/market.
- **Vacuum/Analyze**: run on active partitions off-peak; autovacuum should suffice with batch inserts.

## Example records

Example `request_order_new` body (illustrative unified view; actual `body` stores the venue-native JSON):

```json
{
  "req_id": 123456789,
  "timestamp": 1737485965123456,
  "cl_ord_id": 555000111,
  "market_id": 10042,
  "account_id": 7,
  "side": "Buy",
  "qty": 1.0,
  "price": 42000.5,
  "ord_mode": "MLimit",
  "tif": "TifGoodTillCancel",
  "post_only": true,
  "reduce_only": false,
  "metadata": {"strat": "naive_mm"}
}
```

Example `api_event_execution` body (illustrative unified view; actual `body` stores the venue-native JSON):

```json
{
  "timestamp": 1737485965222000,
  "rcv_timestamp": 1737485965223456,
  "market_id": 10042,
  "account_id": 7,
  "exec_type": "XTrade",
  "side": "Buy",
  "native_ord_id": "8745632190",
  "cl_ord_id": 555000111,
  "orig_cl_ord_id": -1,
  "ord_status": "OStatusPartiallyFilled",
  "last_qty": 0.4,
  "last_px": 42000.5,
  "leaves_qty": 0.6,
  "ord_qty": 1.0,
  "ord_price": 42000.5,
  "ord_mode": "MLimit",
  "tif": "TifGoodTillCancel",
  "fee": 0.0002,
  "native_execution_id": "99887766",
  "metadata": {},
  "is_taker": false
}
```

## Open questions / decisions

- Do we want to emit a separate `api_event_order_update` for every order status change if we already log detailed execution events? - answer YES
- For responses that include an execution, do we log both `api_response` and `api_event_execution`? - answer YES
- Exact queue behavior on overflow (drop-oldest vs. block): default is drop-oldest with a metric and warning log. - answer drop-oldest with warn log