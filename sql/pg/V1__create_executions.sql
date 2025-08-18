-- Postgres DDL for executions table (see docs/trade_account.md)
CREATE TABLE IF NOT EXISTS executions (
  id                    BIGSERIAL PRIMARY KEY,
  account_id            BIGINT NOT NULL,
  exchange              TEXT   NOT NULL,
  instrument            TEXT   NOT NULL,
  xmarket_id            BIGINT NOT NULL,
  exchange_execution_id TEXT   NOT NULL,
  exchange_ts           TIMESTAMPTZ NOT NULL,
  side                  SMALLINT NOT NULL CHECK (side IN (-1, 0, 1)),
  execution_type        INTEGER NOT NULL,
  qty                   NUMERIC NOT NULL,
  price                 NUMERIC NOT NULL,
  fee                   NUMERIC,
  fee_currency          TEXT,
  live_stream           BOOLEAN NOT NULL DEFAULT false,
  exchange_sequence     BIGINT,
  inserted_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Idempotency and fast lookups
CREATE UNIQUE INDEX IF NOT EXISTS ux_executions_account_market_exec_ts
  ON executions (account_id, xmarket_id, exchange_execution_id, exchange_ts);

CREATE INDEX IF NOT EXISTS ix_executions_account_ts_seq_id
  ON executions (account_id, exchange_ts, exchange_sequence, id);


