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

`ACTIVITY_WALLET_KEYS: [&str; 10]` lives as a **private constant** in `tests/src/activity.rs`, not in `zk-deployer`. Private keys must not be reachable by test code via a public module (`zk_deployer::l1_l2_deposit` is `pub mod`); keeping them in the tests crate and unexported enforces the boundary by module visibility.

`fund_activity_l2_wallets()` in `bin/zk-deployer/src/l1_l2_deposit.rs` takes `recipients: &[Address]` as a parameter — it only needs L2 destination addresses, not private keys. The fixture derives addresses from `ACTIVITY_WALLET_KEYS` and passes them in.

`default_builder()` in `bin/zk-deployer/src/anvil.rs` gains `--accounts 20`, so Anvil pre-funds all 20 accounts (#0–#19) with 10,000 ETH on L1. Accounts #10–#19 are the activity wallets' L1 addresses; no separate L1 funding step is needed.

**Deposit wallet assignment:** each chain gets one dedicated deposit wallet from the activity pool (`chain_index % ACTIVITY_WALLET_KEYS.len()`). `Ecosystem::start_background_activity()` panics if `chain_count > ACTIVITY_WALLET_KEYS.len()` — a fail-fast rather than silent L1 nonce sharing. With 10 keys this covers all realistic test topologies.

**State step:** `StepKey::ChainActivityWalletsFunded(u64)` is added to `state.rs` as a distinct resumable step alongside `ChainL2Funded`. The apply command checks and skips only if this specific step is already marked done — a state.json from a previous run without activity wallet support resumes correctly. The test cache auto-invalidates via content hashing (`zk_deployer_src` and `tests_src` both change when this feature is introduced); the distinct step key covers the non-cache (manual state.json) path.

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
    failed: Arc<AtomicBool>,    // set when any task exhausts MAX_CONSECUTIVE_ERRORS
    activity_started: Arc<AtomicBool>,  // cleared on stop()/Drop to allow restart
    tasks: Vec<JoinHandle<()>>,
}

impl ActivityHandle {
    /// Stops new submissions. An in-flight L1→L2 deposit submitted just before
    /// pause() may still arrive on L2 — do not rely on pause() for exact quiescing.
    pub fn pause(&self);
    /// Resume after a pause.
    pub fn resume(&self);
    /// Stop all background tasks, await cancellation, and clear the activity_started
    /// guard so start_background_activity() can be called again on the same chain.
    pub async fn stop(mut self);
    /// Returns false if any task has exhausted MAX_CONSECUTIVE_ERRORS and exited.
    /// A healthy handle returns true even under transient errors (retries remain).
    pub fn is_alive(&self) -> bool;
    /// Number of L2 transfers whose tx hash was accepted by the RPC (submitted to
    /// mempool, not necessarily mined).
    pub fn txs_sent(&self) -> u64;
    /// Number of L1→L2 deposit transactions confirmed on L1 so far.
    pub fn deposits_sent(&self) -> u64;
}

impl Drop for ActivityHandle {
    // Aborts all tasks and clears activity_started (fire-and-forget).
    fn drop(&mut self);
}
```

### Background task internals

```rust
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
const CRASH_RESTART_DELAY: Duration = Duration::from_secs(3);
```

Each enabled flow spawns one tokio task:

```
consecutive_errors = 0
outer loop (restart on crash):
    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS:
        set failed flag, return   ← terminal failure; is_alive() returns false
    inner loop (normal operation):
        if paused: sleep briefly, continue
        let tick_start = Instant::now();
        do work (send tx / deposit)
        on success: increment counter; consecutive_errors = 0
        on error:   log; consecutive_errors += 1; break inner
    sleep min(interval, CRASH_RESTART_DELAY) before outer retry
    sleep until tick_start + interval  ← start-to-start timing
```

**L2 transfers**: rotate through `ACTIVITY_WALLET_KEYS` round-robin (index mod N). Each wallet has its own nonce sequence — no mutex needed.

**L1→L2 deposits**: each chain uses one dedicated wallet (`chain_index % ACTIVITY_WALLET_KEYS.len()`). That wallet owns its own L1 nonce sequence — no races between chains.

### `Chain::start_background_activity()`

```rust
impl Chain {
    pub fn start_background_activity(&self, config: ActivityConfig) -> ActivityHandle;
}
```

`Chain` holds `activity_started: Arc<AtomicBool>`. On call, atomically sets it (panics if already set — programming error). Returns the handle immediately. The handle holds a clone of the same `Arc`; `stop()`/`Drop` clears it, allowing restart.

The fixture path waits for the first successful tick on each enabled flow before returning — a misconfigured setup fails fast at fixture time.

### `Ecosystem::start_background_activity()`

Thin wrapper: calls `chain.start_background_activity(config.clone())` for every chain, panics if `chain_count > ACTIVITY_WALLET_KEYS.len()`, stores the `Vec<ActivityHandle>` internally. Handles drop (tasks abort, guards clear) when `Ecosystem` drops.

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
    handle.stop().await; // clears guard; could call start_background_activity() again
    Ok(())
}
```

## File changes summary

| File | Change |
|------|--------|
| `bin/zk-deployer/src/anvil.rs` | Add `--accounts 20` to `default_builder()` |
| `bin/zk-deployer/src/l1_l2_deposit.rs` | Add `fund_activity_l2_wallets(recipients: &[Address])` |
| `bin/zk-deployer/src/commands/apply/mod.rs` | Call `fund_activity_l2_wallets()` as a distinct step; add `StepKey::ChainActivityWalletsFunded(u64)` |
| `bin/zk-deployer/src/state.rs` | Add `StepKey::ChainActivityWalletsFunded(u64)` variant |
| `tests/src/activity.rs` | New: private `ACTIVITY_WALLET_KEYS`, `ActivityConfig`, `ActivityHandle`, `run_l2_transfers`, `run_l1_deposits` |
| `tests/src/chain.rs` | Add `activity_started: Arc<AtomicBool>`, `start_background_activity()` |
| `tests/src/ecosystem.rs` | Add `Vec<ActivityHandle>`, `start_background_activity()` with chain count guard |
| `tests/src/fixtures/l1.rs` | Derive activity wallet addresses, pass to `fund_activity_l2_wallets()`; add `activity: Option<ActivityConfig>` param |
| `tests/src/lib.rs` | Re-export `ActivityConfig`, `ActivityHandle` |
