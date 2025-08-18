## Naive MM strategy: technical debt and design risks (HFT/low-latency)

### Scope
This report analyzes `src/strats/naive_mm/strategy.rs` and closely related hot-path components: `src/strats/api.rs` (strategy API/runner), `src/app/env.rs`, `src/app/subscription_manager.rs`, `src/md/unified_handler.rs`, `src/xcommons/oms.rs`, and `src/exchanges/binance_account.rs`. Focus is on latency, robustness, and correctness for HFT.

### Key findings in `naive_mm` strategy
- **Hot-path string keys**: Using `String` as map keys/lookup (`live_orders`, `cl_to_symbol`, `market_id_to_symbol`, `StrategyIo.order_txs`) adds hashing/allocations per tick.
  - Prefer `XMarketId` (`i64`) or interned IDs for all hot-path maps and lookups.
- **Unsafe default account id**: Posting/canceling with `account_id` via `unwrap_or_default()` can emit requests with `0` if config is missing.
  - Enforce `Some(account_id)` before any order action; otherwise skip with a warning.
- **Hard-coded tick size**: `tick = 0.1` and adapter uses fixed formatting (`{:.2}` for price, `{:.6}` for qty). Breaks for instruments with other `tickSize`/`stepSize`.
  - Introduce per-symbol filters (tick/step/minNotional) and round consistently; avoid hard-coded decimals.
- **Cancel-all gating risk**: Cancel-all is sent only after `on_position` marks market ready. If positions are delayed/broken, stale orders can remain live.
  - Send cancel-all on first OBS/ready or after a short startup timer as a fallback (idempotent).
- **Latency base timestamp mismatch**: Tick-to-trade uses OBS snapshot timestamp set to local receive time (not exchange ts), skewing semantics.
  - Either rename metrics to “local tick-to-trade” or propagate exchange timestamps from L2 into OBS snapshots and use those.
- **Cancel-then-post displacement**: Uses cancel followed by post when threshold is crossed. Increases time flat and race exposure.
  - Prefer cancelReplace (single-hop) where the venue supports it (Binance Futures does).
- **Missing exchange filters**: No conformance to `tickSize`, `stepSize`, `minNotional` ⇒ rejections/retries and latency jitter.
- **Chatty logging in hot path**: `info!` on every execution/position; should be downgraded or sampled in production.
- **Metrics mutex in hot path**: Multiple `GLOBAL_METRICS.lock()` calls within handlers; consider buffering or lock-free counters.

### Immediate refactors (low risk, high impact)
- **Replace string keys with `XMarketId`**:
  - `live_orders: HashMap<i64, (Option<(i64, f64)>, Option<(i64, f64)>)>`
  - `cl_to_symbol` → `cl_to_market_id: HashMap<i64, i64>`
  - `StrategyIo.order_txs: HashMap<i64, Sender<OrderRequest)>`
- **Validate `account_id`**: Refuse to place/cancel when `binance_account_id.is_none()`; do not use `0`.
- **Eliminate hard-coded tick**: Load per-symbol filters into strategy state during `configure`; round price/qty accordingly. Align adapter formatting with filters.
- **Safer cancel-all**: Send once per symbol on first OBS or via a short timer; keep the existing position-driven gate as an additional guard.
- **Clarify latency metrics**: Either use exchange ts in OBS snapshots or rename metrics to reflect local timing.

### Cross-cutting issues and improvements
- **Unified handler backpressure** (`src/md/unified_handler.rs`): Uses `output_tx.send(...)` (await) for all data, which can add latency and HOL blocking.
  - Use `try_send` with coalesce/drop policy for market data; never drop executions/positions.
- **Strategy runner mailbox** (`src/strats/api.rs`): Single-message dispatch without micro-batching.
  - Add `strategy_mailbox_capacity`, optional micro-batch drain (up to N messages per wakeup), and MD drop/coalesce policy.
- **DB coupling** (`src/strats/api.rs` → Postgres in `StrategyRunner`): Forces `DATABASE_URL` and pool init in the path that wires account state.
  - Make DB optional or move to a dedicated sink; strategy wiring should not depend on DB availability.
- **OMS extensions** (`src/xcommons/oms.rs`): No explicit cancelReplace type.
  - Add `CancelReplaceRequest` where supported and adapter passthrough.
- **OBS timestamping** (`src/app/subscription_manager.rs`): OBS snapshots use local receive time from L2 updates.
  - Propagate exchange timestamps when available on updates and prefer those for latency metrics.

### Binance account adapter (`src/exchanges/binance_account.rs`)
- **Rebuilds Ed25519 signing key per request**: Expensive and avoidable.
  - Parse/store `SigningKey` in `new()`; reuse in `ws_api_request`.
- **Request ID/cleanup**: Uses `format!("xtrader-{}", timestamp)` with ms precision; pending map entries aren’t removed on timeout.
  - Use monotonic ids (`monoseq::next_id()` or a counter) and ensure pending removal on timeout/error.
- **Precision/filters**: Hard-coded formatting `{:.2}`, `{:.6}` for price/qty.
  - Fetch and apply symbol precision/filters; format consistently.
- **Prefer cancelReplace**: Implement `order.cancelReplace` to reduce exposure and round-trips for displacement.
- **Observability**: Add metrics for WS-API round-trip latency, error codes, and timeouts.

### Security and configuration
- **Secrets in repo**: `naive_mm.yaml` contains live API credentials and private key material.
  - Move secrets to environment variables or a secret manager. Keep YAML fields as references (e.g., `${BINANCE_API_KEY}`).

### Latency-oriented improvements
- **Data path**: try-send + coalescing for MD; never drop exec/pos/acks.
- **Execution path**: cancelReplace, precise filters, precomputed rounded price/qty; avoid per-request string allocations where possible.
- **Runtime**: run strategy on a dedicated current-thread Tokio runtime, pin CPU, minimize cross-core migrations.
- **Allocations**: use faster hashers (e.g., `ahash`/`fxhash`) for strategy maps; reserve capacities.
- **Logging**: reduce level in hot handlers; sample if needed.
- **Metrics**: buffer and flush periodically; minimize locking in hot path.

### Suggested edits by file
- `src/strats/naive_mm/strategy.rs`
  - Replace `String` keys with `XMarketId`; store `order_txs` by market id.
  - Enforce non-zero `account_id`; set `metadata` to include `market_id` and `symbol`.
  - Inject per-symbol filters; remove hard-coded tick; round and format accordingly.
  - Use cancelReplace when available; fallback to cancel→post.
  - Trigger cancel-all on first OBS or via timer in addition to position readiness.
  - Downgrade hot-path logs to debug.
- `src/exchanges/binance_account.rs`
  - Pre-parse ed25519 `SigningKey` in `new()`; reuse; handle PEM/DER/base64/hex robustly.
  - Unique request ids with monotonic generator; cleanup pending map on timeout.
  - Implement cancelReplace; apply symbol filters for price/qty; avoid hard-coded precision.
  - Add Prometheus metrics for WS-API latency/errors.
- `src/md/unified_handler.rs`
  - Switch MD to `try_send` (with coalesce/drop policy); keep exec/pos/acks lossless.
- `src/strats/api.rs` + `src/app/env.rs`
  - Add runtime config: mailbox capacity, batch limit, MD drop policy; make DB optional.
- `src/app/subscription_manager.rs`
  - Carry exchange timestamps into OBS snapshots when available.
- `src/xcommons/oms.rs`
  - Add `CancelReplaceRequest` and adapter pass-through for venues that support it.

### Correctness pitfalls to address
- Invalid price/qty precision causes rejections and jitter.
- Cancel-all depending solely on positions can leave orphan orders when positions are delayed.
- Pending-map leak on WS-API timeouts; eventual memory growth.
- Latency metrics labeled as exchange-based but computed from local receive times.

### Minimal implementation plan
1. Strategy hot-path cleanups: `XMarketId` everywhere, strict `account_id`, per-symbol filters in config, remove hard-coded tick, metadata on orders.
2. Adapter improvements: pre-parse signing key, monotonic request ids with cleanup, filter-aware price/qty formatting.
3. Unified handler backpressure: `try_send` + coalescing for MD; never drop exec/pos/acks.
4. Optional DB path and mailbox config (capacity, batch, drop policy).
5. Secrets out of YAML; use env variables.
6. Add cancelReplace; add exchange ts to OBS when available; clarify latency metrics.

### Notes
- Aligns with the unified single-threaded Strategy API, keeping all mutable state within the strategy task and pushing typed messages through a bounded mailbox.
- Prioritize changes that reduce allocations, locking, and cross-task awaits on the hot path.

