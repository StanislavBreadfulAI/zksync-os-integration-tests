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
- Crash-safe restart: errors are caught, logged, and the loop retries after a delay — a silently dead background task would give false positives in tests
- Start-to-start interval timing: next tick is scheduled from work-start, not work-end, preventing drift under slow RPC
- `Option<Duration>` per activity type: `None` = disabled, `Some(interval)` = enabled at that rate

## Out of Scope (for now)

- Replacing the production watchdog (that's a future extraction, not this work)
- Withdrawals / WithdrawalFinalize flows
- Prometheus metrics
- Contract call activity

## Design

### Wallet partitioning

A new `ACTIVITY_WALLET_KEYS: [&str; 5]` constant in `bin/zk-deployer/src/l1_l2_deposit.rs`, alongside the existing `DEFAULT_L2_RICH_KEYS`. These are public test fixture keys (e.g. Anvil HD mnemonic accounts #10–#14) — same pattern as test wallets, just a separate named pool.

A new `fund_activity_l2_wallets()` mirrors `fund_default_l2_wallets()` and is called from `apply` alongside existing wallet funding. The activity wallets are baked into the deployment cache snapshot — zero cost on cache hit.

`Chain` gains an `activity_wallets()` accessor returning the parsed `ACTIVITY_WALLET_KEYS` signers. Test code never touches these; activity code never touches `wallet(0..N)`. The boundary is enforced by construction.

### `ActivityConfig`

Lives in `tests/src/activity.rs` (not a separate crate — this is test infrastructure; extracting to `lib/` is deferred until there's a second consumer).

```rust
#[derive(Clone)]
pub struct ActivityConfig {
    /// L2 self-transfers, round-robin across ACTIVITY_WALLET_KEYS. None = disabled.
    pub l2_transfers: Option<Duration>,
    /// L1→L2 deposits via Bridgehub, rotating depositor wallet. None = disabled.
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
    tasks: Vec<JoinHandle<()>>,
}

impl ActivityHandle {
    /// Pause: background tasks sleep at the top of each iteration until resumed.
    pub fn pause(&self);
    /// Resume after a pause.
    pub fn resume(&self);
    /// Gracefully stop all background tasks and await their cancellation.
    pub async fn stop(mut self);
    /// Number of L2 transfers successfully sent (tx hash returned) so far.
    pub fn txs_sent(&self) -> u64;
    /// Number of L1→L2 deposits successfully submitted so far.
    pub fn deposits_sent(&self) -> u64;
}

impl Drop for ActivityHandle {
    // Aborts all tasks (fire-and-forget). Tests that need clean shutdown call stop() explicitly.
    fn drop(&mut self);
}
```

Counters increment on **confirmed send** (tx hash returned / deposit receipt received), not on submission attempt — meaningful for assertions like `assert!(handle.txs_sent() >= 3)`.

### Background task internals

Each enabled flow spawns one tokio task. A `CRASH_RESTART_DELAY: Duration = Duration::from_secs(3)` constant caps how long the outer loop waits before retrying after a crash. The task structure follows the watchdog crash-safe pattern:

```
outer loop (restart on crash):
    inner loop (normal operation):
        if paused: sleep briefly, continue
        let tick_start = Instant::now();
        do work (send tx / deposit)
        on success: increment counter
        on error: log, break inner → outer restarts after min(interval, CRASH_RESTART_DELAY)
        sleep until tick_start + interval  ← start-to-start timing
```

**L2 transfers**: rotate through `ACTIVITY_WALLET_KEYS` round-robin (index mod N). Each wallet has its own nonce sequence — no mutex needed.

**L1→L2 deposits**: use a single wallet from the activity pool (rotating slowly, one per restart), submitting via `deposit_eth()` from `zk-deployer`. Uses a small fixed amount (1 ETH) so the activity wallet balance is not exhausted quickly.

### `Chain::start_background_activity()`

```rust
impl Chain {
    pub fn start_background_activity(&self, config: ActivityConfig) -> ActivityHandle;
}
```

Spawns one tokio task per enabled flow. Returns the handle immediately — the caller owns it. `Chain` does not store the handle.

### `Ecosystem::start_background_activity()`

Thin wrapper: calls `chain.start_background_activity(config.clone())` for every chain, stores the `Vec<ActivityHandle>` internally. Handles are dropped (tasks aborted) when `Ecosystem` drops.

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
    // assert initial state while silent
    let handle = eco.chain().start_background_activity(ActivityConfig::deposits_only());
    // ... do test work under deposit noise ...
    handle.pause();
    // ... sensitive assertion with no new deposits ...
    handle.resume();
    assert!(handle.deposits_sent() >= 1);
    handle.stop().await;
    Ok(())
}
```

## File changes summary

| File | Change |
|------|--------|
| `bin/zk-deployer/src/l1_l2_deposit.rs` | Add `ACTIVITY_WALLET_KEYS`, `fund_activity_l2_wallets()` |
| `bin/zk-deployer/src/commands/apply/mod.rs` | Call `fund_activity_l2_wallets()` during apply |
| `tests/src/activity.rs` | New: `ActivityConfig`, `ActivityHandle`, `run_l2_transfers`, `run_l1_deposits` |
| `tests/src/chain.rs` | Add `activity_wallets()`, `start_background_activity()` |
| `tests/src/ecosystem.rs` | Add `Vec<ActivityHandle>`, `start_background_activity()` |
| `tests/src/fixtures/l1.rs` | Add `activity: Option<ActivityConfig>` param to `ecosystem` fixture |
| `tests/src/lib.rs` | Re-export `ActivityConfig`, `ActivityHandle` |

## Open questions

None — all design decisions resolved in review.
