//! Runbook steps shared by every protocol-upgrade test.
//!
//! The per-version modules ([`crate::upgrade_v30_to_v31`],
//! [`crate::upgrade_v31_to_v32`]) own the steps that only their upgrade needs;
//! everything here — the protocol-ops argument plumbing, the manifest apply,
//! the ChainAdmin/ServerNotifier timestamp and the diamond cut — is identical
//! across versions because the same protocol-ops commands drive them.

use std::path::Path;

use alloy::primitives::Address;
use anyhow::{Context, Result};
use protocol_ops::commands::chain;
use protocol_ops::commands::dev::execute_manifest::apply_manifest;
use protocol_ops::common::abi::{IChainTypeManagerAbi, ZkChainAbi};
use protocol_ops::common::forge::ForgeScriptArgs;
use protocol_ops::common::{EcosystemArgs, EcosystemChainArgs, SharedRunArgs};

use crate::eth::{call, provider};

// ---------------------------------------------------------------------------
// protocol-ops invocation glue
// ---------------------------------------------------------------------------

pub fn shared_args(l1_rpc: &str, out_dir: &Path) -> SharedRunArgs {
    SharedRunArgs {
        l1_rpc_url: l1_rpc.to_string(),
        out: Some(out_dir.to_path_buf()),
        // The upgrade scripts (CoreUpgrade_v31 etc.) don't have path-taking
        // entrypoints yet, so per-run IO scoping isn't available on the
        // upgrade path.
        subdir: None,
        forge_args: ForgeScriptArgs::default(),
    }
}

pub fn ecosystem_args(bridgehub: Address) -> EcosystemArgs {
    EcosystemArgs {
        bridgehub: Some(bridgehub),
        env: None,
    }
}

pub fn chain_args(bridgehub: Address, chain_id: u64) -> EcosystemChainArgs {
    EcosystemChainArgs {
        ecosystem: ecosystem_args(bridgehub),
        chain_id,
    }
}

/// Apply the manifest a protocol-ops command wrote to `out_dir` with `keys`.
pub async fn apply(out_dir: &Path, keys: &[&str], l1_rpc: &str) -> Result<()> {
    let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    apply_manifest(&out_dir.join("manifest.json"), &keys, None, l1_rpc, true)
        .await
        .with_context(|| format!("apply manifest from {}", out_dir.display()))
}

// ---------------------------------------------------------------------------
// Upgrade steps
// ---------------------------------------------------------------------------

/// Schedule the upgrade timestamp via `chain set-upgrade-timestamp`. The
/// command's AdminFunctions script notifies both ChainAdmin and the
/// ServerNotifier; the latter's UpgradeTimestampUpdated event is what the
/// server's L1UpgradeTxWatcher reacts to. The timestamp is set in the past
/// so the watcher fires immediately.
/// `keys` must cover every signer the emitted bundle targets — the ChainAdmin
/// owner, which on some ecosystems differs from the governance owner.
pub async fn schedule_upgrade_timestamp(
    l1_rpc: &str,
    workdir: &Path,
    keys: &[&str],
    bridgehub: Address,
    chain_id: u64,
) -> Result<()> {
    let provider = provider(l1_rpc).await?;

    let upgrade_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(60);

    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve CTM")?;
    // Liveness check that the CTM proxy resolves; the target protocol version is now derived
    // internally by `set-upgrade-timestamp` (the era-contracts command dropped its explicit arg).
    let _target_pv = call(&provider, ctm, IChainTypeManagerAbi::protocolVersionCall {}).await?;

    let out_dir = workdir.join("schedule_upgrade");
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    // AdminFunctions.s.sol::adminScheduleUpgrade multicalls BOTH
    // ChainAdmin.setUpgradeTimestamp and ServerNotifier.setUpgradeTimestamp,
    // so this single command is what triggers the server's L1UpgradeTxWatcher.
    chain::set_upgrade_timestamp::run(chain::set_upgrade_timestamp::ChainSetUpgradeTimestampArgs {
        topology: chain_args(bridgehub, chain_id),
        access_control_restriction: Address::ZERO,
        upgrade_timestamp: upgrade_timestamp.to_string(),
        shared: shared_args(l1_rpc, &out_dir),
    })
    .await
    .context("chain set-upgrade-timestamp")?;
    apply(&out_dir, keys, l1_rpc)
        .await
        .context("apply set-upgrade-timestamp")?;

    Ok(())
}

/// Run the L1 chain upgrade (diamond cut via `chain upgrade` + apply). This
/// bumps the chain diamond's protocolVersion to the CTM's current version,
/// unblocking the server's upgrade_gatekeeper.
/// `keys` must cover every signer the emitted bundle targets (see
/// [`schedule_upgrade_timestamp`]).
pub async fn run_chain_upgrade(
    l1_rpc: &str,
    workdir: &Path,
    keys: &[&str],
    bridgehub: Address,
    chain_id: u64,
) -> Result<()> {
    let out_dir = workdir.join("chain_upgrade");
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    chain::upgrade::run(chain::upgrade::ChainUpgradeArgs {
        topology: ecosystem_args(bridgehub),
        chain_id: Some(chain_id),
        access_control_restriction: Address::ZERO,
        shared: shared_args(l1_rpc, &out_dir),
    })
    .await
    .context("chain upgrade")?;
    apply(&out_dir, keys, l1_rpc)
        .await
        .context("apply chain upgrade")?;

    Ok(())
}

/// Assert the chain diamond's packed protocol version has the expected major.
pub async fn assert_protocol_version(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    expected_major: u64,
) -> Result<()> {
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;
    let packed = call(&provider, diamond, ZkChainAbi::getProtocolVersionCall {}).await?;
    let major = (packed.wrapping_to::<u64>() >> 32) & 0xFFFF;
    anyhow::ensure!(
        major == expected_major,
        "expected protocol version {expected_major}, got {major}"
    );
    Ok(())
}
