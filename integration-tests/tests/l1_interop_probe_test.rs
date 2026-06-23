//! Probe: does the message-root interop proof path work when the SOURCE chain
//! settles directly on L1 (no gateway)? This isolates the "atomic interop on
//! L1, without the gateway" question — reviewer comment #8 flags that the
//! proof's block-number extraction may break for L1-settled chains now that the
//! L1 node is final (commit `71bc43441` builds interop roots on L1).
//!
//! It boots only the single L1-settling chain (6565), sends an L2->L1 message,
//! and dumps the RAW `zks_getL2ToL1LogProof` (messageRoot variant) response so
//! we can see whether a proof is produced and what `gateway_block_number` /
//! `root` look like when there is no gateway in the path.

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use alloy::sol;
use anyhow::{Context, Result};
use integration_tests::anvil::{Anvil, DEFAULT_ANVIL_PRIVATE_KEY};
use integration_tests::l1_state::{load_ecosystem, resolve_l1_state};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;
use std::str::FromStr;
use std::time::Duration;

const L1_MESSENGER_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x80, 0x08,
]);

sol! {
    #[sol(rpc)]
    contract IL1Messenger {
        function sendToL1(bytes calldata _message) external returns (bytes32);
    }
}

/// Call `zks_getL2ToL1LogProof` (messageRoot variant) and return the raw JSON.
async fn get_message_root_proof_raw(
    rpc_url: &str,
    tx_hash: FixedBytes<32>,
) -> Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "zks_getL2ToL1LogProof",
        "params": [tx_hash, 0, "messageRoot"]
    });
    let resp = client.post(rpc_url).json(&body).send().await?;
    let json: serde_json::Value = resp.json().await?;
    Ok(json)
}

async fn run_probe() -> Result<()> {
    integration_tests::server::get_or_create_run_id("l1_interop_probe");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    println!("Anvil ready at {}", anvil.rpc_url());

    let (chain_name, chain_id) = eco.l1_settling();
    println!("Source = L1-settling chain {chain_id} ({chain_name})");

    let server = ServerBuilder::new(preset, chain_name)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server: {:?}", e))?;
    let l2_rpc_url = server.rpc_url();
    println!("Server ready at {l2_rpc_url}");

    // ---- Send L2->L1 message ----
    println!("\n=== Sending L2->L1 message on L1-settling chain ===");
    let wallet = EthereumWallet::new(
        LocalSigner::from_str(DEFAULT_ANVIL_PRIVATE_KEY).context("parse test private key")?,
    );
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_builtin(&l2_rpc_url)
        .await
        .context("connect L2 provider")?;
    // generate-l1-state pre-queued an L1->L2 deposit for our test account; wait
    // for the server to process it so the sender has funds for the message tx.
    let test_address = LocalSigner::from_str(DEFAULT_ANVIL_PRIVATE_KEY)?.address();
    println!("  Waiting for test account {test_address} to be funded on L2...");
    let fund_start = tokio::time::Instant::now();
    loop {
        let bal = provider.get_balance(test_address).await.unwrap_or_default();
        if !bal.is_zero() {
            println!("  Funded: balance={bal}");
            break;
        }
        anyhow::ensure!(
            fund_start.elapsed() < Duration::from_secs(120),
            "test account never funded on L1-settling chain"
        );
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    let messenger = IL1Messenger::new(L1_MESSENGER_ADDRESS, &provider);
    let receipt = messenger
        .sendToL1(Bytes::from(b"hello L1 interop".to_vec()))
        .send()
        .await
        .context("send L2->L1 message")?
        .get_receipt()
        .await
        .context("get receipt")?;
    anyhow::ensure!(receipt.status(), "L2->L1 message reverted");
    let tx_hash = receipt.transaction_hash;
    println!(
        "  Message sent: tx={tx_hash}, block={:?}, txIndex={:?}",
        receipt.block_number, receipt.transaction_index
    );

    // ---- Poll the proof RPC and dump RAW responses ----
    println!("\n=== Polling zks_getL2ToL1LogProof (messageRoot) — raw output ===");
    let start = tokio::time::Instant::now();
    let mut last = serde_json::Value::Null;
    while start.elapsed() < Duration::from_secs(300) {
        let json = get_message_root_proof_raw(&l2_rpc_url, tx_hash).await?;
        let result = json.get("result").cloned().unwrap_or(serde_json::Value::Null);
        if let Some(err) = json.get("error") {
            println!("  RPC error: {err}");
        }
        if !result.is_null() {
            println!(
                "  PROOF PRODUCED:\n{}",
                serde_json::to_string_pretty(&result)?
            );
            last = result;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    if last.is_null() {
        println!("  NO PROOF after 300s (result stayed null)");
    }

    let _ = server.kill();
    let _ = anvil.kill();
    println!("\nProbe done.");
    Ok(())
}

#[tokio::test]
async fn probe_l1_settled_message_proof() {
    run_probe().await.expect("l1 interop probe failed");
}
