/// Moving a validium chain to logs-only data availability, the way a testnet
/// operator would.
///
/// A chain's DA setup is two orthogonal settings (see [`tests::da`]): the
/// *mechanism* — `(l1DAValidator, L2DACommitmentScheme)` — and the *scope*,
/// `PubdataContent`. An old-style validium is `EmptyNoDA` + `FULL_PUBDATA`.
/// Logs-only narrows the scope to `LOGS_ONLY`: only the mandatory L2->L1 log
/// region is committed, which is what keeps an atomic-interop chain's IMT
/// leaves reconstructible from L1 without paying for full-state DA. The scope
/// is folded into the ZKsync OS batch public input via the chain-config hash,
/// so the settlement layer enforces it rather than trusting the operator.
///
/// Both setters are `onlyAdmin`, i.e. they go through the chain's ChainAdmin,
/// and `setPubdataContent` additionally reverts while committed-but-unverified
/// batches exist — those ran under the old chain config and would become
/// unprovable — so the operator has to quiesce the chain first.
///
/// HAZARD, and the reason this test stops at the switch: the server always runs
/// its batches with `ChainConfig.pubdata_content = FullPubdata`
/// (`native_pig::v32_chain_config` derives the config from the chain id alone),
/// so once L1 says `LOGS_ONLY` the two chain-config hashes disagree and
/// `proveBatches` reverts with `InvalidProof()` — the chain wedges at the first
/// batch proven after the switch. Flipping the scope is therefore only safe once
/// the server derives `pubdata_content` from the chain's L1 configuration; until
/// then this test covers the operator-facing half (the ChainAdmin path, the
/// quiesce guard, and the resulting L1 state) and deliberately does not drive
/// post-switch traffic. Add that assertion — and the interop-leaves-on-L1 one —
/// when the server follows the scope.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;

use tests::da::{self, L2DACommitmentScheme, PubdataContent, PubdataPricingMode};
use tests::fixtures::validium_ecosystem;
use tests::Ecosystem;

/// Quiescing waits for the prove pipeline, which on a cold chain is a full
/// commit/prove/execute round.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(300);

#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn validium_switches_to_logs_only_da(#[future] validium_ecosystem: Ecosystem) -> Result<()> {
    let eco = validium_ecosystem.await;
    let chain_id = eco.chain().chain_id();
    let bridgehub = eco.chain().bridgehub_addr();
    let l1_rpc = eco.chain().l1_rpc_url().to_string();
    let workdir: PathBuf = eco.workdir().to_path_buf();

    let no_da_validator = eco
        .deployed()
        .context("the validium fixture must be a fresh deployment")?
        .no_da_l1_validator();
    let admin_key = da::chain_admin_owner_key(&workdir, chain_id)?;

    // ── The chain starts as an old-style validium ────────────────────────────
    let (l1_validator, scheme) = da::da_validator_pair(&l1_rpc, bridgehub, chain_id).await?;
    anyhow::ensure!(
        l1_validator == no_da_validator && scheme == L2DACommitmentScheme::EmptyNoDa as u8,
        "expected the no-DA pair, got ({l1_validator:#x}, scheme {scheme})"
    );
    anyhow::ensure!(
        da::pubdata_content(&l1_rpc, bridgehub, chain_id).await?
            == PubdataContent::FullPubdata as u8,
        "a freshly deployed chain must start on FULL_PUBDATA"
    );
    anyhow::ensure!(
        da::pubdata_pricing_mode(&l1_rpc, bridgehub, chain_id).await?
            == PubdataPricingMode::Validium as u8,
        "a no-DA chain must start on validium pubdata pricing"
    );

    // Traffic under the old setup, so the switch happens on a chain with real
    // committed history rather than on genesis.
    let hash = eco.chain().ping().await?;
    eco.chain().wait_for_tx_finalized(hash).await?;

    // ── Quiesce, then narrow the committed scope ─────────────────────────────
    da::wait_for_no_unverified_batches(&l1_rpc, bridgehub, chain_id, QUIESCE_TIMEOUT)
        .await
        .context("wait for the chain to have no unverified batches")?;
    da::set_pubdata_content(
        &l1_rpc,
        bridgehub,
        chain_id,
        PubdataContent::LogsOnly,
        &admin_key,
    )
    .await
    .context("switch the pubdata content to LOGS_ONLY")?;

    anyhow::ensure!(
        da::pubdata_content(&l1_rpc, bridgehub, chain_id).await? == PubdataContent::LogsOnly as u8,
        "pubdata content did not switch to LOGS_ONLY"
    );
    // The mechanism is untouched: logs-only is a scope change, and the pair the
    // chain commits with has to keep matching what the committer stores.
    let (l1_validator, scheme) = da::da_validator_pair(&l1_rpc, bridgehub, chain_id).await?;
    anyhow::ensure!(
        l1_validator == no_da_validator && scheme == L2DACommitmentScheme::EmptyNoDa as u8,
        "the DA pair must not move with the scope"
    );

    Ok(())
}

/// The other half of a DA migration: moving the *mechanism* off no-DA, so the
/// chain actually publishes its (now logs-only) pubdata. The runbook is
/// `setPubdataPricingMode` -> `setDAValidatorPair` -> quiesce ->
/// `setPubdataContent` -> restart the server in the matching pubdata mode; the
/// pair change is what stalls commits (the committer rejects any batch whose
/// scheme differs from the stored one), which is also what makes the quiesce
/// converge.
///
/// Ignored: the restarted server rebuilds the last committed batch under its new
/// pubdata mode and panics on the mismatch —
/// `Rebuilt batch info does not match stored batch info for batch N` — so a live
/// chain cannot change its pubdata mode today. Un-ignore once the server tracks
/// the mode per batch (or otherwise tolerates a mode boundary); the test is
/// written against the intended behaviour.
#[ignore = "server cannot change pubdata mode on a live chain: restart rebuilds the last batch under the new mode"]
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn validium_switches_to_calldata_da(#[future] validium_ecosystem: Ecosystem) -> Result<()> {
    // `chain set-da-validator-pair` runs a forge script.
    tests::fixtures::ensure_contracts_built().await;

    let mut eco = validium_ecosystem.await;
    let chain_id = eco.chain().chain_id();
    let bridgehub = eco.chain().bridgehub_addr();
    let l1_rpc = eco.chain().l1_rpc_url().to_string();
    let workdir: PathBuf = eco.workdir().to_path_buf();

    let calldata_validator = eco
        .deployed()
        .context("the validium fixture must be a fresh deployment")?
        .rollup_l1_da_validator();
    let admin_key = da::chain_admin_owner_key(&workdir, chain_id)?;

    let hash = eco.chain().ping().await?;
    eco.chain().wait_for_tx_finalized(hash).await?;

    // Pricing first: the server refuses to boot while its pubdata mode
    // disagrees with the chain's pricing mode.
    da::set_pubdata_pricing_mode(
        &l1_rpc,
        bridgehub,
        chain_id,
        PubdataPricingMode::Rollup,
        &admin_key,
    )
    .await
    .context("switch the pubdata pricing mode")?;

    da::set_da_validator_pair(
        &l1_rpc,
        &workdir,
        bridgehub,
        chain_id,
        calldata_validator,
        protocol_ops::types::L2DACommitmentScheme::BlobsAndPubdataKeccak256,
        &[&admin_key],
    )
    .await
    .context("switch the DA validator pair to calldata")?;

    da::wait_for_no_unverified_batches(&l1_rpc, bridgehub, chain_id, QUIESCE_TIMEOUT)
        .await
        .context("wait for the chain to have no unverified batches")?;
    da::set_pubdata_content(
        &l1_rpc,
        bridgehub,
        chain_id,
        PubdataContent::LogsOnly,
        &admin_key,
    )
    .await
    .context("switch the pubdata content to LOGS_ONLY")?;

    eco.restart_chain_with_config(chain_id, "l1_sender:\n  pubdata_mode: Calldata\n")
        .await
        .context("restart the server in the new pubdata mode")?;

    let hash = eco.chain().ping().await?;
    eco.chain()
        .wait_for_tx_finalized(hash)
        .await
        .context("post-switch batch must commit, prove and execute")?;

    Ok(())
}
