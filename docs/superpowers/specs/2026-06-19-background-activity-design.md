# Background Activity Mode — Design Spec

**Date:** 2026-06-19
**Status:** Draft

## Problem

The current test framework is "silent by default": chains produce no activity until a test explicitly calls `ping()`, `send_tx()`, `transfer()`, etc. This means tests always run against a perfectly idle chain, which makes race conditions and concurrency bugs between the sequencer, L1 watcher, priority queue, and batch pipeline invisible.

## Goal

Add an opt-in **background activity mode** that continuously fires L2 transactions and L1→L2 deposits on a chain while a test runs. Tests execute inside a live, moving chain rather than an isolated vacuum — exposing ordering/interleaving bugs that only surface under real concurrent traffic.

## Inspiration

The production equivalent is the [`matter-labs/watchdog`](https://github.com/matter-labs/watchdog) TypeScript service. Key patterns borrowed:
- Flow-per-activity-type: each activity runs as an independent loop with its own restart logic
- Crash-safe restart with bounded budget: errors are caught, logged, and the loop retries — but gives up after `MAX_CONSECUTIVE_ERRORS` consecutive failures
- Start-to-start interval timing: next tick is scheduled from work-start, not work-end, preventing drift under slow RPC
- `Option<Duration>` per activity type: `None` = disabled, `Some(interval)` = enabled at that rate

## Out of Scope (for now)

- Replacing the production watchdog (that's a future extraction, not this work)
- Withdrawals / WithdrawalFinalize flows
- Prometheus metrics
- Contract call activity

## Design

### Wallet partitioning

`ACTIVITY_WALLET_KEYS: [&str; 10]` is a **private constant in `tests/src/activity.rs`** — not in `zk-deployer`. `zk_deployer::l1_l2_deposit` is a `pub mod`, so any constant placed there is reachable by test code. Keeping the keys in the tests crate and unexported enforces the boundary by module visibility.

**L2 funding** is done directly in the fixture's cache-miss path (not inside `apply`): after `apply::run()` completes and before `save_state()`, the fixture calls `zk_deployer::l1_l2_deposit::deposit_eth()` once per activity wallet per chain, deriving addresses from the private `ACTIVITY_WALLET_KEYS` constant. The deposits land in the L1 snapshot and are cached with everything else. No changes to `ApplyArgs`, no new `StepKey`, no changes to `state.rs`.

**L1 funding** is handled by adding `--accounts 20` to `default_builder()` in `bin/zk-deployer/src/anvil.rs`. Anvil pre-funds all 20 accounts (#0–#19) with 10,000 ETH on L1; accounts #10–#19 are the activity wallets' L1 addresses.

**Cache invalidation:** adding `ACTIVITY_WALLET_KEYS` and `activity.rs` to `tests/src/` changes the `tests_src` content hash, which is an input to the cache key — existing entries auto-invalidate.

**Deposit wallet assignment:** `Chain` holds an `activity_wallet_index: usize` set during `Ecosystem::assemble()` as `i % ACTIVITY_WALLET_KEYS.len()`. The deposit loop uses `ACTIVITY_WALLET_KEYS[self.activity_wallet_index]` — one dedicated L1 signer per chain, no nonce races. `Ecosystem::start_background_activity()` panics if `chain_count > ACTIVITY_WALLET_KEYS.len()` (10 keys covers all realistic topologies). For `Chain` objects built outside of `Ecosystem` (e.g. the v30 fixture), `activity_wallet_index` defaults to 0.

### `ActivityConfig`

Lives in `tests/src/activity.rs`.

```rust
#[derive(Clone)]
pub struct ActivityConfig {
    /// L2 self-transfers, round-robin across ACTIVITY_WALLET_KEYS. None = disabled.
    pub l2_transfers: Option<Duration>,
    /// L1→L2 deposits via Bridgehub, one dedicated wallet per chain. None = disabled.
    pub l1_deposits: Option<Duration>,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            l2_transfers: Some(Duration::from_millis(500)),
            l1_deposits: Some(Duration::from_secs(5)),
        }
    }
}

impl ActivityConfig {
    pub fn transfers_only() -> Self {
        Self { l2_transfers: Some(Duration::from_millis(500)), l1_deposits: None }
    }

    pub fn deposits_only() -> Self {
        Self { l2_transfers: None, l1_deposits: Some(Duration::from_secs(5)) }
    }
}
```

### `ActivityHandle`

```rust
pub struct ActivityHandle {
    txs_sent: Arc<AtomicU64>,
    deposits_sent: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,             // set when any task exhausts MAX_CONSECUTIVE_ERRORS
    activity_started: Arc<AtomicBool>,   // cleared on stop()/Drop to allow restart
    tasks: Vec<JoinHandle<()>>,
}

impl ActivityHandle {
    /// Stops new submissions. An in-flight L1→L2 deposit submitted just before
    /// pause() may still arrive on L2 — do not rely on pause() for exact quiescing.
    pub fn pause(&self);
    /// Resume after a pause.
    pub fn resume(&self);
    /// Gracefully stop all background tasks, await cancellation, and clear the
    /// activity_started guard. Only stop().await guarantees restart safety; do
    /// not drop-then-restart (Drop aborts without awaiting).
    pub async fn stop(mut self);
    /// Returns false if any task has exhausted MAX_CONSECUTIVE_ERRORS and exited.
    pub fn is_alive(&self) -> bool;
    /// Number of L2 transfers whose tx hash was accepted by the RPC (submitted to
    /// mempool, not necessarily mined).
    pub fn txs_sent(&self) -> u64;
    /// Number of L1→L2 deposit transactions confirmed on L1 so far.
    pub fn deposits_sent(&self) -> u64;
}

impl Drop for ActivityHandle {
    // Teardown only: aborts tasks and clears activity_started fire-and-forget.
    // The old task may still be unwinding when the guard clears — do not rely on
    // Drop for restart safety. Use stop().await instead.
    fn drop(&mut self);
}
```

### Background task internals

```rust
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
const CRASH_RESTART_DELAY: Duration = Duration::from_secs(3);
```

Each enabled flow spawns one tokio task. Single flat loop — success path paces by interval, error path backs off by restart delay:

```
consecutive_errors = 0
loop:
    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS:
        set failed flag, return              ← terminal; is_alive() → false

    if paused:
        sleep briefly
        continue

    tick_start = Instant::now()

    match do_work():
        Ok =>
            counter += 1
            consecutive_errors = 0
            sleep_until(tick_start + interval)          ← start-to-start pacing

        Err(e) =>
            log(e)
            consecutive_errors += 1
            sleep(min(interval, CRASH_RESTART_DELAY))   ← back-off before retry
```

**L2 transfers**: rotate through `ACTIVITY_WALLET_KEYS` round-robin (index mod N). Each wallet owns its own nonce sequence — no mutex needed.

**L1→L2 deposits**: use `self.activity_wallet_index` (set per chain in `Ecosystem::assemble()`). One L1 signer per chain — no nonce races between chains.

### `Chain::start_background_activity()`

```rust
impl Chain {
    pub fn start_background_activity(&self, config: ActivityConfig) -> ActivityHandle;
}
```

`Chain` holds `activity_started: Arc<AtomicBool>` and `activity_wallet_index: usize`. On call, atomically sets `activity_started` (panics if already set — programming error). Returns the handle immediately. The handle holds a clone of `activity_started`; `stop().await` clears it safely. `Drop` also clears it but without await — use only for end-of-test teardown, not for restart.

The fixture path waits for the first successful tick on each enabled flow before returning — a misconfigured setup fails fast at fixture time.

### `Ecosystem::start_background_activity()`

Thin wrapper: panics if `chain_count > ACTIVITY_WALLET_KEYS.len()`, then calls `chain.start_background_activity(config.clone())` for every chain. Stores the `Vec<ActivityHandle>` internally; handles drop when `Ecosystem` drops.

### Fixture integration

```rust
#[fixture]
pub async fn ecosystem(
    #[default(vec![TEST_CHAIN_ID])] chains: Vec<u64>,
    #[default(None)] activity: Option<ActivityConfig>,
) -> Ecosystem;
```

**Silent (existing tests — unchanged):**
```rust
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn my_test(#[future] ecosystem: Ecosystem) -> Result<()> { ... }
```

**Noisy from the start (default rates):**
```rust
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn my_noisy_test(
    #[future]
    #[with(vec![6565], Some(ActivityConfig::default()))]
    ecosystem: Ecosystem,
) -> Result<()> { ... }
```

**Fine-grained control (manual handle):**
```rust
async fn my_precise_test(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let handle = eco.chain().start_background_activity(ActivityConfig::deposits_only());
    // ... test work under deposit noise ...
    handle.pause(); // stops new submissions; an in-flight deposit may still land
    // ... assertion that tolerates a final in-flight deposit ...
    handle.resume();
    assert!(handle.deposits_sent() >= 1);
    assert!(handle.is_alive());
    handle.stop().await; // clears guard safely; start_background_activity() can be called again
    Ok(())
}
```

## File changes summary

| File | Change |
|------|--------|
| `bin/zk-deployer/src/anvil.rs` | Add `--accounts 20` to `default_builder()` |
| `tests/src/activity.rs` | New: private `ACTIVITY_WALLET_KEYS`, `ActivityConfig`, `ActivityHandle`, `run_l2_transfers`, `run_l1_deposits` |
| `tests/src/chain.rs` | Add `activity_started: Arc<AtomicBool>`, `activity_wallet_index: usize`, `start_background_activity()` |
| `tests/src/ecosystem.rs` | Add `Vec<ActivityHandle>`, `start_background_activity()` with chain count guard |
| `tests/src/fixtures/l1.rs` | Fund activity wallets after apply and before snapshot; add `activity: Option<ActivityConfig>` param |
| `tests/src/lib.rs` | Re-export `ActivityConfig`, `ActivityHandle` |
