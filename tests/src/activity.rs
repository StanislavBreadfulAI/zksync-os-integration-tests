//! Opt-in background activity ("noise") for integration tests.
//!
//! When enabled, dedicated activity wallets continuously fire L2 transfers and
//! L1→L2 deposits on a chain while a test runs, so the test exercises a live,
//! moving chain rather than an idle one. See `Chain::start_background_activity`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use tokio::task::JoinHandle;
use zk_deployer::l1_l2_deposit::{deposit_eth, DEFAULT_L1_TO_L2_GAS_PRICE};

/// After this many consecutive failed iterations with no success in between, a
/// flow task gives up: it sets the `failed` flag and exits. `is_alive()` then
/// returns false, so a broken background loop is detectable rather than silent.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Back-off slept after a failed iteration before the next retry.
pub const CRASH_RESTART_DELAY: Duration = Duration::from_secs(3);

/// Amount of ETH per background L1→L2 deposit (1 ETH). Small enough that a
/// dedicated activity wallet (pre-funded with 10,000 ETH on L1) lasts the whole
/// test run.
pub(crate) const ACTIVITY_DEPOSIT_AMOUNT_WEI: u128 = 1_000_000_000_000_000_000;

/// L2 ETH funded to each activity wallet per chain at setup (via L1→L2 deposit)
/// so it can pay gas for background self-transfers.
///
/// Kept small on purpose: activity wallets need only L2 gas money — the 1-wei
/// self-transfers cost nothing of substance, and the L1→L2 deposits are paid
/// from the wallet's L1 balance, not this. Funding all 10 wallets on every
/// chain from the deployer's 10,000 ETH adds up fast on multi-chain setups, so
/// this stays well below the per-test-wallet amount that `apply` uses.
pub(crate) const ACTIVITY_WALLET_L2_FUND_ETH: u64 = 10;

/// Private activity-wallet pool: Anvil HD mnemonic accounts #10–#19.
///
/// Kept private to this module (and out of `zk-deployer`, whose `l1_l2_deposit`
/// is a public module) so test code cannot reach these keys — the
/// test-wallet / activity-wallet boundary is enforced by visibility.
///
/// `default_builder()` passes `--accounts 20`, so these accounts are pre-funded
/// with ETH on L1.
pub(crate) const ACTIVITY_WALLET_KEYS: [&str; 10] = [
    // Anvil #10 — 0xBcd4042DE499D14e55001CcbB24a551F3b954096
    "0xf214f2b2cd398c806f84e317254e0f0b801d0643303237d97a22a48e01628897",
    // Anvil #11 — 0x71bE63f3384f5fb98995898A86B02Fb2426c5788
    "0x701b615bbdfb9de65240bc28bd21bbc0d996645a3dd57e7b12bc2bdf6f192c82",
    // Anvil #12 — 0xFABB0ac9d68B0B445fB7357272Ff202C5651694a
    "0xa267530f49f8280200edf313ee7af6b827f2a8bce2897751d06a843f644967b1",
    // Anvil #13 — 0x1CBd3b2770909D4e10f157cABC84C7264073C9Ec
    "0x47c99abed3324a2707c28affff1267e45918ec8c3f20b8aa892e8b065d2942dd",
    // Anvil #14 — 0xdF3e18d64BC6A983f673Ab319CCaE4f1a57C7097
    "0xc526ee95bf44d8fc405a158bb884d9d1238d99f0612e9f33d006bb0789009aaa",
    // Anvil #15 — 0xcd3B766CCDd6AE721141F452C550Ca635964ce71
    "0x8166f546bab6da521a8369cab06c5d2b9e46670292d85c875ee9ec20e84ffb61",
    // Anvil #16 — 0x2546BcD3c84621e976D8185a91A922aE77ECEc30
    "0xea6c44ac03bff858b476bba40716402b03e41b8e97e276d1baec7c37d42484a0",
    // Anvil #17 — 0xbDA5747bFD65F08deb54cb465eB87D40e51B197E
    "0x689af8efa8c651a91ad287602527f3af2fe9f6501a7ac4b061667b5a93e037fd",
    // Anvil #18 — 0xdD2FD4581271e230360230F9337D5c0430Bf44C0
    "0xde9be858da4a475276426320d5e9262ecfc3ba460bfac56360bfa6c4c28b4ee0",
    // Anvil #19 — 0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199
    "0xdf57089febbacf7ba0bc227dafbffa9fc08a93fdc68e1e42411a14efcf23656e",
];

/// What background activity to run on a chain. `None` disables a flow.
#[derive(Clone)]
pub struct ActivityConfig {
    /// Interval between L2 self-transfers (round-robin across the wallet pool).
    pub l2_transfers: Option<Duration>,
    /// Interval between L1→L2 deposits (one dedicated wallet per chain).
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
    /// L2 transfers only, no deposits.
    pub fn transfers_only() -> Self {
        Self {
            l2_transfers: Some(Duration::from_millis(500)),
            l1_deposits: None,
        }
    }

    /// L1→L2 deposits only, no L2 transfers.
    pub fn deposits_only() -> Self {
        Self {
            l2_transfers: None,
            l1_deposits: Some(Duration::from_secs(5)),
        }
    }
}

/// Point-in-time-ish view of a chain's background activity.
///
/// The fields are read with independent relaxed loads, so this is a
/// near-snapshot — not one atomic read. A counter may tick between two field
/// reads. Fine for test assertions and logging; do not treat the fields as
/// mutually consistent to the instruction.
#[derive(Debug, Clone, Copy)]
pub struct ActivityStats {
    /// L2 transfers whose tx hash was accepted by the RPC (mempool, not mined).
    pub txs_sent: u64,
    /// L1→L2 deposit transactions confirmed on L1.
    pub deposits_sent: u64,
    /// True once a flow task has exhausted `MAX_CONSECUTIVE_ERRORS` and exited.
    pub failed: bool,
    /// True while submissions are paused.
    pub paused: bool,
}

struct ActivityStateInner {
    txs_sent: AtomicU64,
    deposits_sent: AtomicU64,
    paused: AtomicBool,
    failed: AtomicBool,
}

/// Shared, cloneable state between the handle and its background tasks: one
/// `Arc` over all counters/flags. The tasks are the writers; the handle reads.
#[derive(Clone)]
pub(crate) struct ActivityState(Arc<ActivityStateInner>);

impl ActivityState {
    pub(crate) fn new() -> Self {
        Self(Arc::new(ActivityStateInner {
            txs_sent: AtomicU64::new(0),
            deposits_sent: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }))
    }

    pub(crate) fn record_tx(&self) {
        self.0.txs_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_deposit(&self) {
        self.0.deposits_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn mark_failed(&self) {
        self.0.failed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.0.paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.0.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self) -> ActivityStats {
        ActivityStats {
            txs_sent: self.0.txs_sent.load(Ordering::Relaxed),
            deposits_sent: self.0.deposits_sent.load(Ordering::Relaxed),
            failed: self.0.failed.load(Ordering::Relaxed),
            paused: self.0.paused.load(Ordering::Relaxed),
        }
    }
}

/// Handle to running background activity on a chain. Drop or `stop().await`
/// to halt it.
pub struct ActivityHandle {
    state: ActivityState,
    activity_started: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl ActivityHandle {
    pub(crate) fn new(
        state: ActivityState,
        activity_started: Arc<AtomicBool>,
        tasks: Vec<JoinHandle<()>>,
    ) -> Self {
        Self {
            state,
            activity_started,
            tasks,
        }
    }

    /// Stop new submissions. An in-flight L1→L2 deposit submitted just before
    /// `pause()` may still arrive on L2 — do not rely on this for exact quiescing.
    pub fn pause(&self) {
        self.state.set_paused(true);
    }

    /// Resume after a `pause()`.
    pub fn resume(&self) {
        self.state.set_paused(false);
    }

    /// Current activity counters and flags. See [`ActivityStats`] — this is a
    /// near-snapshot (independent relaxed loads), not one atomic read.
    pub fn stats(&self) -> ActivityStats {
        self.state.snapshot()
    }

    /// Gracefully stop all background tasks, await their cancellation, and clear
    /// the chain's start guard. Only `stop().await` is restart-safe — `Drop`
    /// aborts without awaiting.
    pub async fn stop(mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
        self.activity_started.store(false, Ordering::Relaxed);
    }
}

impl Drop for ActivityHandle {
    fn drop(&mut self) {
        // Teardown only: abort without awaiting. The old task may still be
        // unwinding when the guard clears, so this is not restart-safe — callers
        // that restart must use stop().await.
        for task in &self.tasks {
            task.abort();
        }
        self.activity_started.store(false, Ordering::Relaxed);
    }
}

/// Next round-robin index. `len` must be non-zero.
pub(crate) fn next_index(prev: usize, len: usize) -> usize {
    (prev + 1) % len
}

/// Sleep until `deadline`, or return immediately if it has already passed.
async fn sleep_until(deadline: std::time::Instant) {
    let now = std::time::Instant::now();
    if deadline > now {
        tokio::time::sleep(deadline - now).await;
    }
}

/// L2 self-transfer loop. Round-robins across `signers`; each signer sends a
/// 1-wei self-transfer, so every signer has its own nonce sequence (no mutex).
pub(crate) async fn run_l2_transfers(
    l2_rpc: String,
    signers: Vec<PrivateKeySigner>,
    interval: Duration,
    state: ActivityState,
) {
    let mut idx = 0usize;
    let mut consecutive_errors = 0u32;
    loop {
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            state.mark_failed();
            eprintln!("[activity] l2_transfers gave up after {MAX_CONSECUTIVE_ERRORS} errors");
            return;
        }
        if state.is_paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let tick_start = std::time::Instant::now();
        let signer = signers[idx].clone();
        match send_self_transfer(&l2_rpc, &signer).await {
            Ok(()) => {
                state.record_tx();
                consecutive_errors = 0;
                idx = next_index(idx, signers.len());
                sleep_until(tick_start + interval).await;
            }
            Err(e) => {
                eprintln!("[activity] l2 transfer error: {e:#}");
                consecutive_errors += 1;
                tokio::time::sleep(interval.min(CRASH_RESTART_DELAY)).await;
            }
        }
    }
}

async fn send_self_transfer(l2_rpc: &str, signer: &PrivateKeySigner) -> anyhow::Result<()> {
    let addr = signer.address();
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect(l2_rpc)
        .await?;
    let tx = TransactionRequest::default()
        .with_to(addr)
        .with_value(U256::from(1u64));
    // Submit to the mempool; we count the accepted hash, not mining (the
    // PendingTransactionBuilder is intentionally dropped without watching).
    let _hash = *provider.send_transaction(tx).await?.tx_hash();
    Ok(())
}

/// L1→L2 deposit loop. Uses one dedicated `depositor_sk` for this chain, so its
/// L1 nonce sequence is never shared with another chain's loop.
pub(crate) async fn run_l1_deposits(
    l1_rpc: String,
    bridgehub: Address,
    chain_id: u64,
    depositor_sk: String,
    interval: Duration,
    state: ActivityState,
) {
    let recipient = match depositor_sk.parse::<PrivateKeySigner>() {
        Ok(s) => s.address(),
        Err(e) => {
            eprintln!("[activity] bad depositor key: {e}");
            state.mark_failed();
            return;
        }
    };
    let amount = U256::from(ACTIVITY_DEPOSIT_AMOUNT_WEI);
    let mut consecutive_errors = 0u32;
    loop {
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            state.mark_failed();
            eprintln!("[activity] l1_deposits gave up after {MAX_CONSECUTIVE_ERRORS} errors");
            return;
        }
        if state.is_paused() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let tick_start = std::time::Instant::now();
        match deposit_eth(
            &l1_rpc,
            bridgehub,
            chain_id,
            recipient,
            amount,
            DEFAULT_L1_TO_L2_GAS_PRICE,
            &depositor_sk,
        )
        .await
        {
            Ok(_) => {
                state.record_deposit();
                consecutive_errors = 0;
                sleep_until(tick_start + interval).await;
            }
            Err(e) => {
                eprintln!("[activity] l1 deposit error: {e:#}");
                consecutive_errors += 1;
                tokio::time::sleep(interval.min(CRASH_RESTART_DELAY)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn config_default_enables_both_flows() {
        let c = ActivityConfig::default();
        assert_eq!(c.l2_transfers, Some(Duration::from_millis(500)));
        assert_eq!(c.l1_deposits, Some(Duration::from_secs(5)));
    }

    #[test]
    fn transfers_only_disables_deposits() {
        let c = ActivityConfig::transfers_only();
        assert!(c.l2_transfers.is_some());
        assert_eq!(c.l1_deposits, None);
    }

    #[test]
    fn deposits_only_disables_transfers() {
        let c = ActivityConfig::deposits_only();
        assert_eq!(c.l2_transfers, None);
        assert!(c.l1_deposits.is_some());
    }

    #[test]
    fn key_pool_has_ten_entries() {
        assert_eq!(ACTIVITY_WALLET_KEYS.len(), 10);
    }

    #[test]
    fn next_index_wraps() {
        assert_eq!(next_index(0, 3), 1);
        assert_eq!(next_index(1, 3), 2);
        assert_eq!(next_index(2, 3), 0);
    }

    #[test]
    fn next_index_single_element_stays() {
        assert_eq!(next_index(0, 1), 0);
    }
}
