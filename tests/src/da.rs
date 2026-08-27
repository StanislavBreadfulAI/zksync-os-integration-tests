//! Data-availability reconfiguration steps.
//!
//! A chain's DA setup is two orthogonal settings in diamond storage:
//!
//! - the **mechanism** — `(l1DAValidator, L2DACommitmentScheme)`, set together
//!   by `Admin.setDAValidatorPair` because the L1 validator expects a specific
//!   L2 output. The committer rejects any batch whose scheme does not match the
//!   stored one, so the server must speak the new scheme from the moment the
//!   pair changes;
//! - the **scope** — `PubdataContent`, set by `Admin.setPubdataContent`:
//!   `FULL_PUBDATA` commits the whole pubdata, `LOGS_ONLY` only the mandatory
//!   L2->L1 log region (which is what keeps an atomic-interop chain's IMT
//!   leaves reconstructible from L1 without paying for full-state DA). It is
//!   folded into the ZKsync OS batch public input via the chain-config hash.
//!
//! Both are `onlyAdmin`, i.e. they go through the chain's ChainAdmin, and both
//! are rejected while committed-but-unverified batches exist — those batches
//! were executed under the old config and would become unprovable.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use protocol_ops::common::abi::{IChainAdminAbi, ZkChainAbi};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::eth::{call, provider, send_as_signer};
use crate::upgrade::{apply, chain_args, shared_args};

/// `L2DACommitmentScheme` (`system-contracts/contracts/Constants.sol`), as the
/// `uint8` the diamond stores and the ABI exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum L2DACommitmentScheme {
    EmptyNoDa = 1,
    PubdataKeccak256 = 2,
    BlobsAndPubdataKeccak256 = 3,
    BlobsZKsyncOS = 4,
}

/// `PubdataPricingMode` (`l1-contracts/contracts/common/Config.sol`). The
/// server refuses to start when its pubdata mode disagrees with this, so a
/// mechanism change has to move it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PubdataPricingMode {
    Rollup = 0,
    Validium = 1,
}

/// `PubdataContent` (`system-contracts/contracts/Constants.sol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PubdataContent {
    FullPubdata = 0,
    LogsOnly = 1,
}

/// The chain's current `(l1DAValidator, l2DACommitmentScheme)`.
pub async fn da_validator_pair(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
) -> Result<(Address, u8)> {
    let diamond = resolve_diamond(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;
    let pair = call(&provider, diamond, ZkChainAbi::getDAValidatorPairCall {}).await?;
    Ok((pair._0, pair._1))
}

/// The chain's current `PubdataContent`.
pub async fn pubdata_content(l1_rpc: &str, bridgehub: Address, chain_id: u64) -> Result<u8> {
    let diamond = resolve_diamond(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;
    call(&provider, diamond, ZkChainAbi::getPubdataContentCall {}).await
}

/// The chain's current `PubdataPricingMode`.
pub async fn pubdata_pricing_mode(l1_rpc: &str, bridgehub: Address, chain_id: u64) -> Result<u8> {
    let diamond = resolve_diamond(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;
    call(&provider, diamond, ZkChainAbi::getPubdataPricingModeCall {}).await
}

/// Wait until every committed batch has been verified.
///
/// Both DA setters revert with `ZKsyncOSChainConfigUpdateWithUnverifiedBatches`
/// while the two counters disagree, so an operator has to quiesce the chain
/// before reconfiguring it — that wait is part of the runbook, not a test hack.
pub async fn wait_for_no_unverified_batches(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    timeout: Duration,
) -> Result<()> {
    let diamond = resolve_diamond(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;

    let deadline = Instant::now() + timeout;
    loop {
        let committed = call(
            &provider,
            diamond,
            ZkChainAbi::getTotalBatchesCommittedCall {},
        )
        .await?;
        let verified = call(
            &provider,
            diamond,
            ZkChainAbi::getTotalBatchesVerifiedCall {},
        )
        .await?;
        if committed == verified {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "chain still has unverified batches (committed {committed}, verified {verified})"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Set the chain's DA validator pair through `chain set-da-validator-pair`,
/// which drives `AdminFunctions.s.sol` and emits a ChainAdmin bundle.
pub async fn set_da_validator_pair(
    l1_rpc: &str,
    workdir: &Path,
    bridgehub: Address,
    chain_id: u64,
    l1_da_validator: Address,
    scheme: protocol_ops::types::L2DACommitmentScheme,
    keys: &[&str],
) -> Result<()> {
    let out_dir = workdir.join("set_da_validator_pair");
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    protocol_ops::commands::chain::set_da_validator_pair::run(
        protocol_ops::commands::chain::set_da_validator_pair::ChainSetDaValidatorPairArgs {
            topology: chain_args(bridgehub, chain_id),
            access_control_restriction: Address::ZERO,
            l1_da_validator,
            l2_da_commitment_scheme: scheme,
            shared: shared_args(l1_rpc, &out_dir),
        },
    )
    .await
    .context("chain set-da-validator-pair")?;
    apply(&out_dir, keys, l1_rpc)
        .await
        .context("apply set-da-validator-pair")
}

/// Set the chain's `PubdataContent` through `ChainAdmin.multicall`, signed by
/// the ChainAdmin's owner — the path `AdminFunctions.s.sol` uses for every
/// other `onlyAdmin` setter.
///
/// TODO(protocol-ops): replace with a `chain set-pubdata-content` command once
/// `AdminFunctions.s.sol` grows the entry point (it has one for the DA pair
/// but not yet for the scope).
pub async fn set_pubdata_content(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    content: PubdataContent,
    chain_admin_owner_key: &str,
) -> Result<()> {
    admin_call(
        l1_rpc,
        bridgehub,
        chain_id,
        ZkChainAbi::setPubdataContentCall {
            _pubdataContent: content as u8,
        }
        .abi_encode(),
        chain_admin_owner_key,
    )
    .await
    .context("ChainAdmin.multicall(setPubdataContent)")
}

/// Set the chain's `PubdataPricingMode`, the same way.
pub async fn set_pubdata_pricing_mode(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    mode: PubdataPricingMode,
    chain_admin_owner_key: &str,
) -> Result<()> {
    admin_call(
        l1_rpc,
        bridgehub,
        chain_id,
        ZkChainAbi::setPubdataPricingModeCall {
            _pricingMode: mode as u8,
        }
        .abi_encode(),
        chain_admin_owner_key,
    )
    .await
    .context("ChainAdmin.multicall(setPubdataPricingMode)")
}

/// Send one `onlyAdmin` diamond call through `ChainAdmin.multicall`.
async fn admin_call(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    calldata: Vec<u8>,
    chain_admin_owner_key: &str,
) -> Result<()> {
    let diamond = resolve_diamond(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;
    let chain_admin = call(&provider, diamond, ZkChainAbi::getAdminCall {}).await?;

    send_as_signer(
        l1_rpc,
        chain_admin_owner_key,
        chain_admin,
        IChainAdminAbi::multicallCall {
            _calls: vec![IChainAdminAbi::Call {
                target: diamond,
                value: U256::ZERO,
                data: Bytes::from(calldata),
            }],
            _requireSuccess: true,
        },
    )
    .await
}

/// The private key of the chain's ChainAdmin owner, from the deployment's
/// `wallets.yaml` (chain `owner`). Every `onlyAdmin` setter is signed with it.
pub fn chain_admin_owner_key(workdir: &Path, chain_id: u64) -> Result<String> {
    let path = workdir.join("ecosystem").join("wallets.yaml");
    let wallets = protocol_ops::common::wallets::load_wallets(&path)
        .with_context(|| format!("load wallets from {}", path.display()))?;
    let chain = wallets
        .chains
        .get(&chain_id.to_string())
        .with_context(|| format!("chain {chain_id} not found in {}", path.display()))?;
    let key = chain
        .owner
        .private_key
        .as_ref()
        .with_context(|| format!("chain {chain_id} owner has no private key"))?;
    Ok(format!("0x{}", hex::encode(key.to_bytes())))
}

async fn resolve_diamond(l1_rpc: &str, bridgehub: Address, chain_id: u64) -> Result<Address> {
    protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")
}
