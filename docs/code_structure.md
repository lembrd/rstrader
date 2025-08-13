# Application code layout and Strategy API

This project is an algorithmic trading engine. It will handle the whole lifecycle of trading:
- Market data ingestion and processing
- Storage and analytics
- Strategy orchestration
- Order routing and position/account state (future)

We have implemented market data collection and persistence first. Now we will reorganize the code and module structure because the project is growing and becoming more complex. The goals are: clear separation of responsibilities, stable public APIs between modules, and a Strategy API to run multiple strategies using a shared runtime.

## Target folder structure

src/
    xcommons/                  - common base code: domain entities, enums, error, config, utils
        config.rs              - typed config loading (YAML/ENV), helpers
        error.rs               - error types, Result alias
        types.rs               - core domain types (exchanges, instruments, unified stream data)
        time.rs                - time helpers, monotonic clock abstractions
    exchanges/                 - exchange-specific code
        binance/
        deribit/
        okx/
        mod.rs
    md/                        - market data common processing (exchange-agnostic)
        orderbook.rs
        deduper.rs
        latency_arb.rs
        unified_handler.rs
        mod.rs
    output/                    - sinks and output formats
        parquet.rs
        questdb.rs
        multi_stream_sink.rs
        file_manager.rs
        schema.rs
        record_batch.rs
        mod.rs
    metrics/                   - exporters and global registry
        exporters.rs
        hdr.rs
        snapshot.rs
        mod.rs
    trading/                   - base trading abstractions (future)
        account_state/         - position tracking pipeline (see `docs/trade_account.md`)
    strats/                    - strategies using the Strategy API
        md_collector/          - current market data collector refactored as a strategy
            mod.rs
            config.rs
            strategy.rs
        mod.rs                 - strategy registry and wiring
    app/                       - application runtime and environment
        env.rs                 - `AppEnvironment` implementation
        runtime.rs             - strategy launcher and lifecycle
        cli.rs                 - CLI facade that dispatches to strategies
    main.rs                    - thin entrypoint delegating to `app::runtime`

Notes:
- Existing code in `src/pipeline`, `src/subscription_manager`, and `src/output` will be referenced and gradually moved into the new layout. Names above describe the destination; moves can be phased.
- Public APIs should live under `xcommons`, `md`, `output`, `metrics`, and `strats` with minimal cross-dependencies to keep compile times and coupling low.

## Strategy API (Rust)

The Strategy API enables running any strategy within a shared runtime. Strategies own their configuration and use the provided application environment to subscribe to data, spawn tasks, and emit outputs. The API is minimal to keep the hot path lean.

```rust
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::sync::Arc;

pub type AppResult<T> = Result<T, crate::error::AppError>;

#[async_trait]
pub trait Strategy: Send + Sync {
    type Config: DeserializeOwned + Send + Sync + 'static;

    fn name(&self) -> &'static str;

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()>;

    async fn stop(&self) -> AppResult<()>;
}

pub struct StrategyContext {
    pub env: Arc<dyn AppEnvironment>,
}

#[async_trait]
pub trait StrategyFactory: Send + Sync {
    fn name(&self) -> &'static str;
    fn create(&self) -> Box<dyn Strategy<Config = serde_yaml::Value>>;
}

pub struct StrategyRegistry {
    factories: Vec<Box<dyn StrategyFactory>>,
}

impl StrategyRegistry {
    pub fn new() -> Self { Self { factories: Vec::new() } }
    pub fn register(mut self, factory: Box<dyn StrategyFactory>) -> Self { self.factories.push(factory); self }
    pub fn get(&self, name: &str) -> Option<&Box<dyn StrategyFactory>> { self.factories.iter().find(|f| f.name() == name) }
}

#[async_trait]
pub trait AppEnvironment: Send + Sync {
    fn metrics(&self) -> Arc<dyn Metrics + Send + Sync>;
    fn clock(&self) -> Arc<dyn MonotonicClock + Send + Sync>;

    async fn start_subscriptions(
        &self,
        subs: Vec<SubscriptionDescriptor>,
        channel_capacity: usize,
    ) -> AppResult<tokio::sync::mpsc::Receiver<crate::types::StreamData>>;

    async fn start_sink(&self, sink: SinkConfig, rx: tokio::sync::mpsc::Receiver<crate::types::StreamData>) -> AppResult<tokio::task::JoinHandle<AppResult<()>>>;

    fn spawn(&self, fut: impl std::future::Future<Output = ()> + Send + 'static);
}

pub trait Metrics {
    fn counter_inc(&self, name: &str, value: u64);
    fn gauge_set(&self, name: &str, value: f64);
}

pub trait MonotonicClock { fn now(&self) -> std::time::Instant; }

#[derive(Clone, Debug)]
pub struct SubscriptionDescriptor {
    pub exchange: crate::types::Exchange,
    pub stream_type: crate::types::StreamType,
    pub instrument: String,
    // Optional: number of parallel feeds to use for latency arbitrage (default: 1)
    pub arb_streams_num: usize,
}

#[derive(Clone, Debug)]
pub enum SinkConfig {
    Parquet { output_dir: std::path::PathBuf },
    QuestDb,
}
```

Key design points:
- `Strategy` owns configuration and lifecycle.
- `AppEnvironment` is the abstraction around the runtime: subscriptions, sinks, metrics, and task spawning.
- `StrategyRegistry` enables discovery and dispatch by name (used by the CLI and runtime).

## Configuration and CLI

Main entry point should take an argument specifying which strategy to run and a path to a YAML config file.

- CLI example: `xtrader run --strategy md_collector --config md_collector.yaml --verbose`
- Config is validated before strategy start; the strategy receives the fully typed struct.

### Example: `md_collector.yaml`

```yaml
strategy: md_collector
runtime:
  metrics_port: 9898
  channel_capacity: 10000
sink:
  type: questdb # or parquet
  output_dir: ./test_output # required only for parquet
subscriptions:
  - exchange: OKX
    stream_type: L2
    instrument: BTC-USDT-SWAP
    arb_streams_num: 10
  - exchange: BINANCE
    stream_type: TRADES
    instrument: BTCUSDT
    # default is 1; shown here explicitly
    arb_streams_num: 1
```

### Mapping to existing modules
- Subscriptions use `subscription_manager` under the hood via `AppEnvironment::start_subscriptions`.
- Sinks wrap existing `output::run_multi_stream_*_sink` functions via `AppEnvironment::start_sink`.
- Metrics reuse `metrics::exporters` and the global registry; HTTP `/metrics` remains minimal and lightweight.

## MD Collector strategy design

The MD Collector becomes a normal strategy orchestrating subscriptions and sinks.

```rust
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
pub struct MdCollectorConfig {
    pub runtime: RuntimeSection,
    pub sink: SinkSection,
    pub subscriptions: Vec<SubscriptionItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeSection { pub channel_capacity: usize }

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SinkSection { Parquet { output_dir: std::path::PathBuf }, Questdb }

#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionItem {
    pub exchange: String,
    pub stream_type: String,
    pub instrument: String,
    // Optional: number of parallel feeds to use for latency arbitrage (default: 1)
    #[serde(default = "default_arb_streams_num")]
    pub arb_streams_num: usize,
}

fn default_arb_streams_num() -> usize { 1 }

pub struct MdCollector;

#[async_trait]
impl Strategy for MdCollector {
    type Config = MdCollectorConfig;

    fn name(&self) -> &'static str { "md_collector" }

    async fn start(&self, ctx: StrategyContext, cfg: Self::Config) -> AppResult<()> {
        let subs: Vec<SubscriptionDescriptor> = cfg
            .subscriptions
            .into_iter()
            .map(|s| SubscriptionDescriptor {
                exchange: crate::types::Exchange::from_str(&s.exchange).unwrap(),
                stream_type: crate::types::StreamType::from_str(&s.stream_type).unwrap(),
                instrument: s.instrument,
                arb_streams_num: s.arb_streams_num,
            })
            .collect();

        let rx = ctx
            .env
            .start_subscriptions(subs, cfg.runtime.channel_capacity)
            .await?;

        let sink_cfg = match cfg.sink {
            SinkSection::Parquet { output_dir } => SinkConfig::Parquet { output_dir },
            SinkSection::Questdb => SinkConfig::QuestDb,
        };

        let handle = ctx.env.start_sink(sink_cfg, rx).await?;
        let _ = handle.await?;
        Ok(())
    }

    async fn stop(&self) -> AppResult<()> { Ok(()) }
}
```

Flow:
- Build `SubscriptionDescriptor` list from config.
- Start subscriptions and obtain a channel receiver of `StreamData`.
- Start the selected sink and await completion or shutdown.
 - Optional: when `arb_streams_num` > 1, the environment should spin up that many parallel feeds for the subscription and fuse them in `md::latency_arb` into a single stream using timestamps; behavior is equivalent to the prior `L2:EXCHANGE@INSTRUMENT[10]` notation.

## Runtime lifecycle

1. Initialize logging, TLS, environment variables.
2. Start metrics exporters (Prometheus + optional StatsD) and minimal HTTP server.
3. Parse CLI, load YAML config, validate it.
4. Lookup strategy in `StrategyRegistry` and instantiate it.
5. Create `AppEnvironment` and pass it to the strategy via `StrategyContext`.
6. Run `Strategy::start`, then wait for CTRL-C or configured shutdown timer.
7. On shutdown, signal subscriptions and sinks to finish, await tasks, and flush metrics.

## Phased migration plan

Phase 1: Introduce API and wiring
- Add `app::env` implementing `AppEnvironment` using existing `subscription_manager` and `output` modules.
- Add `strats::mod` with `StrategyRegistry` and register `md_collector`.
- Move CLI dispatch into `app::cli` to support `--strategy` and `--config` while keeping compatibility with current flags.

Phase 2: Extract MD Collector
- Create `strats/md_collector` and move orchestration logic from `main.rs` into `MdCollector` using the new API.
- Route subscriptions and sink creation through `AppEnvironment` instead of ad-hoc wiring in `main.rs`.

Phase 3: Module layout clean-up
- Move `pipeline/*` to `md/*` (keeping public function names and re-exporting through `md::` to minimize breakage).
- Move shared types from `types.rs` to `xcommons/types.rs`; re-export `crate::types` for compatibility during the transition.
- Ensure `metrics`, `output`, and `exchanges` expose stable public APIs.

Phase 4: Trading base (scaffold only)
- Add `trading/account_state` scaffolding per `docs/trade_account.md` (no behavior changes yet).
- Define interfaces for OMS hooks without enabling order routing.

Phase 5: Hardening and performance
- Add structured config validation and error messages.
- Benchmarks for subscription-to-sink latency and throughput.
- Backpressure policy in `AppEnvironment::start_subscriptions` channel capacity settings.

## Performance and reliability

- The Strategy hot path must allocate minimally and avoid unnecessary clones.
- Channels should be sized per workload; expose `channel_capacity` in config and provide sensible defaults.
- Failures in sink or subscription tasks should trigger graceful shutdown, with clear error propagation.
- Metrics must be cheap to gather; HTTP `/metrics` remains a lightweight TCP responder.

## Testing and benchmarks

- Unit tests for config parsing and validation.
- Integration tests for end-to-end MD collection (see `tests/` and reuse existing QuestDB integration tests).
- Criterion benches for pipeline stages; keep reports under `criterion_reports/`.

## Typos and terminology fixes

- "algo trading engine" → "algorithmic trading engine".
- "enoties" → "entities".
- "parquest_sink" → "parquet_sink".
- "Startegy" → "Strategy".
- "environemnt" → "environment".
- "managable" → "manageable".

## Key takeaways

- The Strategy API is a small, stable surface: Strategy + AppEnvironment + Registry.
- MD Collector becomes a first-class strategy with YAML config.
- The runtime is responsible for environment setup, metrics, and graceful shutdown.
- The codebase layout separates exchange-specific code, MD processing, sinks, strategies, and the app runtime.