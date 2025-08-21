-- Trade log table (initial simple schema; optimize later)
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

    -- strategy instance identifier
    strategy_id       BIGINT NOT NULL,

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


