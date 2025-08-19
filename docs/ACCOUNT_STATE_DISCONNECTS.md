### Trade account state: disconnect handling, risks, and improvements

This document reviews how Binance account state behaves when websockets close and proposes improvements to ensure stability and correctness. It focuses on two websocket flows used in production:

- Binance user data stream (executions/account updates via listenKey)
- Public trade websocket (market trade ticks)

The goal is to make the application robust to connection loss and message gaps, while simplifying recovery of positions which currently involves a non-trivial backfill and replay pipeline.

## Current behavior

### Account state lifecycle (per market)
At startup, `AccountState::run_market` restores state then switches to live:

```320:499:src/trading/account_state/mod.rs
pub async fn run_market(&mut self, market_id: i64, start_epoch_ts: i64, fee_bps: f64, contract_size: f64) -> Result<()> {
    // 1) subscribe to live; buffer
    // 2) compute restart point from datastore (PG)
    // 3) fetch historical via REST strictly greater than last_point/epoch; upsert idempotently
    // 4) replay from epoch to build in-memory positions; emit snapshot
    // 5) drain buffered live and continue online
    // 6) on channel close (disconnect): full restore (REST backfill + replay) and resubscribe loop
}
```

Key points:
- Live executions are persisted idempotently and immediately applied to in-memory positions.
- On live disconnect, the loop performs a full restore using REST (`userTrades`) and replays from epoch to rebuild deterministic state. Then it resubscribes and continues in a loop.

### Binance user data adapter (executions)
`BinanceFuturesAccountAdapter::subscribe_live` creates a `listenKey`, runs a keepalive task, connects to the user data websocket, and forwards `ORDER_TRADE_UPDATE` (preferred, includes fees) and `TRADE_LITE` as `XExecution`s. Deduplication and a short buffer for `TRADE_LITE` are used to avoid double-emits.

```508:742:src/exchanges/binance_account.rs
async fn subscribe_live(&self, account_id: i64, market_id: i64) -> Result<mpsc::Receiver<XExecution>> {
    // POST listenKey; spawn keepalive (PUT) every 20 minutes
    // connect ws: wss://fstream.binance.com/ws/{listenKey}
    // handle ORDER_TRADE_UPDATE with commission fields; prefer over TRADE_LITE
    // buffer TRADE_LITE up to 1.5s awaiting ORTU; otherwise emit TL
    // on ws Close/Error: break; channel closes → AccountState detects disconnect
}
```

Observed behaviors/limitations:
- Keepalive is spawned as an independent task without cancellation. If the stream closes, the keepalive task keeps running for the old `listenKey`, and a resubscribe creates another keepalive task (leak and wasted requests).
- The adapter itself does not attempt reconnect/backoff. It simply exits; `AccountState` owns the restore/resubscribe loop.

### Public trade websocket connector
The public trade stream forwards trade ticks. On close/error it breaks and returns; upstream subscription management is expected to recreate the stream.

```638:711:src/exchanges/binance.rs
async fn start_trade_stream(&mut self, symbol: &str, tx: tokio::sync::mpsc::Sender<RawMessage>) -> Result<()> {
    // connect, forward Message::Text to tx
    // on Close/Error/None: break/return; no automatic reconnect in this layer
}
```

### Persistence and restore
Executions are persisted idempotently to Postgres with a unique constraint on `(account_id, xmarket_id, exchange_execution_id, exchange_ts)` and are reloaded strictly greater than a last point based on `TIMESTAMPTZ`.

```100:203:src/trading/account_state/mod.rs
impl ExecutionsDatastore for PostgresExecutionsDatastore {
    async fn upsert(&mut self, exec: &XExecution) -> Result<()> { /* trades only; idempotent */ }
    async fn load_range(&self, account_id: i64, market_id: i64, gt_ts: i64, _gt_seq: Option<i64>) -> Result<Vec<XExecution>> { /* > gt_ts */ }
    async fn last_point(&self, account_id: i64, market_id: i64) -> Result<Option<(i64, Option<i64>)>> { /* max(exchange_ts) */ }
}
```

The REST backfill path uses `userTrades` with `startTime = (gt_ts/1000)+1` (strictly greater) per symbol, paging by time windows. This closes gaps when the live stream is down.

## Failure modes and risks

- Missing live executions while disconnected: covered by REST backfill; however recovery latency depends on REST responsiveness and pagination across time windows.
- ListenKey keepalive leak: old keepalive tasks continue running after disconnect; on repeated cycles this produces unnecessary PUT traffic and potential rate limit noise.
- No per-stream heartbeat/idle detection: if the user stream stalls silently (no frames), we rely on the tungstenite close/error. Idle detection could reduce MTTR for half-open connections.
- Hot resubscribe loop: `AccountState` resubscribes immediately after each restore. If REST or WS endpoints are temporarily failing, this can spin. There is no exponential backoff/jitter or circuit-breaker at this layer.
- Operational complexity: Full restore/replay on every disconnect is correct, but heavy. For complex multi-market strategies and position derivations, end-to-end correctness is easier to guarantee by restarting the whole app/process after a decisive account-stream failure, rather than attempting in-place resyncs repeatedly.

## Recommendations

### 1) Fix keepalive lifecycle (prevent leaks)
- Tie the keepalive task to the lifespan of the websocket read loop using a cancellation mechanism (e.g., `CancellationToken` or a `oneshot` drop guard). On WS close/error, cancel the keepalive before exiting.
- Ensure a fresh keepalive task starts only for the current `listenKey` and stops when switching to a new key.

### 2) Add reconnection backoff and jitter
- In `AccountState` post-disconnect resubscribe loop, add exponential backoff with jitter for both REST backfill and WS resubscribe errors to avoid thundering herd and to play well with exchange rate limits.
- Track and expose reconnection counters and last successful recovery timestamp via metrics.

### 3) Heartbeat and stall detection for user stream
- Handle Ping/Pong explicitly and track `last_frame_at`. If no frames arrive for N seconds (configurable), proactively close and trigger a resubscribe.
- Optional periodic application-level ping (small subscription `ping` payloads or a timer) if the server does not proactively ping.

### 4) Tighten REST backfill boundaries
- We currently query strictly greater than last persisted microsecond timestamp and translate to `startTime` in milliseconds as `(gt_ts/1000)+1`. This is correct, but document it and assert that `exchange_ts` in DB uses exchange timestamps (ms) promoted to micros to avoid mismatches.
- Consider including `fromId` pagination when available to reduce reliance on time windows for very active markets.

### 5) Make recovery strategy configurable: in-place restore vs. process restart
- Add a config flag (YAML/env) to choose behavior on account stream loss:
  - `recovery_mode = "restore"` (default): keep current behavior (full REST backfill + replay + resubscribe loop).
  - `recovery_mode = "restart"`: on user stream close or repeated failures beyond thresholds, emit a fatal error and exit the process. Rely on an external supervisor (systemd, k8s, container restart policy) to restart cleanly. This simplifies correctness for complex position pipelines.
- Provide thresholds for escalation: e.g., more than X disconnects in Y minutes or continuous downtime > Z seconds → exit.

### 6) Improve observability
- Expose metrics for:
  - `account_reconnections_total{exchange, market}`
  - `account_last_live_exec_timestamp_us{market}` and lag to `now`
  - `account_restore_cycles_total{market}`, `account_restore_duration_ms{market}`
  - `listenkey_keepalive_errors_total`, `ws_idle_timeouts_total`
- Emit structured logs on each phase: subscribe, disconnect reason, backfill size/time, replay time, resubscribe.

### 7) Defensive buffering on close
- Before dropping the user stream, flush any pending `TRADE_LITE` placeholders to reduce the chance of losing events if `ORDER_TRADE_UPDATE` never arrives due to the close.

### 8) Resource hygiene
- Ensure spawned tasks (keepalive, readers) are tracked and cancelled on shutdown/resubscribe to prevent gradual resource accumulation.

## Concrete implementation notes

- Keepalive cancellation in adapter: create a `CancellationToken` passed to the keepalive task; cancel it when the ws loop ends, or scope the task under the same async task using `tokio::select!` with a `shutdown` signal.
- Backoff: use decorrelated jitter backoff starting at 250ms up to 30s for both REST and WS reconnect attempts.
- Idle detection: add a watchdog timer (`tokio::time::interval`) that checks `last_frame_at` and forces a reconnect when exceeding threshold.
- Config: extend `BinanceAccountParams` or a top-level runtime config with `recovery_mode`, `max_disconnects_per_min`, and `max_account_downtime_sec`.
- Exit-on-failure path: propagate a fatal `AppError` from the account runner task to the strategy/runtime; allow the main process to exit so an external orchestrator restarts it.

## Why a restart option can be safer

Position restoration involves complex replay semantics and edge cases (dedup, ordering by microsecond timestamps, mixed `ORDER_TRADE_UPDATE` and `TRADE_LITE`, partial gaps). While the current restore path is correct and deterministic, operational simplicity often favors a clean process restart on account-stream failure. A bounded restart policy reduces in-memory drift risks and keeps the system in a known-good boot path: restore from epoch → live.

## Summary of quick wins

- Add keepalive cancellation to stop leaking tasks after disconnects.
- Add exponential backoff with jitter on restore/resubscribe loops.
- Add idle timeout/heartbeat to detect half-open connections.
- Add a configurable restart policy for account stream loss.
- Instrument recovery and reconnection with metrics for alerting.

These changes will make the application stable under connection loss while giving the option to prefer a full restart on critical account-stream failures.


