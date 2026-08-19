//! v31→v32 protocol upgrade steps.
//!
//! Each function here is a *real* step of the upgrade runbook (the fixture is
//! restored as-is; no state mending is needed). The flow, in order:
//!
//! 1. `ecosystem upgrade-prepare-all` (deployer) — deploy the v32 ecosystem
//!    contracts, among them the `PriorityOpLowerBound` registry and the
//!    `V32UpgradeZKsyncOS` upgrade contract
//! 2. `ecosystem upgrade-governance` (governor) — governance stages 0+1+2
//! 3. [`record_priority_op_lower_bound`] — pin the chain's priority-op count,
//!    then [`wait_for_priority_ops_processed`]; `V32UpgradeZKsyncOS` rejects
//!    the diamond cut until every op below the pin has been processed on L2
//! 4. [`schedule_upgrade_timestamp`] — notify ChainAdmin + ServerNotifier; the
//!    server then injects the L2 upgrade tx and its upgrade_gatekeeper holds
//!    v32 batches until the L1 chain upgrade lands
//! 5. [`run_chain_upgrade`] — diamond cut, L1 protocolVersion → v32
//!
//! Steps 1, 2, 4 and 5 go through protocol-ops commands and [`apply`]; step 3
//! is a direct L1 call (see [`record_priority_op_lower_bound`]).
//!
//! Unlike v30→v31 there is no stage-3 token migration and no base-token supply
//! backfill: a chain created on v31 gets `baseTokenHasTotalSupply` from
//! `DiamondInit`, which is what the v32 upgrade checks.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use anyhow::{Context, Result};
use protocol_ops::common::abi::ZkChainAbi;
use serde::Deserialize;

use crate::eth::{call, provider, send_as_signer};
pub use crate::upgrade::{
    apply, assert_protocol_version, chain_args, ecosystem_args, run_chain_upgrade,
    schedule_upgrade_timestamp, shared_args,
};

alloy::sol! {
    /// Standalone registry the v32 upgrade reads the per-chain priority-op
    /// bound from. Local because protocol-ops has no command wrapping
    /// `RecordPriorityOpLowerBound.s.sol` yet.
    /// TODO(protocol-ops): drop in favour of `protocol_ops::common::abi` once
    /// the registry gets one.
    interface IPriorityOpLowerBound {
        function lowerBoundPriorityOp(address chain) external;
        function lowerBound(address chain) external view returns (uint256);
        function recorded(address chain) external view returns (bool);
    }
}

/// The slice of the CTM-side prepare output this test reads. `DefaultCTMUpgrade`
/// serializes the deployed registry under `[state_transition]`.
#[derive(Deserialize)]
struct CtmUpgradeOutput {
    state_transition: CtmStateTransition,
}

#[derive(Deserialize)]
struct CtmStateTransition {
    priority_op_lower_bound_addr: Address,
}

/// Where `ecosystem upgrade-prepare-all` writes the per-CTM output
/// (`v31_upgrade_inner.rs` builds the same path).
fn ctm_upgrade_output_path(ctm: Address) -> PathBuf {
    protocol_ops::common::paths::contracts_root()
        .join("l1-contracts")
        .join("script-out")
        .join(format!("v31-upgrade-ctm-{ctm:#x}.toml"))
}

/// Read the `PriorityOpLowerBound` registry address out of the CTM prepare
/// output.
pub fn priority_op_lower_bound_registry(ctm: Address) -> Result<Address> {
    let path = ctm_upgrade_output_path(ctm);
    let output: CtmUpgradeOutput = protocol_ops::common::files::read_toml_file(&path)
        .with_context(|| format!("read CTM upgrade output {}", path.display()))?;
    Ok(output.state_transition.priority_op_lower_bound_addr)
}

/// Pin the chain's priority-op count in the registry (`lowerBoundPriorityOp`).
///
/// `V32UpgradeZKsyncOS` requires a recorded bound plus every priority op below
/// it processed, which together prove the v31 base-token supply backfill
/// executed on L2 before v32 removes its entry point. The call is
/// permissionless and idempotent — the same contract call
/// `RecordPriorityOpLowerBound.s.sol` broadcasts.
///
/// It must land in its own transaction, well before the diamond cut: the
/// upgrade reads the bound before the chain's facets are replaced.
///
/// TODO(protocol-ops): replace with a command wrapping the era-contracts script.
pub async fn record_priority_op_lower_bound(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    ctm: Address,
    sender_key: &str,
) -> Result<()> {
    let registry = priority_op_lower_bound_registry(ctm)?;
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;

    if !call(
        &provider,
        registry,
        IPriorityOpLowerBound::recordedCall { chain: diamond },
    )
    .await?
    {
        send_as_signer(
            l1_rpc,
            sender_key,
            registry,
            IPriorityOpLowerBound::lowerBoundPriorityOpCall { chain: diamond },
        )
        .await
        .context("PriorityOpLowerBound.lowerBoundPriorityOp")?;
    }
    Ok(())
}

/// Wait until the chain has processed every priority op below the recorded
/// bound — the second half of the v32 upgrade's precondition. The counter only
/// advances as the batches holding those ops are executed on L1.
pub async fn wait_for_priority_ops_processed(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    ctm: Address,
    timeout: Duration,
) -> Result<()> {
    let registry = priority_op_lower_bound_registry(ctm)?;
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;

    let bound = call(
        &provider,
        registry,
        IPriorityOpLowerBound::lowerBoundCall { chain: diamond },
    )
    .await?;

    let deadline = Instant::now() + timeout;
    loop {
        let processed = call(
            &provider,
            diamond,
            ZkChainAbi::getFirstUnprocessedPriorityTxCall {},
        )
        .await?;
        if processed >= bound {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "priority queue did not drain to the recorded bound in time \
             (processed {processed}, bound {bound})"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
