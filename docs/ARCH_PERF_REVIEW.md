## XTrader architecture and performance review

Focus: ultra-low latency data ingestion, robustness, and maintainability.

### Executive summary

- Overall architecture is solid: unified handler per subscription, explicit sinks, and exchange-specific processors with shared metrics.
- Several hot-path inefficiencies and correctness risks exist in arbitration and connectors. Fixing them will reduce end-to-end latency jitter and improve stability under load.
- Key themes: avoid double parsing, fix registry misuse, bound memory in arbitration, reduce logging on hot paths, and tighten schema/symbol consistency.

---

### Critical issues to fix first

- Duplicate channel declarations in `latency_arb.rs` (bug, potential logic confusion)
  - File: `src/pipeline/latency_arb.rs`
  - In `run_arbitrated_trades`, `ip_tx/ip_rx` are declared three times and never used. Shadowing creates dead code and confusion.
  - Impact: unnecessary allocations and maintenance hazard.
  - Action: remove the duplicated channels entirely (or wire them properly if needed for telemetry).

- OKX registries ignored in L2 arbitration (functional bug)
  - File: `src/pipeline/latency_arb.rs`
  - In `run_arbitrated_l2`, connectors are constructed with `Arc::new(InstrumentRegistry::new())` instead of using the passed-in `okx_*_registry`. Replacement connectors during rotation repeat the mistake.
  - Impact: unit conversion (contract multiplier) may be wrong; symbol validation may fail; inconsistent behavior vs unified handlers and vs trades arb.
  - Action: thread through and use the provided `okx_swap_registry`/`okx_spot_registry` for both initial spawn and rotation.

- Unbounded dedup memory in arbitration (risk: memory growth, latency spikes)
  - File: `src/pipeline/latency_arb.rs`
    - Trades: `seen_trades: HashSet<(ExchangeId, String, String)>`
    - L2: `seen_events: HashSet<(i64, i64)>`
  - Impact: runs indefinitely, sets grow without bound; increases hash table rehashing, cache misses, and memory pressure.
  - Action: replace with time/size-bounded dedup (e.g., `lru::LruCache` keyed by event key/trade_id with TTL) or a ring buffer plus bloom filter approach. Keep per-symbol caps (e.g., 100k–1M depending on traffic) and periodic trimming.

---

### High-impact performance opportunities

- Avoid double JSON parsing (connector + processor)
  - Files: `src/exchanges/binance.rs`, `src/exchanges/okx.rs`
  - Today, connectors parse messages to manage sync (e.g., Binance sequence) and then processors parse again to transform to unified structs.
  - Options:
    - Move minimal sync extraction to string-scan or SIMD-JSON partial parsing for just fields needed (`U/u/pu`, arrays presence) and pass raw text to processor once.
    - Or move parsing to connectors and pass already-typed domain structs to processors (trait over typed messages), eliminating second parse.
  - Expected benefit: 10–30% lower CPU in hot path, lower GC pressure, tighter tail latency.

- Reduce hot-path logging to debug-only and coalesce
  - Files: arbitration, processors, connectors.
  - Replace frequent `info!` with `debug!` (guarded by `log_enabled!`) and periodic summaries. Metrics reporting already exists; keep reporting but lower verbosity.
  - Expected benefit: smoother latency and less contention on the global logger.

- Use faster hash maps in hot paths
  - Files:
    - `src/output/multi_stream_sink.rs` writers HashMap
    - Arbitration accounting maps (wins/msgs)
  - Switch to `rustc_hash::FxHashMap` or `ahash::AHashMap` for faster hashing where DOS is not a concern.

- Parquet I/O offload strategy
  - File: `src/output/multi_stream_sink.rs`
  - You use `tokio::task::block_in_place` for file creation and writes. Prefer `tokio::task::spawn_blocking` with a dedicated CPU-bound thread pool or a small per-writer synchronous worker. This avoids stealing core runtime threads under load.

- Avoid extra string formatting for writer keys
  - File: `src/output/multi_stream_sink.rs`
  - Key currently: `format!("{}_{}_{}_{}", stream_type, exchange, symbol, "")` adds a trailing underscore and an empty segment.
  - Use a struct key `(StreamType, ExchangeId, Symbol)` with derived `Hash`/`Eq`, or a clean string without the extra segment.

- Reuse Arrow schemas and prebuilt column buffers
  - File: `src/output/multi_stream_sink.rs`
  - Cache `Arc<Schema>` per stream type once per `StreamWriter`. Consider preallocating arrays and using builders to reduce per-batch allocations.

- ILP serialization correctness and speed
  - File: `src/output/questdb.rs`
  - Ensure ILP escaping for strings includes commas and spaces if they ever appear (currently only quotes are escaped). Add unit tests for edge cases.
  - Consider buffering per-stream and writing via a single background TCP writer to reduce syscalls.

---

### Consistency and correctness

- Symbol format consistency for OKX trades
  - File: `src/exchanges/okx.rs`
  - In `OkxProcessor::process_trade_message`, `ticker` is set to `inst_id` (OKX format like `BTC-USDT[-SWAP]`). Elsewhere we normalize to unified `BTCUSDT` (e.g., L2 processing and Deribit conversion). Inconsistency complicates sinks/analytics.
  - Action: convert to unified symbol via existing helpers (e.g., `convert_symbol_from_okx`).

- Panic on unsupported schema type
  - File: `src/output/schema.rs`
  - `get_schema_for_stream_type` panics on unknown types. Prefer returning `Result<Arc<Schema>>` and handle errors at call sites.

- Metrics counter reuse for batches written
  - File: `src/types.rs` (`Metrics::increment_batches_written` increments `messages_processed`).
  - Action: add explicit batch counters to avoid conflating concepts.

---

### Arbitration design notes and refactors

- Trades arbitration (`run_arbitrated_trades`)
  - Pros: simple “first arrival wins” dedup on `(exchange, symbol, trade_id)`; shared processor instance; periodic metrics.
  - Issues:
    - Unbounded `seen_trades` set. Add TTL/size limit.
    - No IP telemetry hook actually used; the `ip_*` channels are unused. Either wire `peer_addr()` reporting as done in L2 or remove.
    - Ramp-up delay hard-coded to 1s. Consider a configuration-driven ramp and faster ramp for recovery.

- L2 arbitration (`run_arbitrated_l2`)
  - Pros: first-arrival dedupe using `(first_update_id, update_id)`, windowed rotation of losing connectors, snapshot reconciliation for Binance.
  - Issues:
    - Registry misuse for OKX (see Critical issues).
    - Also unbounded `seen_events`. Bound it.
    - Rotation window resets all counters; consider exponential decay instead to avoid synchronized dips.
    - Consider checksum verification when available (OKX books-l2-tbt) and continuity gates per exchange.

Suggested refactor outline for arbitration modules:

- Introduce a small `Deduper<K>` utility with LRU + TTL semantics and metrics. Use it for both L2 and trades.
- Abstract connector creation into a `ConnectorFactory` that accepts optional registries and the exchange to ensure consistent usage across spawn and rotation.
- Parameterize ramp-up delay, rotation interval, and max connections via `SubscriptionSpec` (already includes `max_connections`) and/or CLI.
- Add optional “drop policy” on output send (e.g., `try_send` with drop-on-full for lowest-latency deployments), configurable per sink.

---

### Connectors and processors

- Binance connector
  - Sync state machine inside `next_message` does partial JSON parses; that’s fine, but still materializes `BinanceDepthUpdate` for state checks sometimes and then the processor parses again.
  - Refactor idea: Extract only the fields needed for sync (`U/u/pu`) via a fast path (string scan or SIMD) and skip full struct conversion on connector side.

- OKX connector/processor
  - Good use of instrument registry for unit conversion. Ensure registry is always available in any path that touches normalization (arb, unified handler, tests).
  - For trades, ensure quantity normalization applies consistently as it does for L2.

- Deribit connector
  - Efficient fast-path routing for subscription messages; good.
  - Consider moving to typed fast path for trade arrays to avoid `serde_json::Value` on the hot path.

---

### Output subsystem

- Parquet sink
  - Good periodic flush and idle prune. Improve:
    - Use `spawn_blocking` for Arrow/Parquet writes and file creation.
    - Replace writer-key `String` with a small struct key or a clean formatted string (no trailing underscore).
    - Ensure `close()` flushes remaining buffers using the actual `FileManager` rather than warning. Provide `close_with(file_manager)` or restructure so the manager is owned at the sink level and writers can request flush/close via a method.

- QuestDB ILP sink
  - Improve escaping and add tests for tickers with special chars (commas/spaces unlikely but test anyway).
  - Consider a background writer task with a channel to aggregate and write batches across streams for fewer syscalls.

---

### Concurrency and backpressure

- Channels
  - `mpsc::channel::<StreamData>(10000)` in `main.rs`: Consider making capacity configurable. For ultra-low latency, allow a “lossy” mode using `try_send` in handlers/arbitration to drop late updates rather than backpressure the pipeline.
  - Sinks: consider `try_send` or bounded internal buffers with shedding.

- Task pinning / CPU affinity (advanced)
  - Consider pinning critical tasks per core (via `tokio` worker assignment or `taskset`/cgroups) for consistent tail latency in production.

---

### Logging and observability

- Reduce `info!` in hot loops and promote periodic metrics via `MetricsReporter`.
- Add a per-subscription “dropped due to backpressure” counter if lossy mode is enabled.
- Emit a one-line high-signal summary on shutdown per handler, including min/avg/max parse/transform/code latencies and throughput.

---

### Concrete refactoring checklist

1) Fix critical bugs and inconsistencies
   - Remove duplicated `ip_tx/ip_rx` in `run_arbitrated_trades`.
   - Use provided OKX registries in `run_arbitrated_l2` (initial and rotation spawns).
   - Normalize OKX trade tickers to unified format in `OkxProcessor::process_trade_message`.
   - Replace `panic!` in `get_schema_for_stream_type` with a `Result` API.

2) Bound arbitration memory
   - Introduce `Deduper` with size and TTL (e.g., `lru::LruCache` or `hashlink`). Replace `HashSet` in both arb flows.

3) Reduce parsing overhead
   - Connector side: extract minimal sync fields via fast path and avoid full struct parse; processor does the full parse only once.
   - Optional: feature-gated `simd-json` in release builds.

4) Parquet sink improvements
   - Switch to `spawn_blocking` for writer init and `write()` calls.
   - Clean writer key; cache schemas per writer.
   - Make `close()` flush remaining buffers properly (own the manager in sink and plumb it).

5) Optional low-latency modes
   - Configurable channel sizes and drop policies.
   - Fewer logs, higher aggregation interval for metrics.

---

### Suggested small diffs (sketches)

Remove duplicate ip channels in trades arb:

```startLine:86:endLine:95:src/pipeline/latency_arb.rs
    // Channels from connector readers to arb
    let (raw_tx, mut raw_rx) = mpsc::channel::<(usize, RawMessage)>(10000);
    // Remove unused ip channels here or implement telemetry if needed
```

Use provided OKX registries in L2 arb spawn and rotation:

```startLine:282:endLine:301:src/pipeline/latency_arb.rs
            let mut connector: Box<dyn ExchangeConnector<Error = AppError>> = match exchange {
                Exchange::BinanceFutures => Box::new(crate::exchanges::binance::BinanceFuturesConnector::new()),
                Exchange::OkxSwap => {
                    let reg = okx_swap_registry.clone().expect("OKX SWAP registry not initialized");
                    Box::new(crate::exchanges::okx::OkxConnector::new_swap(reg))
                }
                Exchange::OkxSpot => {
                    let reg = okx_spot_registry.clone().expect("OKX SPOT registry not initialized");
                    Box::new(crate::exchanges::okx::OkxConnector::new_spot(reg))
                }
                Exchange::Deribit => { /* unchanged */ }
            };
```

Normalize OKX trade tickers in processor:

```startLine:1486:endLine:1500:src/exchanges/okx.rs
            let ticker = self.convert_symbol_from_okx(&trade_data.inst_id);
            let trade = crate::types::TradeUpdate {
                /* ... */
                ticker,
                /* ... */
            };
```

---

### Longer-term ideas

- Zero-copy parsing with pre-allocated arenas or custom deserializers for the dominant message shapes (Binance depth, OKX depth/trades).
- Integrate kernel timestamping (SO_TIMESTAMPING) and NIC hardware timestamps (if applicable) for more accurate code latency metrics.
- Consider a “multi-sink dispatcher” with fan-out and optional per-stream backpressure controls (Parquet + QuestDB simultaneously).

---

### Conclusion

The codebase is close to production-ready for high-throughput collection. Addressing the highlighted bugs and applying the targeted refactors will yield measurable reductions in CPU time and tail latency while improving correctness (OKX normalization) and memory safety (bounded dedup sets). The changes are localized and can be rolled out incrementally with unit tests for each fix.


