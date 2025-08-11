### Stream latency arbitrage: research and implementation plan

This document proposes an exchange-aware, low-latency design to exploit feed latency differences by racing multiple connections for the same logical stream and emitting the earliest valid update while guaranteeing correctness and idempotency across the unified pipeline.

### Goals
- **Reduce end-to-end latency**: race several network paths and stream variants; commit the earliest valid update.
- **Preserve correctness**: maintain exchange-specific sequence continuity for L2; dedupe trades by unique IDs; never emit duplicates.
- **Integrate cleanly**: fit into current `UnifiedExchangeHandler` and `ExchangeConnector`/`ExchangeProcessor` architecture with minimal disruption.
- **Operate safely**: per-connection health, backoff, metrics; bounded buffers; predictable fallbacks.

### Where this fits in the current architecture
- One `SubscriptionSpec` today maps to a single `UnifiedExchangeHandler` which owns one `ExchangeConnector` + one `ExchangeProcessor` and streams `StreamData` to the sink.
- For latency arbitrage we introduce an intermediary component that owns N connectors for the same logical subscription and emits a single, deduped stream into the existing handler/processor path.

Proposed new module: `src/pipeline/latency_arb/` with core types:
- `LatencyArbGroup` (per `stream_type + exchange + instrument`): manages multiple connectors, arbitrates earliest valid message, and forwards unified updates downstream.
- `ArbKeyExtractor` (trait, per exchange): extracts minimal keys to decide acceptance/ordering without doing full transformation twice.
- `L2ContinuityChecker` (per exchange): validates L2 continuity when mixing connections.

### Key challenges and solutions
- **Deduplication across connections**
  - **TRADES**: drop by unique trade key (e.g., Binance `trade_id`, OKX `tradeId`, Deribit `trade_id`). Maintain `last_seen_trade_id` (or a small LRU set) per instrument. Forward first arrival only.
  - **L2**: sequence continuity differs by exchange. We must accept only updates that advance the book correctly. Do not naïvely interleave streams.
    - Binance Depth: fields `U` (first_update_id), `u` (final_update_id), `pu` (prev_final_update_id). Accept a message M if `U <= last_applied+1 <= u`; then set `last_applied = u`.
    - OKX Books: fields `prevSeqId`, `seqId`. Accept if `prevSeqId == last_seq_id`; then set `last_seq_id = seqId`.
    - Deribit Book: `change_id` monotonically increases; accept strictly increasing `change_id`. If Deribit ever publishes gap-detectable metadata, extend here.
  - This policy lets any connection “win” only when it delivers the next admissible segment; duplicates and out-of-order future segments are dropped or briefly buffered.

- **Multiple endpoints / IPs**
  - Maintain K connectors per group. Each connector may target:
    - the same hostname (LB may route to different servers),
    - alternate hostnames (e.g., `fstream.binance.com`, `fstream1`, `fstream2`),
    - different update cadences (e.g., Binance `@depth@0ms` vs `@depth@100ms`),
    - optionally, resolved IPs with TLS SNI set to the original hostname.
  - Start with multiple connections to the canonical hostname(s). Add optional DNS A/AAAA expansion in a later phase.

- **Performance considerations**
  - Extract only arbitration keys in hot path (fast JSON field lookup or exchange-specific lightweight parse) to decide acceptance without full transform; perform full `ExchangeProcessor` transformation only for accepted messages.
  - Bound per-connector and group-level queues; prefer drop-old for L2 pre-accept buffers.
  - Preserve current metrics model (code/parse/transform/overall latencies) and add per-connector arrival latency and win-rate.

### High-level dataflow
1) `SubscriptionManager` detects `--arb-*` config; for each `SubscriptionSpec` creates a `LatencyArbGroup` instead of a single `UnifiedExchangeHandler`.
2) `LatencyArbGroup` starts N `ExchangeConnector`s (same exchange+instrument) with optional variant endpoints.
3) Each connector pushes `RawMessage{exchange_id, data, timestamp}` into the group with a `connector_id` tag.
4) Group calls `ArbKeyExtractor` to get `ArbKey`:
   - For L2: `(first_id, final_id, prev_id?)`
   - For TRADES: `(trade_id)`
5) Group’s acceptance logic:
   - TRADES: if `trade_id` not yet seen → accept and forward; else drop.
   - L2: accept if it advances continuity (rules per exchange above). Optionally keep a tiny lookahead buffer keyed by `first_id` to bridge micro-gaps.
6) On accept: forward the original `RawMessage` to the exchange’s `ExchangeProcessor.process_unified_message` with the group-level packet_id/rcv_timestamp, then forward resulting `StreamData` to the sink.
7) Metrics and health are updated per-connector and per-group.

### Acceptance logic (L2) — pseudocode
```rust
// Binance example (bridging rule)
if arb_key.first_id <= last_applied + 1 && last_applied + 1 <= arb_key.final_id {
    accept();
    last_applied = arb_key.final_id;
} else if arb_key.final_id <= last_applied {
    drop_duplicate();
} else {
    maybe_buffer_or_drop();
}

// OKX example (strict continuity)
if arb_key.prev_seq_id == last_seq_id {
    accept();
    last_seq_id = arb_key.seq_id;
} else if arb_key.seq_id <= last_seq_id {
    drop_duplicate();
} else {
    maybe_buffer_or_drop();
}
```

### Exchange-specific key extraction (fast path)
- Add small, zero-copy/minimal parse extractors inside `src/exchanges/`:
  - `binance::extract_depth_keys(&str) -> Option<(U, u, pu)>`
  - `okx::extract_depth_keys(&str) -> Option<(prevSeqId, seqId)>`
  - `deribit::extract_book_keys(&str) -> Option<(change_id)>`
  - Trades: extract `trade_id`/`tradeId`/`trade_seq` similarly.
- Default to full parse fallback only if lightweight extraction fails.

### Connection diversity and DNS/IP strategy
- Mandatory: resolve the exchange hostname to DNS A/AAAA records and use up to `N` distinct IP addresses per subscription (where `N` is the per-subscription max connections or the global default). Set TLS SNI to the canonical hostname while dialing IPs.
- Initiation policy: do not connect all at once. Ramp up connections sequentially with a fixed 5-second delay between connection attempts to spread load and quickly observe early winners.
- Phase 2: add optional alternative hostnames per exchange (config-driven) in addition to IP fanout.
- Phase 3: consider multiple update cadence variants (e.g., Binance `@0ms` and `@100ms`) as further diversity when allowed by rate limits.

### Configuration surface (CLI)
- `--arb-mode <off|simple|multi>`: default `off` (current behavior). `simple` = duplicate same endpoint; `multi` = also use alt hostnames/interval variants where supported.
- `--arb-connections <N>`: number of parallel connections per group (default 2–3).
- `--arb-lookahead <K>`: L2 small buffer size for ahead-of-next segments (default 0 or 2).
- `--arb-dns-fanout`: enable DNS multi-IP dialing (later phase).
- `--arb-variants <comma-list>`: exchange-specific variants, e.g., `binance:depth@0ms,depth@100ms`.
- `--arb-rotation-interval-seconds <S>`: rotation cadence (default 60).
- `--arb-rotate-k <K>`: number of connectors to rotate out per interval (default 1; max floor(N/2)).
- `--arb-min-dwell-intervals <M>`: minimum intervals a connector must stay before eligible to drop (default 2).
- `--arb-cooldown-intervals <C>`: intervals before a dropped endpoint can be re-tried (default 3).
- `--arb-connection-budget-per-exchange <NUM>`: max concurrent connections budget per exchange (global limiter).
- `--arb-rotate-threshold <float>`: optional minimum score gap to trigger rotation (prevents churn).

#### New subscription syntax (per-subscription max connections)
- Format: `STREAM:EXCHANGE@SYMBOL[N]`, where `[N]` is optional and sets the max concurrent connections for that specific subscription.
  - Examples:
    - `L2:BINANCE_FUTURES@BTCUSDT[10]` → up to 10 concurrent connections for this L2 stream.
    - `TRADES:OKX_SWAP@ETHUSDT[3]` → up to 3 concurrent connections for this trade stream.
- Precedence: if `[N]` is provided it overrides `--arb-connections` for that subscription only.
- Validation: enforce exchange-specific connection/rate limits; clamp `N` to allowed budgets and to the number of resolved IPs.

### Metrics and observability
- Per-connector: message arrival rate, average arrival delta to accepted commit, win-rate (% of accepted updates delivered), reconnects/errors.
- Per-group: accepted msg rate, drop reasons (dup, ahead, invalid), buffer pressure, continuity gaps, resync count.
- Reuse existing `Metrics` for parse/transform/code/overall latencies; add a small `ArbMetrics` struct.

#### Verbose mode (`--verbose`) visibility
- Periodic per-host metrics: in verbose mode, display per-connector lines including `connector_id`, `hostname`, `resolved_ip`, `state`, `win_rate`, `accepted`, `dropped`, `avg_arrival_delta_us`, `score`.
- Lifecycle events: log when a host connects, disconnects, reconnects, is rotated in, and rotated out (with reason: low score/health/budget).
- Example (one-liners):
  - `[arb][conn=3][BINANCE][BTCUSDT] connected host=fstream1.binance.com ip=1.2.3.4`
  - `[arb][conn=3][BINANCE][BTCUSDT] metrics win_rate=0.62 accepted=124 dropped=19 avg_delta_us=4200 score=0.51`
  - `[arb][conn=3][BINANCE][BTCUSDT] disconnected err=ConnectionReset`
  - `[arb][rotate] drop conn=2 reason=low_score score=0.12; add conn=5 host=fstream2.binance.com ip=5.6.7.8`

### Dynamic connection rotation (explore/exploit under rate limits)
- We will run up to N concurrent connections per exchange/instrument and continuously measure which connectors are winners (deliver accepted updates first) vs losers (arrive later/never win).
- Every rotation interval (default 60s):
  - Compute per-connector sliding-window metrics: `win_count`, `loss_count`, `win_rate`, `avg_arrival_delta_us`, health/reconnects.
  - Drop losing connectors (bottom K by score) and start new ones to explore alternative paths (other hostnames/intervals/IPs), respecting exchange connection/rate limits.
  - Because latency drifts over time, this rotation runs continuously with hysteresis (min dwell and cooldown) to avoid flapping.
- Respect strict per-exchange connection budgets: we cannot subscribe to every variant indefinitely; we rotate instead of scaling unboundedly.
- Emit rotation events in logs/metrics for observability (who was dropped/added and why).

### Failure modes and recovery
- Connector failure: keep group alive as long as ≥1 connector is healthy; restart failed connectors with backoff.
- L2 continuity gap: trigger exchange-specific resync (existing snapshot+replay logic) using one connector while others continue feeding; after resync, continue arbitration.
- Buffer overflow: drop oldest ahead-of-next entries; never block hot path.

### Implementation plan (phased)
- Phase 0: groundwork
  - Add module `pipeline/latency_arb/` with interfaces: `LatencyArbGroup`, `ArbKeyExtractor`, `L2ContinuityChecker`.
  - Wire feature flags into `cli.rs` and plumbing in `subscription_manager.rs`.

- Phase 1: MVP (TRADES first, then L2 safest exchanges)
  - TRADES: implement multi-connector, dedupe by trade ID; forward earliest arrival. Low risk.
  - L2 Binance: implement bridging acceptance on `(U, u, pu)` with `last_applied`; no lookahead buffer initially.
  - Add per-connector metrics and simple health management.
  - Extend subscription parser to support `[N]` suffix and propagate `max_connections` per subscription.
  - Implement DNS A/AAAA resolution and sequential connection ramp-up (5s spacing) up to `max_connections` or address list size, whichever is smaller.

- Phase 2: OKX & Deribit L2
  - OKX: enforce `prevSeqId == last_seq_id` acceptance.
  - Deribit: strictly increasing `change_id` acceptance.
  - Introduce optional small lookahead buffer (`--arb-lookahead`) for ahead segments.

- Phase 3: endpoint diversity and variants
  - Add exchange-specific variant builders:
    - Binance: `@depth@0ms` and `@depth@100ms`.
    - OKX: `books5` vs higher-fidelity channels if available to you; prefer the lowest-latency allowed by account tier.
  - Add alternative hostnames where supported.

- Phase 4: DNS multi-IP fanout (optional)
  - Resolve A/AAAA records; dial multiple IPs with SNI; measure impact; keep configurable.

- Phase 5: polish and docs
  - Extend README/DEPLOYMENT with new flags and guidance.
  - Add integration tests with simulated connectors producing controlled sequences and delays.

- Phase 6: dynamic rotation under rate limits
  - Implement rotation engine with sliding-window metrics, scoring, hysteresis, cooldown, and per-exchange budgets.
  - Wire rotation into `LatencyArbGroup` and expose rotation metrics/events.
  - Validate improved first-arrival latency vs. static N connections over multi-minute horizons.

### Touch points in codebase
- `src/cli.rs`: new flags and validation.
- `src/subscription_manager.rs`: instantiate `LatencyArbGroup` when arb is enabled.
- `src/pipeline/unified_handler.rs`: add a path that consumes accepted `RawMessage`s from `LatencyArbGroup` and calls `processor.process_unified_message` to preserve current metrics.
- `src/exchanges/{binance,okx,deribit}.rs`: implement lightweight `ArbKeyExtractor` helpers.
- `src/types.rs`: add `ArbMetrics` and small helper structs for keys if needed.

### Testing strategy
- Unit tests for acceptance logic per exchange (synthetic sequences from two connectors racing; verify only valid next segments are accepted).
- Property tests: random interleaving with duplicates/ahead messages; assert continuity and no duplicates in output.
- Integration tests: mock connectors with delays; measure win-rate and end-to-end latency reduction vs single-connector baseline.

### Risks and mitigations
- Mixing L2 streams incorrectly can break book continuity. Mitigation: strict exchange-specific acceptance rules; optional lookahead buffer; immediate resync on gap.
- Increased resource usage (sockets/CPU). Mitigation: configurable `--arb-connections`, bounded queues, prune idle connections.
- TLS to IP dialing complexity. Mitigation: make DNS fanout optional and later-phase; start with multi-connection to hostnames.

### Rollout plan
- Ship `--arb-mode simple` with TRADES enabled by default and L2 opt-in per exchange.
- Observe metrics in staging; validate no duplicate rows in Parquet/QuestDB and continuity counters are stable.
- Gradually enable L2 per exchange and increase `--arb-connections` in production.

### Expected impact
- TRADES: near-linear improvement in first-seen latency with 2–3 connections in adverse network conditions.
- L2: measurable reduction in time-to-next-commit while maintaining strict continuity, especially when one path is intermittently slower.
