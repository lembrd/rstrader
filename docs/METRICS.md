## Metrics strategy for low‑latency HFT (Rust)

This document reviews Rust metrics libraries that could replace the current manual metrics in `src/types.rs::Metrics`, evaluates their overhead and operational trade‑offs for a low‑latency/high‑throughput (HFT) system, and proposes a pragmatic adoption plan.

### Current state (summary)

- Manual counters and rolling averages in `Metrics` with periodic log reporting via `MetricsReporter`.
- Advantages: minimal dependencies, predictable overhead, no background I/O on critical path.
- Limitations: no standard scraping/export, no histograms per label, difficult external observability and alerting.

### HFT‑specific requirements

- Extremely low per‑event overhead on the hot path (< ~50–100 ns budget per update, ideally amortized off the hot path).
- Non‑blocking and allocation‑free on hot path; no syscalls or locks in hot path.
- Metrics emission must be asynchronous and back‑pressure aware (lossy is acceptable for counters; for latency histograms, bounded loss or local buffering).
- Ability to disable or degrade gracefully via feature flags without code churn.
- Support for common backends (Prometheus/StatsD/OTel) for ops alignment, but only if costs stay off the hot path.

### Library options (Rust)

- metrics (with exporters)
  - What it is: A façade crate (`metrics`) with a global recorder and exporters (`metrics-exporter-prometheus`, `metrics-exporter-statsd`, etc.). Macros: `counter!`, `gauge!`, `histogram!`.
  - Pros: Mature ecosystem, pluggable backends, simple callsites, batching/buffering via exporters; can be compiled out or set to a no‑op recorder.
  - Cons: Labels and dynamic key construction add noticeable overhead; global recorder adds indirection. Careful configuration needed to ensure lock‑free non‑blocking hot path. Histogram recording still costs; use thread‑local buffering if needed.
  - Fit: Good, if we constrain usage patterns (const keys, minimal labels, thread‑local buffering, background exporter).

- prometheus (client library)
  - What it is: Native Prometheus client (`prometheus` crate) with registries, counters/gauges/histograms and HTTP exposition.
  - Pros: Direct, widely used, no façade layer.
  - Cons: Synchronous data structures; histograms often lock or contend; exposition requires an HTTP endpoint. Not ideal for hot path instrumentation.
  - Fit: Suitable for background aggregation/readout of metrics we update atomically; avoid direct per‑event histogram updates on hot path.

- OpenTelemetry (otel)
  - What it is: Standardized telemetry with metrics/traces/logs and many exporters.
  - Pros: Interop with enterprise tooling; unified traces/metrics if needed.
  - Cons: Heavier dependency tree; more allocation/indirection; typical implementations add higher overhead than HFT tolerances.
  - Fit: Not recommended on hot path. Consider only for control‑plane services or off‑critical components.

- hdrhistogram
  - What it is: High dynamic‑range histograms for latency/size distributions.
  - Pros: Excellent for latency distributions; per‑thread histograms can be merged periodically with very low hot‑path contention.
  - Cons: Recording still costs; avoid sharing across threads; choose bounds carefully to keep memory small.
  - Fit: Strong for latency measurement if done per‑thread and flushed asynchronously.

- cadence (StatsD client)
  - What it is: UDP/buffered StatsD client.
  - Pros: Very low overhead if batched and flushed on a background thread; off‑box aggregation (Datadog/StatsD/Telegraf).
  - Cons: UDP loss; additional infra; labels are limited vs Prometheus.
  - Fit: Good for counters/rates where approximate is fine; keep hot path to enqueue only.

- hotmic / dipstick / metriken (other metrics crates)
  - What they are: Alternative metrics libraries focused on low overhead with pluggable sinks.
  - Pros: Some emphasize lock‑free ring buffers and HdrHistogram aggregation.
  - Cons: Smaller ecosystems, fewer maintained exporters and integrations than `metrics`.
  - Fit: Viable if we want a very small surface; ecosystem maturity varies.

- tokio-metrics (runtime instrumentation)
  - Observes runtime/task metrics, not business metrics.
  - Fit: Useful for diagnosing async scheduling; not for hot path per‑event trading metrics.

### Decision matrix (qualitative)

- Hot‑path counters: `metrics` with a tuned recorder or manual `Atomic*` with periodic snapshot → Prefer manual atomics or `metrics` with const keys + thread‑local buffering.
- Latency distributions: Per‑thread `hdrhistogram` instances merged on interval → Avoid shared histograms on hot path.
- Export/scrape protocol: Prometheus pull or StatsD UDP push on a background thread → Avoid exporting from hot path.
- Operational standardization: If ops requires Prometheus, expose an HTTP handler that only reads pre‑aggregated state.

### Recommended approach

1) Keep manual atomics for the absolute hot path where updates are on the per‑message critical path.
   - They are the lowest and most predictable overhead.
   - Organize counters as `AtomicU64`/`AtomicUsize`, optionally thread‑local for sharding; aggregate in a background task.

2) Add `hdrhistogram` per thread for latency metrics, flush/merge every N ms on a background worker.
   - Record with pre‑validated bounds; avoid allocation.

3) Introduce a thin, optional façade to export aggregated values through one of:
   - Prometheus: Use `prometheus` or `metrics-exporter-prometheus`, but only to expose snapshots from our aggregator, not to update per‑event.
   - StatsD: Use `cadence` with buffered UDP for counters/rates; background flushing.
   - Feature‑gate exporters so prod trading can disable them entirely or run them on a separate process/host.

4) For non‑critical components (ingestion coordination, control plane, sinks): it’s acceptable to use `metrics` macros directly with a tuned recorder.

### Do we need to replace our metrics with an external library?

- Short answer: No for the hot path; Yes for standardized export/scrape.
  - Retain manual atomics and per‑thread histograms on the hot path.
  - Add a background aggregation/export layer to integrate with Prometheus/StatsD without touching the hot path.

### Implementation sketch

- Data plane (hot path)
  - Per thread:
    - `Atomic*` counters (or thread‑local counters merged periodically).
    - `hdrhistogram::Histogram<u64>` for latency buckets.
  - Update operations are branch‑free, lock‑free, and allocation‑free.

- Control/aggregation plane (background)
  - Every 200–1000 ms:
    - Snapshot atomics and reset deltas.
    - Drain/merge thread‑local histograms.
    - Compute derived rates/averages.
    - Expose snapshots:
      - Prometheus: update `prometheus` registry or a `metrics` recorder from aggregated values.
      - StatsD: `cadence` send increments/gauges in batches.
  - Protect against exporter stalls (timeouts, bounded queues, drop oldest).

- Configuration
  - Feature flags: `metrics-prom`, `metrics-statsd`, `metrics-none`.
  - Runtime toggles for exporter endpoints and flush intervals.

### Migration path from current code

1) Isolate current `Metrics` into a read‑only snapshot struct produced by an internal aggregator.
2) Replace in‑place rolling averages with: per‑thread histograms + background computed percentiles/EMA.
3) Introduce an `Aggregator` service:
   - Owns shard‑local counters and histograms.
   - Exposes `record_*` functions that are inlined, lock‑free, non‑allocating.
   - Runs a background task to publish/export.
4) Add an exporter behind a feature flag:
   - Prometheus: implement `/metrics` HTTP handler that reads the latest snapshot.
   - StatsD: background flusher using `cadence`.
5) Keep `MetricsReporter` logging for human debugging, sourced from the aggregator snapshot.

### Benchmarking plan

- Microbench per‑event update cost:
  - Atomic counter inc
  - `hdrhistogram` record (per‑thread)
  - `metrics` macro with const key (recorder set to no‑op and to exporter)
- Saturation tests with representative message rates and label cardinality.
- End‑to‑end tests validating no tail‑latency regression when exporters are enabled.

### Minimal crate set to consider

- hdrhistogram: precise latency distributions with merge support.
- cadence (optional): low‑overhead StatsD client, buffered UDP.
- prometheus or metrics-exporter-prometheus (optional): scrape endpoint, used only for exporting pre‑aggregated snapshots.
- metrics (optional): façade for non‑critical components if we want uniform APIs.

### Summary

- Do not instrument the trading hot path with heavyweight external libraries.
- Keep hot path updates to manual atomics and per‑thread `hdrhistogram` recording.
- Add a background aggregator and optional exporters for operational visibility.
- Gate exporters with features and ensure they never introduce back‑pressure to the data plane.


