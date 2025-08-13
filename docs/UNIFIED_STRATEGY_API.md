## Unified Strategy API: single-threaded, handler-first design

Goal: make strategy development feel like single-threaded programming while keeping the runtime highly concurrent, low-latency, and safe. Strategies declare subscriptions and implement typed handlers; the framework handles fan-in, backpressure, and concurrency outside of the strategy task.

### Key principles

- Simplicity first: the strategy is a single task with a mailbox and synchronous handlers.
- Explicit subscription-to-handler mapping: every subscribe call specifies which handler receives messages.
- Zero shared-state inside strategies: all mutable state is owned by the strategy task.
- High performance by default: bounded mailboxes, backpressure policy, minimal allocations, pre-sized channels.
- Clear separation of concerns: strategies orchestrate; environment runs subscriptions, account runners, and sinks [[memory:5988415]].

### High-level API sketch

Core types added/extended in `src/strats/api.rs` and `src/app/env.rs`:

```rust
/// Unique handle for each subscription declared by the strategy
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct SubscriptionId(pub u32);

/// Messages delivered to a single-threaded strategy mailbox
pub enum StrategyMessage {
    MarketDataL2 { sub: SubscriptionId, msg: OrderBookL2Update },
    MarketDataTrade { sub: SubscriptionId, msg: TradeUpdate },
    Execution { account_id: i64, exec: XExecution },
    Position { account_id: i64, market_id: XMarketId, pos: Position },
    Control(StrategyControl),
}

pub enum StrategyControl { Start, Stop, Timer, User(serde_json::Value) }

/// Strategy-visible IO helpers (placing orders, emitting metrics, forwarding to sinks, etc.)
pub struct StrategyIo<'a> {
    pub env: &'a dyn AppEnvironment,
    // future: order routing, custom sinks, timers, metrics helpers
}

/// Single-threaded strategy trait
#[async_trait::async_trait]
pub trait Strategy: Send + Sync {
    type Config: serde::de::DeserializeOwned + Send + Sync + 'static;

    fn name(&self) -> &'static str;

    /// Declare subscriptions and initial timers; no background tasks here
    async fn configure(&mut self, reg: &mut StrategyRegistrar, ctx: &StrategyContext, cfg: &Self::Config) -> AppResult<()>;

    /// Typed handlers (sync, executed on the strategy task)
    fn on_l2(&mut self, _sub: SubscriptionId, _msg: OrderBookL2Update, _io: &mut StrategyIo) {}
    fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}
    fn on_execution(&mut self, _account_id: i64, _exec: XExecution, _io: &mut StrategyIo) {}
    fn on_position(&mut self, _account_id: i64, _market_id: XMarketId, _pos: Position, _io: &mut StrategyIo) {}
    fn on_control(&mut self, _c: StrategyControl, _io: &mut StrategyIo) {}
}

/// Registrar offered to the strategy during configure()
pub struct StrategyRegistrar<'a> {
    pub mailbox_tx: tokio::sync::mpsc::Sender<StrategyMessage>,
    pub env: &'a dyn AppEnvironment,
}

impl<'a> StrategyRegistrar<'a> {
    pub fn subscribe_l2(&mut self, spec: SubscriptionSpec) -> SubscriptionId { /* registers and wires to mailbox */ }
    pub fn subscribe_trades(&mut self, spec: SubscriptionSpec) -> SubscriptionId { /* registers and wires to mailbox */ }
    pub fn subscribe_executions(&mut self, account_id: i64) { /* wires XExecution -> mailbox */ }
    pub fn subscribe_positions(&mut self, account_id: i64) { /* wires Position -> mailbox */ }
    pub fn set_timer(&mut self, every: std::time::Duration) { /* posts StrategyControl::Timer */ }
}
```

Environment additions in `AppEnvironment`:

```rust
pub trait AppEnvironment {
    fn channel_capacity(&self) -> usize;
    fn verbose(&self) -> bool;
    fn prom_registry(&self) -> Arc<metrics::exporters::PrometheusExporter>;

    /// Existing multi-stream start (kept for sinks and utilities)
    fn start_subscriptions(&self, subs: Vec<SubscriptionSpec>) -> mpsc::Receiver<StreamData>;

    /// New: wire a single subscription directly into a strategy mailbox with typed dispatch
    fn wire_l2_to_strategy(&self, sub: SubscriptionSpec, tx: mpsc::Sender<StrategyMessage>, sub_id: SubscriptionId) -> Result<tokio::task::JoinHandle<Result<()>>>;
    fn wire_trades_to_strategy(&self, sub: SubscriptionSpec, tx: mpsc::Sender<StrategyMessage>, sub_id: SubscriptionId) -> Result<tokio::task::JoinHandle<Result<()>>>;

    /// Accounts → strategy
    fn wire_executions_to_strategy(&self, account: BinanceAccountParams, tx: mpsc::Sender<StrategyMessage>) -> Result<Vec<tokio::task::JoinHandle<()>>>;
    fn wire_positions_to_strategy(&self, account: BinanceAccountParams, tx: mpsc::Sender<StrategyMessage>) -> Result<Vec<tokio::task::JoinHandle<()>>>;

    fn start_sink(&self, cfg: EnvSinkConfig, rx: mpsc::Receiver<StreamData>) -> Result<tokio::task::JoinHandle<Result<()>>>;
}
```

Notes:
- The existing `start_subscriptions` is preserved for bulk sinks and tooling.
- Strategy-first methods wire typed updates straight into the strategy mailbox with no extra allocations other than message move. The underlying unified handlers already exist (`SubscriptionManager`/`UnifiedHandlerFactory`).

### Threading model

- One Tokio task per strategy (single-threaded). The strategy owns all mutable state and processes `StrategyMessage` sequentially.
- Each subscription/account runner runs on its own task(s) managed by the environment. They forward messages into the strategy mailbox via `try_send` to honor backpressure.
- Backpressure policy (configurable):
  - bounded mailbox size (default: 8192)
  - drop-oldest for bursty market data; never drop executions/positions
  - metrics counters for dropped MD messages

### Backpressure and latency

- Mailbox capacity and drop policy are part of strategy runtime config:

```yaml
runtime:
  channel_capacity: 8192
  strategy_mailbox_capacity: 8192
  drop_policy: { market_data: DropOldest, control: KeepLatest }
```

- Subscription spec format remains unchanged and supports arbitration width for latency arb [[memory:6129442]]. Example:

```text
L2:OKX_SWAP@BTCUSDT[4],TRADES:BINANCE_FUTURES@ETHUSDT
```

### Developer experience: lifecycle

1) Implement `Strategy` with typed handlers only; no async locking, no channels.
2) In `configure`, call `subscribe_l2`, `subscribe_trades`, `subscribe_executions`, `subscribe_positions` as needed.
3) The framework runs a single-threaded loop that dispatches to your handlers.

Example:

```rust
pub struct NaiveMmStrat {
    fair_px: f64,
}

#[async_trait::async_trait]
impl Strategy for NaiveMmStrat {
    type Config = NaiveMmConfig;

    fn name(&self) -> &'static str { "naive_mm" }

    async fn configure(&mut self, reg: &mut StrategyRegistrar, _ctx: &StrategyContext, cfg: &Self::Config) -> AppResult<()> {
        for s in &cfg.subscriptions { // typed enums already parsed
            let spec = SubscriptionSpec { stream_type: s.stream_type, exchange: s.exchange, instrument: s.instrument.clone(), max_connections: None };
            match s.stream_type { StreamType::L2 => { reg.subscribe_l2(spec); }, StreamType::Trades => { reg.subscribe_trades(spec); } }
        }
        Ok(())
    }

    fn on_l2(&mut self, _sub: SubscriptionId, msg: OrderBookL2Update, _io: &mut StrategyIo) {
        // update fair price using best bid/ask or inventory skew
        self.fair_px = msg.price;
    }

    fn on_trade(&mut self, _sub: SubscriptionId, _msg: TradeUpdate, _io: &mut StrategyIo) {}
}
```

### How this maps to current code

- `Strategy`, `StrategyContext`, and `AppEnvironment` already exist [[memory:5988415]]. We add `configure` and typed handlers while keeping `name` and the existing registry.
- `SubscriptionManager` already spawns per-subscription unified handlers and can be extended to push typed messages into an arbitrary `mpsc::Sender<StrategyMessage>` (instead of, or in addition to, `StreamData` fan-in).
- Accounts: `AccountState` exposes `subscribe_executions` and `subscribe_positions` broadcast receivers today. The environment can subscribe and forward to the strategy mailbox without changing account internals.
- Sinks: keep `start_sink` with unified `StreamData` for Parquet/QuestDB pipelines; strategies can opt-in to forward selected messages via a small helper on `StrategyIo`.

### Minimal implementation plan (incremental, safe)

1) Add `StrategyMessage`, `SubscriptionId`, `StrategyIo`, and `StrategyRegistrar` to `src/strats/api.rs` (behind a feature flag, e.g., `feature = "strat-single-thread"`).
2) Extend `AppEnvironment` with `wire_*_to_strategy` methods that adapt `SubscriptionManager`/`UnifiedHandlerFactory` runners to a provided mailbox.
3) Implement a `StrategyRunner` that:
   - creates the mailbox (`mpsc::channel(strategy_mailbox_capacity)`)
   - builds a `StrategyRegistrar` and calls `strategy.configure(...)`
   - spawns the needed wires in the environment
   - runs a `while let Some(msg) = rx.recv().await { dispatch }` loop
4) Rework `md_collector` example to the new API; keep current path working in parallel.
5) Rework `naive_mm` to declare subscriptions in `configure` and move logic to typed handlers.
6) Add metrics: dropped messages, per-handler latency, handler error counters.
7) Document the API and examples (this file) and update `README`/`docs/code_structure.md`.

### Performance considerations

- Keep messages small and `Copy`-friendly fields; avoid heap allocations in hot path.
- Use `try_send` with drop-oldest for market data to avoid head-of-line blocking.
- Use a dedicated single-threaded Tokio runtime for strategy tasks if desired (current-thread), ensuring no accidental parallel handler execution.
- Reuse existing unified handlers and OKX registries; do not duplicate parsing.

### Error handling and recoverability

- Market data wires auto-reconnect as they do today; upon reconnect, messages continue into the same mailbox.
- Strategy handler panics abort the strategy task only; environment logs and triggers `Stop` control to enable clean shutdown.
- Executions/positions are lossless (no drops); if mailbox is saturated, MD drops first.

### Configuration alignment

- Config keeps typed enums and subscription parsing as-is; `STREAM:EXCHANGE@SYMBOL[<maxConnections>]` remains valid [[memory:6129442]].
- Add optional `strategy_mailbox_capacity` and `drop_policy` into existing `runtime` sections; default to current channel capacity for a sensible OOTB experience [[memory:5988415]].

### Benefits

- Single-threaded mental model for strategy authors; no locks/channels in strategy code.
- Clear, typed handlers for MD, executions, positions.
- Backpressure-safe with predictable latency under load.
- Backwards compatible rollout; existing sinks and collectors continue to work.

## Industry best practices integrated (HFT/low-latency)

- Single-writer principle (Disruptor-style): strategy state is mutated only by the strategy task; all inputs are serialized into a mailbox. Eliminates locks and false sharing.
- Bounded SPSC mailbox: non-blocking `try_send` into a fixed-capacity queue; avoids head-of-line blocking. MD uses drop-oldest/coalescing; executions/positions are lossless.
- Sequence and watermarking: each subscription carries a monotonically increasing sequence; dispatch validates monotonicity and emits metrics if gaps/out-of-order occur (no reordering in hot path).
- Micro-batching: the dispatcher drains up to N messages per tick (configurable), amortizing wakeups and improving cache locality. Handlers remain simple and synchronous.
- Coalescing for market data: optional per-subscription coalescing reduces redundant work under burst (e.g., keep latest L2 per-symbol within a tick). Never coalesce executions/positions.
- Minimal allocations: favor stack data, small POD structs, and interned identifiers. Prefer `XMarketId`/`ExchangeId` over `String` tickers in handler-facing messages.
- Current-thread runtime and CPU pinning: run the strategy loop on a dedicated Tokio current-thread runtime, pin the OS thread to a CPU core, and avoid cross-core migrations for stable latency.
- Backpressure visibility: expose gauges/counters for mailbox depth, drops (by reason/type), and handler latencies (p50/p99). Keep metrics collection low-overhead.
- Deterministic record/replay: support binary logging of `StrategyMessage` for deterministic offline simulation and regression.

### API refinements to reflect best practices

- Add optional batch handlers for hot paths:

```rust
// Optional fast-path: receive a small batch drained from the mailbox
fn on_l2_batch(&mut self, _batch: &mut smallvec::SmallVec<[OrderBookL2Update; 32]>, _io: &mut StrategyIo) {}
fn on_trade_batch(&mut self, _batch: &mut smallvec::SmallVec<[TradeUpdate; 64]>, _io: &mut StrategyIo) {}
```

- Add per-subscription drop/coalesce policy:

```rust
pub enum MdDropPolicy { DropOldest, DropNewest, CoalesceLatest, Block }

impl<'a> StrategyRegistrar<'a> {
    pub fn subscribe_l2_with_policy(&mut self, spec: SubscriptionSpec, policy: MdDropPolicy) -> SubscriptionId { /* ... */ }
}
```

- Mailbox configuration in runtime config:

```yaml
runtime:
  strategy_mailbox_capacity: 8192
  strategy_batch_limit: 64
  md_drop_policy: CoalesceLatest
  prioritize_control_msgs: true
```

- Prefer stable identifiers in messages. In addition to `ticker: String`, include `market_id: XMarketId` in `StrategyMessage` variants to eliminate string hashing in hot paths.

### OS/runtime tuning hooks (optional, off by default)

- Pin strategy thread (affinity) and set `tokio::runtime::Builder::new_current_thread()` with tuned `event_interval`.
- Enable busy-poll on NIC/queues where available; keep this at the transport layer, not inside the strategy.
- Avoid CPU frequency scaling; prefer performance governor on Linux. On macOS, keep background load minimal for predictable timers.

### Observability additions

- Per-subscription metrics: input rate, drops by policy, backlog, seq gap counters.
- Per-handler latency histograms and time-in-handler budgets to surface long GC-like pauses.
- Optional heap profiling markers around handler bursts to track allocation regressions.

### Record/replay for testing

- Binary log format for `StrategyMessage` with versioned header and per-message checksums.
- CLI to replay logs into a strategy in-process at original or scaled timing.
- Golden-tests for handlers: feed recorded bursts and assert outputs/side-effects deterministically.


