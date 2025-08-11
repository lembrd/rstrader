## XTrader Architecture, Design, and Performance Review

This document reviews the current codebase with a focus on low-latency performance, correctness under load, and architectural clarity. It highlights potential problems, anti-patterns, and opportunities for refactoring. Recommendations are ordered by impact and proximity to the hot path.

### Executive summary

- Overall, the system has a solid shape: strong modular boundaries; unified handlers remove a major context-switch hotspot; sinks batch and isolate I/O; metrics are pervasive.
- The main remaining latency risks are JSON parsing overhead, string churn and clones, use of wall-clock for code-latency, and backpressure policies that can stall the pipeline.
- Medium-term improvements include specialized parsers, pre-encoding sink tags, replacing dynamic dispatch on hot paths, and a stricter logging and metrics regime for hot loops.

### Top risks and quick wins

1) JSON parse overhead on hot paths
- Observed
  - `serde_json` is used widely in processors (e.g., Binance/OKX), sometimes parsing into `Value` then re-parsing into typed structs.
  - Raw payloads are held in `String`, requiring copies and conversions.
- Impact
  - Adds parse allocations and CPU in tight loops; generates tail latency under bursty traffic.
- Actions
  - Replace `serde_json` with `simd-json` and borrowed deserialization where possible (crate already present in Cargo.toml). Parse directly from `&mut [u8]` without intermediate `Value` when feasible.
  - In Binance/OKX processors, avoid the double-step `from_slice(Value)` → `from_value(T)`. Instead, dispatch by minimal field probes or use untagged enums/adjacently-tagged enums for subscription vs data.
  - Keep `RawMessage.data` as bytes (`Bytes` or `Vec<u8>`) and parse from slice; convert to string only for logging.

2) Timekeeping for code latency uses wall-clock
- Observed
  - `now_micros()` uses `SystemTime` in hot code; code-latency is computed relative to `SystemTime` captured on receipt.
- Impact
  - `SystemTime` can jump (NTP), is slower, and mixes wall and monotonic times, skewing code-latency and jitter analysis.
- Actions
  - Use `std::time::Instant` for code latency on all code-measured intervals. Keep exchange timestamp vs local wall-clock for “overall latency” separately.
  - Store both fields in metrics; convert to micros only at sink/publish boundaries.

3) String churn and cloning on the hot path
- Observed
  - Per-message `to_string()` on enums and symbols, format-based composite keys in sinks, repeated symbol conversions for OKX.
- Impact
  - Allocations and hashing in tight loops; avoidable GC pressure.
- Actions
  - Precompute per-writer ILP/Parquet tag prefixes once; keep as `Vec<u8>` and append fields only. Maintain an arena or pre-sized buffers for line assembly.
  - Replace `format!("{}_{}_{}", ...)` keys with a small struct or interned key; store a compact integer key mapping.
  - Cache `ExchangeId` → &'static str mapping; use small enums with `as_str()` returning `&'static str`.
  - In `OkxProcessor`, retain and reuse converted symbol and `InstrumentMetadata` (already partly done). Ensure map lookups are hoisted and cached per message batch.

4) Backpressure policies and channel choices
- Observed
  - Single `mpsc::<StreamData>(10000)` to sink; unified handlers call `send().await`, which can stall producers when the sink is slow (especially Parquet with 5s flush).
- Impact
  - Head-of-line blocking; rising overall latency during I/O spikes.
- Actions
  - Make capacity configurable and allow “lossy” mode with `try_send()` in handlers/arbitration to drop late updates (documented in `docs/ARCH_PERF_REVIEW.md` as an option).
  - Consider multiple sink workers or per-stream bounded queues; or a bounded SPSC lock-free structure for ultra-low latency (crossbeam bounded).
  - For Parquet, support a low-latency mode: lower `FLUSH_INTERVAL_MS`, smaller batches, or double-buffering with a background writer pool.

5) Dynamic dispatch in hot paths
- Observed
  - Widely using `Box<dyn ExchangeProcessor>` and `Box<dyn ExchangeConnector>`.
- Impact
  - Virtual dispatch on the hot path inhibits inlining and prevents zero-cost generics.
- Actions
  - For the critical inner loop (processor), consider an enum over known exchanges plus `match` dispatch, or genericized pipelines parameterized by processor type. Keep trait objects only at the orchestration boundaries.

6) Logging in hot loops
- Observed
  - Info-level logs in some loops (connection, subscription, periodic metrics). Debug logs guarded but still appear in some paths.
- Impact
  - Logging I/O can distort tail latency; even formatting allocations matter.
- Actions
  - Guard all hot-path logs behind `log::log_enabled!()` and keep at `debug!` or lower. Strip logs completely in hot paths once stabilized.

7) Monolithic metrics snapshot lock
- Observed
  - `GLOBAL_METRICS` is `Arc<Mutex<Metrics>>`. Exporters lock on snapshot.
- Impact
  - Low contention today, but can cause stalls with many per-stream updates.
- Actions
  - Replace snapshot fields with atomics for counters and `AtomicU64` for last/avg as needed; or use sharded counters and aggregate in exporter.
  - Keep Prometheus registry updates batched to reduce per-tick work.

8) Parquet sink latency defaults
- Observed
  - Parquet uses `FLUSH_INTERVAL_MS = 5000` and batch size 1000.
- Impact
  - Large flushing windows add end-to-end latency for downstream visibility, fine for archival but not for real-time consumers.
- Actions
  - Document two modes: archival (high compression, big batches) vs realtime (small batches, frequent flush with writer pool). Consider LZ4/ZSTD trade-offs.

---

### Detailed findings by subsystem

#### Runtime and orchestration (`src/main.rs`, `subscription_manager.rs`, `pipeline/unified_handler.rs`)
- Good
  - Unified handler eliminates connector→processor channel and extra task; fewer context switches and reduced scheduling jitter.
  - Clean shutdown flows; default server mode runs until Ctrl-C or `--shutdown-after` is set (matches product requirement to run as a server by default).
  - OKX registries initialized once and shared; avoids redundant warmups.
- Issues / Refactors
  - Make the `mpsc` capacity and sink choice tunable via CLI/env; add a “lossy mode” (drop on backpressure) switch.
  - Consider a custom Tokio runtime with tuned worker count, `IO` driver, and cooperative budget for ingestion tasks.
  - For arbitrated streams (multi-connector), provide an option to assign connectors to dedicated runtime workers or OS cores when extreme latency is needed.

#### Connectors (Binance, OKX, Deribit)
- Good
  - Binance: fast-path scan of U/u/pu in the connector stage to reduce parse; bounded `VecDeque` to cap memory during snapshot sync.
  - OKX: registry caching + unit conversion done once; symbol conversion helpers are correct and explicit.
  - Deribit: fast control-plane routing; preallocated message buffer.
- Issues / Refactors
  - Deribit `pending_rpc_calls: Arc<Mutex<_>>` can block on mutex; replace with `dashmap` or a small lock-free slotmap per outstanding id. Given small RPC volume, this is medium priority.
  - In Binance/OKX `next_message()`, timestamp is `SystemTime::now()`; capture a parallel `Instant::now()` for code-latency and store both.
  - For snapshot reconciliation, unify policy and instrumentation across exchanges: report time-to-sync, number of buffered updates applied, and gaps.
  - Replace repeated `connect_async` string building/lookup logs with guarded debug-only emissions.

#### Processors (`src/exchanges/*::process_message`, `processor.rs`)
- Good
  - Clear separation; rich metrics: parse, transform, overhead, code/overall latency, packet aggregation.
  - Pre-allocated buffers and `std::mem::take` to avoid clones.
- Issues / Refactors
  - JSON parse overhead: use `simd-json` with hand-rolled partial parsing for the minimal fields; remove intermediate `Value` uses.
  - For Binance depth, parse directly into typed struct from `&mut [u8]` and short-circuit subscription acks via tiny `event` probe.
  - Avoid recomputing symbol conversions and metadata per entry; hoist once per packet.
  - Consider representing side/action as compact integers in hot-path structs, converting to strings only at sink boundary.

#### Pipeline arbitration (`pipeline/latency_arb.rs`)
- Good
  - First-arrival arbitration is pragmatic and easy to reason about; TTL+LRU dedupers are lightweight.
  - Connector rotation and per-connector win accounting help keep best sources alive.
- Issues / Refactors
  - Lots of `tokio::spawn` handles; ensure cancellation token propagation and handle abort reasons uniformly.
  - For multi-connector groups, optionally place each connector on a separate worker to minimize inter-task contention.
  - Provide a fast-path that uses `try_send` to a bounded downstream channel, with drop counters surfaced as metrics.

#### Orderbook manager (`pipeline/orderbook.rs`)
- Good
  - Plain `BTreeMap` for price ordering is simple and sufficient.
- Issues / Refactors
  - `OrderedFloat` uses `partial_cmp(...).unwrap_or(Equal)` and allows NaN to compare equal; since inputs are validated, this is acceptable but document assumptions.
  - If throughput becomes a bottleneck, consider specialized radix/pool allocators for price levels, or arrays-of-structs for CPU cache locality (later optimization).

#### Sinks (Parquet, QuestDB)
- Good
  - `spawn_blocking` isolates CPU-heavy writes; per-stream writers avoid cross-stream contention.
  - QuestDB ILP builder uses `ryu` for floats and minimal string escaping.
- Issues / Refactors
  - Pre-encode measurement+tags once per writer (e.g., `b"l2_updates,exchange=...,ticker=..."`) to remove repeated `to_string()`/`format!()`.
  - Replace the composite string key with a small struct and a `FxHashMap` keyed by an interned id.
  - For Parquet: consider a writer pool with double-buffering so `flush` swaps buffers and lets the pool write out-of-band, unblocking producers faster.
  - Make `FLUSH_INTERVAL_MS` configurable; for real-time, default much lower or wire to CLI `--latency-mode`. For QuestDB, `500ms` is good; allow 50–100ms for ultra-low-latency.

#### Metrics and exporters (`metrics/*`)
- Good
  - Clear snapshot aggregator; Prometheus and StatsD support; optional HDR histograms with feature gate.
- Issues / Refactors
  - Consider atomics for snapshot fields to avoid the global `Mutex` on tick; or keep local per-stream counters and aggregate in a single-threaded exporter.
  - Reduce Prometheus label cardinality where it may explode (symbols). Consider quantile updates on a sampled cadence or only when changed beyond thresholds.
  - Ensure the HTTP `/metrics` mini-server reads the full request; current 1024B read is sufficient for simple GET but brittle. Consider using `hyper` you already depend on, or increase buffer and parse minimal lines robustly.

#### Error handling and logging
- Good
  - `AppError` centralizes error creation; exchange-specific errors map well to AppError categories.
- Issues / Refactors
  - Remove or gate panics/unwraps that can occur outside test code (e.g., `panic!` in output schema getters, `expect` in histograms). Convert to `Result` and handle centrally.
  - Normalize error severities (connection vs data vs pipeline) and add backoff policies for repeated transient failures (already present in QuestDB ILP client).
  - Make high-volume `info!` logs in sinks and processors conditional on verbose mode.

---

### Concrete refactoring plan

Phase 1 (hot-path, high impact)
- Parsing
  - Introduce a `parsing` module with `simd-json` helpers: minimal probes (event/result fields) and direct typed deserialization from `&mut [u8]` for Binance/OKX.
  - Change `RawMessage.data` to `bytes::Bytes` (or `Vec<u8>`) and parse without `String` conversion.
- Timekeeping
  - Add `RawMessage.code_arrival: Instant` and switch code-latency to `Instant` deltas everywhere; keep wall-clock for overall-latency only.
  - Extend `Metrics` to hold both monotonic and wall based stats distinctly (preserving current fields for compatibility).
- Sinks
  - Pre-encode ILP/Parquet tag prefixes per writer and reuse a `Vec<u8>` buffer; switch repeated `to_string()` calls to `as_str()`.
  - Add CLI/env controls: `XTRADER_SINK_FLUSH_MS`, `XTRADER_SINK_BATCH`, `XTRADER_SINK_LOSSY`.

Phase 2 (architecture and throughput)
- Dispatch
  - Replace trait-object processors in unified handlers with an `enum Processor { Binance(BinanceProcessor), Okx(OkxProcessor), Deribit(DeribitProcessor) }` and `match` dispatch in hot loops.
  - Retain trait objects at subscription orchestration only.
- Backpressure
  - Pluggable channel policy for handler→sink: `blocking` (current), `lossy` (try_send/drop), `multi` (per-stream queues + sink worker pool).
- Metrics
  - Migrate `GLOBAL_METRICS` to atomics where feasible; shard per-stream counters, aggregate on exporter tick.
  - Add drop counters for lossy mode and backpressure metrics (queue depth high-water mark).

Phase 3 (polish and ops)
- Logging policy: provide `--verbose` and `--trace-io` flags; default to minimal logs in hot paths.
- Harden the mini `/metrics` server or replace with `hyper` minimal server, given the dependency already exists.
- Separate the project into crates: `xtrader-core` (types, metrics), `xtrader-exchanges`, `xtrader-sinks`, `xtrader-app`. This reduces compile times and clarifies boundaries.

---

### Specific code-level notes

- `src/exchanges/binance.rs`
  - Avoid parsing into `serde_json::Value` before `from_value` for depth messages; directly parse into `BinanceDepthUpdate` after a minimal `event` probe.
  - Keep a per-message `ok_symbol` and hoist conversions out of loops; reuse `timestamp_micros` for all entries.

- `src/exchanges/okx.rs`
  - Ensure per-packet metadata lookup is performed once; rely on the registry cache and `metadata_cache` only for misses.
  - Convert `convert_symbol_*` helpers to return `&'static str` when mapping fixed exchange identifiers; for dynamic symbols, add a small symbol interner.

- `src/exchanges/deribit.rs`
  - `pending_rpc_calls` on `Arc<Mutex<...>>`: low volume, but consider `dashmap` to avoid lock convoy under bursty responses.
  - `process_market_data_fast` returns `Some(RawMessage)` only when channel is known; track channel set using a lock-free set or a `hashbrown::HashSet` protected by a very short-lived lock.

- `src/output/questdb.rs` and `src/output/multi_stream_sink.rs`
  - Introduce a `PreEncodedTags` struct with a prebuilt prefix (`measurement,tags `) and re-use per write; reduce `to_string()` conversions.
  - Add a background writer pool for Parquet; use a bounded `mpsc` to pass completed batches.

- `src/metrics/*`
  - Convert `GLOBAL_METRICS` to atomics; leave HDR histograms behind a feature gate. Consider sampling to reduce exporter overhead.

---

### Testing, reliability, and observability

- Add golden tests for parser fast-paths (simd-json) with a corpus of real payloads.
- Add integration tests for backpressure policies (blocking vs lossy) verifying end-to-end latency under sink stalls.
- Record and export queue depths (mpsc lag) and flush durations for both sinks; track p50/p99.
- Validate OKX registry coverage at startup: quantify % of requested symbols known.

---

### Appendix: Recommended defaults for low-latency mode

- `--sink questdb`, `XTRADER_SINK_FLUSH_MS=100`, `XTRADER_SINK_BATCH=200`
- `--lossy-mode` for unified/arbitrated handlers to prefer freshness over completeness
- `TOKIO_WORKER_THREADS=core_count` and pin connectors to separate workers when high fanout
- Guard all hot-path logs; enable `metrics-hdr` only when profiling, not in production by default

---

If desired, I can implement Phase 1 changes (parsing/timekeeping/sink pre-encoding + CLI) as a single PR to make measurable latency improvements end-to-end.


