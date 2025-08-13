# Trade Account

## XAccountConfig

```
int64 account_id;        # internal numeric id
ExchangeId exchange;     # enum, e.g. BINANCE/OKX/DERIBIT
string account_name;     # human-friendly label
timestamp epoch;         # inclusive start timestamp to build state from
string api_key;          # API key (store encrypted at rest)
string secret;           # API secret (store encrypted at rest)
string passphrase;       # optional; required by some exchanges
```


## AccountState
Account state maintains per-instrument positions and PnL derived strictly from execution events that change position.

- uses only `XExecution` events where `execution_type` indicates position-affecting events (e.g., FILL, DELIVERY). Non-position events (e.g., FUNDING, REBATE) affect PnL but not position size; they can be modeled separately if needed.
- position math is handled by the Position engine (see `src/position.rs`).


## AccountState pipeline

This component restores and continuously tracks positions with exactly-once semantics.

1) Connect to the live user executions stream and buffer incoming execution reports in-memory. Deduplicate by the exchange-unique execution id where available.
2) Determine the last applied point for the account using a simple aggregation query over `executions` (e.g., `SELECT max(exchange_ts), max(exchange_sequence) ...`), falling back to `account_config.epoch` if there is no data.
3) From the exchange REST API, load historical executions strictly after the last point up to now. Persist them idempotently into the datastore.
4) Load all executions for the replay window from the datastore and apply them in deterministic order to build the in-memory `AccountState`. Ordering priority: exchange sequence > exchange execution id > (exchange_ts, internal id).
5) Switch to online mode: drop buffered live executions with `exchange_ts` less than or equal to the last applied `exchange_ts` (duplicates will also be ignored by idempotent persistence), then apply the remaining buffered ones, then continue with real-time stream. Persist each new execution idempotently.

Exactly-once semantics:

- Persist executions with a unique constraint on the exchange-unique execution id per account. Re-inserts become no-ops.
- The restart point is derived from `max(exchange_ts)` and, where available, `max(exchange_sequence)`, eliminating the need for a separate watermark table.
- Apply deterministically, and rely on idempotent inserts to prevent double-apply across WS/REST sources.


### ExecutionsDatastore

Persistent storage for `XExecution` entities and related request logs. Use PostgreSQL.

Tables (minimal starting point):

1) executions — normalized executions

```
id                       BIGSERIAL PRIMARY KEY
account_id               BIGINT NOT NULL
exchange                 TEXT NOT NULL
instrument               TEXT NOT NULL            -- symbol or instrument id
xmarket_id               BIGINT NOT NULL          -- Hashed id from XMarketId
exchange_execution_id    TEXT NOT NULL            -- unique per exchange/account/instrument
exchange_ts              TIMESTAMPTZ NOT NULL
side                     SMALLINT NOT NULL        -- 1=BUY, -1=SELL, 0=NONE
execution_type           INTEGER NOT NULL         -- int enum: e.g., 0=FILL,1=DELIVERY,2=FUNDING,...
qty                      NUMERIC NOT NULL
price                    NUMERIC NOT NULL
fee                      NUMERIC                  -- signed; quote or native per exchange
fee_currency             TEXT
live_stream              BOOLEAN NOT NULL DEFAULT false  -- true if received via WS
exchange_sequence        BIGINT                   -- when exchange provides ordering
inserted_at              TIMESTAMPTZ NOT NULL DEFAULT now()

-- idempotency and fast lookups
UNIQUE (account_id, xmarket_id, exchange_execution_id, exchange_ts) 
INDEX (account_id, exchange_ts, exchange_sequence, id)
```


### Implementation notes

- Idempotency: use upserts for `executions`; duplicate WS/REST deliveries will be dropped by unique constraints.
- Restart point: compute via `SELECT max(exchange_ts) AS last_ts, max(exchange_sequence) AS last_seq FROM executions WHERE account_id = ? AND exchange = ?` (use `last_ts` only where sequences are unavailable). Backfill strictly greater than that point.
- Ordering per exchange:
  - BINANCE: `exchange_ts` plus `exchange_execution_id` (or `trade_id`) as tie-breaker.
  - OKX: prefer official sequence if provided; otherwise `exchange_ts` + `fill_id`.
  - DERIBIT: use sequence when available; otherwise `exchange_ts` + `trade_id`.
- Live buffering: keep a bounded buffer by time/size; on switch, discard any items with `exchange_ts` ≤ `last_ts` and then drain the rest in order.
