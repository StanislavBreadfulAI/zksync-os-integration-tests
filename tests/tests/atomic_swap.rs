//! End-to-end atomic-interop swap between two L1-settling chains (no gateway in the path).
//!
//! The `ecosystem` fixture with `#[with(vec![6565, 6566])]` brings up two L1-settling ZKsync OS
//! chains on one Anvil L1, each with its own in-process server. We then drive the bundle-model atomic
//! swap between them via the era-contracts anvil-interop TS driver `atomic-swap-real.ts` (reuses the
//! proven bundle/IMT helpers; fetches the real `zks_getL2ToL1LogProof` proof — the L1 aggregation-hop
//! proof from zksync-os-server branch `kl/l1-settled-interop-proof`).
//!
//! The driver deploys + registers a test ERC20 on each chain, performs the atomic send on both legs
//! (burn + IMT insert), waits for each leg's commitment-tree root to settle on L1, reconstructs the
//! IMT inclusion proofs, and calls `InteropHandler.executeAtomicBundle` per leg — asserting both mints
//! land and the source legs stay Committed.
//!
//! Requirements:
//! - `PROTOCOL_CONTRACTS_ROOT` must point at the era-contracts `atomic-imt-interop` checkout, so
//!   `zk-deployer` deploys the atomic genesis contracts + relaxed gateway-mode guards, and the driver
//!   is found under `l1-contracts/test/anvil-interop/`.
//! - The zksync-os-server local-path override must point at a build of the
//!   `kl/l1-settled-interop-proof` branch (for the L1 aggregation-hop proof).

use alloy::hex;
use anyhow::Result;
use rstest::rstest;
use std::path::PathBuf;
use std::process::Command;
use tests::fixtures::ecosystem;
use tests::Ecosystem;

// Ignored in CI: this test needs the atomic-interop contracts (era-contracts `atomic-imt-interop`)
// and the L1-interop server changes (zksync-os-server `kl/l1-settled-interop-proof`: #8 proof +
// interop-watcher/block-production on L1), none of which are on the pinned refs CI builds (main
// server / draft-v31 contracts). Until those land upstream, run it locally:
//   1. point the deps at the local checkouts via the `[patch]` block in the workspace Cargo.toml,
//   2. `PROTOCOL_CONTRACTS_ROOT=/abs/path/to/era-contracts \
//        cargo test -p tests --release --test atomic_swap -- --ignored --nocapture`.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PROTOCOL_CONTRACTS_ROOT + local server/era-contracts atomic-interop patches; run manually"]
async fn atomic_swap_l1_settled(
    #[future]
    #[with(vec![6565, 6566])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chains: Vec<_> = eco.chains().collect();
    let (a, b) = (chains[0], chains[1]);

    // The #18 fixture funds its own WALLET_KEYS (not necessarily the driver's default anvil key), so
    // pass a known-funded wallet's private key through to the driver as the depositor/recipient.
    let funded_key = format!("0x{}", hex::encode(a.wallet(0).to_bytes()));

    let era_root = PathBuf::from(std::env::var("PROTOCOL_CONTRACTS_ROOT").expect(
        "PROTOCOL_CONTRACTS_ROOT must point at the era-contracts atomic-imt-interop checkout",
    ));
    let driver_dir = era_root.join("l1-contracts/test/anvil-interop");
    anyhow::ensure!(
        driver_dir.join("atomic-swap-real.ts").exists(),
        "atomic-swap-real.ts driver not found at {}",
        driver_dir.display()
    );

    let status = Command::new("yarn")
        .args([
            "ts-node",
            "atomic-swap-real.ts",
            a.l2_rpc_url(),
            &a.chain_id().to_string(),
            b.l2_rpc_url(),
            &b.chain_id().to_string(),
            &funded_key,
            a.l1_rpc_url(),
            &a.bridgehub_addr().to_string(),
        ])
        .current_dir(&driver_dir)
        .status()?;

    anyhow::ensure!(status.success(), "atomic-swap-real.ts driver failed");
    Ok(())
}
