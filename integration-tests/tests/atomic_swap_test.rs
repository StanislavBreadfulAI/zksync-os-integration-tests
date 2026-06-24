//! End-to-end atomic-interop swap between two L1-settling chains (no gateway in the path).
//!
//! Boots the two L1-settling servers (l1_settling / l1_settling_b) against the cached L1 state, then
//! runs the TypeScript driver `atomic-swap-real.ts` (in the era-contracts anvil-interop tree) against
//! their live RPCs. The driver deploys + registers a test ERC20 on each chain, performs the atomic
//! send on both legs (burn + IMT insert), waits for each leg's commitment-tree root to settle on L1,
//! fetches the REAL message proof via `zks_getL2ToL1LogProof` (the L1 aggregation-hop proof from
//! zksync-os-server branch kl/l1-settled-interop-proof), reconstructs the IMT inclusion proofs, and
//! calls `InteropHandler.executeAtomicBundle` on each destination — asserting both mints land and the
//! source legs stay Committed.
//!
//! The flow logic + ABI encoding is reused from the proven anvil-interop TS helpers; only the message
//! proof differs (real RPC vs the anvil mock).

use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{load_ecosystem, resolve_l1_state};
use integration_tests::presets::{load_current_preset, RepoRef};
use integration_tests::server::ServerBuilder;
use std::process::Command;

async fn run_atomic_swap_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("atomic_swap");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    println!("Anvil ready at {}", anvil.rpc_url());

    let (a_name, a_id) = eco.l1_settling();
    let (b_name, b_id) = eco.l1_settling_b();
    println!("Chain A (L1-settling): {a_id} ({a_name})");
    println!("Chain B (L1-settling): {b_id} ({b_name})");

    println!("\n=== Starting both L1-settling servers ===");
    let server_a = ServerBuilder::new(preset.clone(), a_name)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain A server: {:?}", e))?;
    let rpc_a = server_a.rpc_url().to_string();
    println!("Chain A ready at {rpc_a}");

    let server_b = ServerBuilder::new(preset.clone(), b_name)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain B server: {:?}", e))?;
    let rpc_b = server_b.rpc_url().to_string();
    println!("Chain B ready at {rpc_b}");

    // Resolve the era-contracts anvil-interop directory (local-checkout preset only).
    let era_path = match &preset.era_contracts {
        RepoRef::Path(p) => p.clone(),
        _ => anyhow::bail!("atomic_swap_test requires a local-path era_contracts preset"),
    };
    let anvil_interop_dir = era_path.join("l1-contracts/test/anvil-interop");
    anyhow::ensure!(
        anvil_interop_dir.join("atomic-swap-real.ts").exists(),
        "driver not found at {}",
        anvil_interop_dir.display()
    );

    println!("\n=== Running atomic-swap-real.ts driver ===");
    let status = Command::new("yarn")
        .args([
            "ts-node",
            "atomic-swap-real.ts",
            &rpc_a,
            &a_id.to_string(),
            &rpc_b,
            &b_id.to_string(),
        ])
        .current_dir(&anvil_interop_dir)
        .status()
        .context("spawn atomic-swap-real.ts")?;

    let _ = server_a.kill();
    let _ = server_b.kill();
    let _ = anvil.kill();

    anyhow::ensure!(status.success(), "atomic-swap-real.ts driver failed");
    println!("\nAtomic swap test passed!");
    Ok(())
}

#[tokio::test]
async fn test_atomic_swap_l1_settled() {
    run_atomic_swap_test()
        .await
        .expect("atomic_swap_test failed");
}
