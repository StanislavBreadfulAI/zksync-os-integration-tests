//! L1→L2 deposit through Bridgehub `requestL2TransactionDirect`.
//! Forked from `zksync-os-server/tools/generate-deposit` — integration-tests does not build or invoke
//! any `zksync-os-server` tooling except the server binary.

use std::str::FromStr;
use std::time::Instant;

use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::utils::Eip1559Estimation;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use anyhow::{Context, Result};

/// Must match `zksync_os_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE` / OS protocol.
pub const REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE: u64 = 800;

const L2_DEPOSIT_GAS_LIMIT: u64 = 500_000;
/// Gas limit for L1→L2 ERC20 deposits. A first-ever deposit of a token deploys
/// the bridged L2 token via NTV.bridgeMint → BridgedTokenFactory.deploy*, which
/// is dominated by CREATE2 + token-init cost. 500k (the ETH-deposit default) is
/// not enough — the priority tx runs out of gas on L2.
const L2_ERC20_DEPOSIT_GAS_LIMIT: u64 = 5_000_000;

alloy::sol! {
    #[allow(missing_docs)]
    struct L2CanonicalTransaction {
        uint256 txType;
        uint256 from;
        uint256 to;
        uint256 gasLimit;
        uint256 gasPerPubdataByteLimit;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        uint256 paymaster;
        uint256 nonce;
        uint256 value;
        uint256[4] reserved;
        bytes data;
        bytes signature;
        uint256[] factoryDeps;
        bytes paymasterInput;
        bytes reservedDynamic;
    }

    interface IMailbox {
        event NewPriorityRequest(
            uint256 txId,
            bytes32 txHash,
            uint64 expirationTimestamp,
            L2CanonicalTransaction transaction,
            bytes[] factoryDeps
        );
    }

    #[sol(rpc)]
    interface IBridgehub {
        struct L2TransactionRequestDirect {
            uint256 chainId;
            uint256 mintValue;
            address l2Contract;
            uint256 l2Value;
            bytes l2Calldata;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            bytes[] factoryDeps;
            address refundRecipient;
        }

        struct L2TransactionRequestTwoBridgesOuter {
            uint256 chainId;
            uint256 mintValue;
            uint256 l2Value;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            address refundRecipient;
            address secondBridgeAddress;
            uint256 secondBridgeValue;
            bytes secondBridgeCalldata;
        }

        function requestL2TransactionDirect(
            L2TransactionRequestDirect calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function requestL2TransactionTwoBridges(
            L2TransactionRequestTwoBridgesOuter calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function l2TransactionBaseCost(
            uint256 _chainId,
            uint256 _gasPrice,
            uint256 _l2GasLimit,
            uint256 _l2GasPerPubdataByteLimit
        ) external view returns (uint256);

        function assetRouter() external view returns (address);
    }

    #[sol(rpc)]
    interface IERC20Mintable {
        function mint(address to, uint256 amount) external;
        function approve(address spender, uint256 value) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Submit an L1→L2 deposit to a specific L2 recipient.
///
/// Uses the given `private_key` as the L1 signer. If `l2_recipient` is `None`,
/// the deposit goes to the L1 signer's own address.
pub async fn submit_l1_to_l2_deposit_to(
    l1_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    private_key: &str,
    amount_ether: f64,
    l2_recipient: Option<&str>,
) -> Result<FixedBytes<32>> {
    submit_l1_to_l2_deposit_ex(
        l1_rpc_url,
        bridgehub_addr,
        chain_id,
        private_key,
        amount_ether,
        l2_recipient,
        true,
    )
    .await
}

fn deposit_eip1559_estimator(base_fee_per_gas: u128, _rewards: &[Vec<u128>]) -> Eip1559Estimation {
    Eip1559Estimation {
        max_fee_per_gas: base_fee_per_gas * 3 / 2,
        max_priority_fee_per_gas: 0,
    }
}

/// Like [`submit_l1_to_l2_deposit_to`] but allows specifying whether the
/// chain's base token is ETH.  When `base_token_is_eth` is `false` the
/// caller must have already approved the base token to the bridgehub for at
/// least `amount + l2TransactionBaseCost`.  The transaction will be sent
/// with `msg.value = 0`.
pub async fn submit_l1_to_l2_deposit_ex(
    l1_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    private_key: &str,
    amount_ether: f64,
    l2_recipient: Option<&str>,
    base_token_is_eth: bool,
) -> Result<FixedBytes<32>> {
    let submit_start = Instant::now();
    let bridgehub_address: Address = bridgehub_addr
        .parse()
        .with_context(|| format!("invalid bridgehub address {bridgehub_addr}"))?;
    let amount = U256::from((amount_ether * 1e18) as u128);
    let l1_wallet = EthereumWallet::new(
        LocalSigner::from_str(private_key).context("invalid private key for deposit")?,
    );
    let l1_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(l1_wallet.clone())
        .on_builtin(l1_rpc_url)
        .await
        .with_context(|| format!("connect L1 JSON-RPC at {l1_rpc_url}"))?;

    let bridgehub = IBridgehub::new(bridgehub_address, l1_provider.clone());
    let max_priority_fee_per_gas = l1_provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(|e| anyhow::anyhow!("eth_maxPriorityFeePerGas: {e}"))?;
    let base_l1_fees_data = l1_provider
        .estimate_eip1559_fees(Some(deposit_eip1559_estimator))
        .await
        .map_err(|e| anyhow::anyhow!("estimate_eip1559_fees: {e}"))?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let tx_base_cost = bridgehub
        .l2TransactionBaseCost(
            U256::from(chain_id),
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            U256::from(L2_DEPOSIT_GAS_LIMIT),
            U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        )
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Bridgehub.l2TransactionBaseCost: {e}"))?
        ._0;

    let sender = l1_wallet.default_signer().address();
    let recipient: Address = match l2_recipient {
        Some(addr) => addr
            .parse()
            .with_context(|| format!("invalid l2_recipient address {addr}"))?,
        None => sender,
    };
    let request = IBridgehub::L2TransactionRequestDirect {
        chainId: U256::from(chain_id),
        mintValue: amount + tx_base_cost,
        l2Contract: recipient,
        l2Value: amount,
        l2Calldata: vec![].into(),
        l2GasLimit: U256::from(L2_DEPOSIT_GAS_LIMIT),
        l2GasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        factoryDeps: vec![],
        refundRecipient: recipient,
    };

    let msg_value = if base_token_is_eth {
        amount + tx_base_cost
    } else {
        U256::ZERO
    };
    // Use the contract call builder's `.send()` directly (not
    // `.into_transaction_request()` + `provider.send_transaction`): the
    // latter path drops the configured wallet's `from` address, and alloy
    // falls back to `eth_accounts[0]` on the node. On some anvil states
    // (e.g. the committed v30.2 dump under `local-chains/`) that is a
    // different account than our wallet, so the L1 tx succeeds under the
    // wrong sender and the L2 priority tx is credited to the wrong account.
    let l1_deposit_receipt = bridgehub
        .requestL2TransactionDirect(request)
        .value(msg_value)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .from(sender)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("send L1 Bridgehub deposit tx: {e}"))?
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("get L1 deposit receipt: {e}"))?;

    anyhow::ensure!(
        l1_deposit_receipt.status(),
        "L1 deposit transaction reverted"
    );

    let l1_to_l2_tx_log = l1_deposit_receipt
        .inner
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<IMailbox::NewPriorityRequest>().ok())
        .next()
        .context("no L1→L2 NewPriorityRequest log from deposit tx")?;

    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;
    println!(
        "  L1→L2 deposit: submitted on L1 in {:.2}s, L2 priority tx {l2_tx_hash}",
        submit_start.elapsed().as_secs_f64()
    );
    Ok(l2_tx_hash)
}

/// Poll the L2 RPC for the given priority tx's receipt. Returns once the
/// receipt is available *and* the tx succeeded; errors if the tx reverted
/// on L2 or the timeout elapses.
///
/// Paired with [`submit_l1_to_l2_deposit_to`] / [`submit_l1_to_l2_deposit_ex`]:
/// those submit the L1 tx and return the predicted L2 hash; this waits for
/// the L2 side to actually execute. Callers that submit the deposit into a
/// priority queue with no L2 server yet running should skip this step.
pub async fn wait_for_l2_priority_tx_receipt(
    l2_rpc_url: &str,
    l2_tx_hash: FixedBytes<32>,
    timeout: std::time::Duration,
) -> Result<()> {
    // Issue raw `eth_getTransactionReceipt` calls and parse the result as
    // generic JSON instead of an alloy-typed receipt. The L2 side returns
    // ZKsync-specific transaction types (e.g. `0x7f` for L1→L2 priority
    // txs) that alloy's strict typed-receipt enum rejects. We only need
    // the `status` field.
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let hash_hex = format!("{l2_tx_hash:#x}");
    loop {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionReceipt",
            "params": [hash_hex],
        });
        let resp = client
            .post(l2_rpc_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("eth_getTransactionReceipt POST to {l2_rpc_url}"))?;
        let json: serde_json::Value = resp
            .json()
            .await
            .context("eth_getTransactionReceipt response was not JSON")?;
        if let Some(err) = json.get("error") {
            anyhow::bail!("L2 eth_getTransactionReceipt({hash_hex}) RPC error: {err}");
        }
        let result = json
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if !result.is_null() {
            // status is "0x1" (success) or "0x0" (reverted).
            match result.get("status").and_then(|v| v.as_str()) {
                Some("0x1") => return Ok(()),
                Some("0x0") => {
                    anyhow::bail!("L1→L2 priority tx {hash_hex} executed on L2 but reverted");
                }
                other => {
                    anyhow::bail!(
                        "L1→L2 priority tx {hash_hex} receipt has unexpected status field: {other:?}"
                    );
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("L1→L2 priority tx {hash_hex} did not execute on L2 within {timeout:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Mint `amount` of the ERC20 at `token_addr` to `to`. Caller key must be a
/// minter (TestnetERC20Token has unrestricted `mint`).
pub async fn mint_erc20(
    l1_rpc_url: &str,
    private_key: &str,
    token_addr: &str,
    to: &str,
    amount: U256,
) -> Result<()> {
    let token: Address = token_addr.parse().context("parse token address")?;
    let to_addr: Address = to.parse().context("parse recipient address")?;
    let wallet = EthereumWallet::new(
        LocalSigner::from_str(private_key).context("invalid private key for mint_erc20")?,
    );
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_builtin(l1_rpc_url)
        .await
        .context("connect L1 RPC")?;
    let erc20 = IERC20Mintable::new(token, provider);
    let receipt = erc20
        .mint(to_addr, amount)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20.mint send: {e}"))?
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20.mint receipt: {e}"))?;
    anyhow::ensure!(receipt.status(), "ERC20.mint reverted");
    Ok(())
}

/// Approve `spender` for `amount` units of the ERC20 at `token_addr`.
pub async fn approve_erc20(
    l1_rpc_url: &str,
    private_key: &str,
    token_addr: &str,
    spender: &str,
    amount: U256,
) -> Result<()> {
    let token: Address = token_addr.parse().context("parse token address")?;
    let spender_addr: Address = spender.parse().context("parse spender address")?;
    let wallet = EthereumWallet::new(
        LocalSigner::from_str(private_key).context("invalid private key for approve_erc20")?,
    );
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_builtin(l1_rpc_url)
        .await
        .context("connect L1 RPC")?;
    let erc20 = IERC20Mintable::new(token, provider);
    let receipt = erc20
        .approve(spender_addr, amount)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20.approve send: {e}"))?
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("ERC20.approve receipt: {e}"))?;
    anyhow::ensure!(receipt.status(), "ERC20.approve reverted");
    Ok(())
}

/// Submit an L1→L2 ERC20 (non-base-token) deposit via Bridgehub
/// `requestL2TransactionTwoBridges`.
///
/// Assumes the chain's base token is ETH (msg.value carries `mintValue`). The
/// L1 signer must have already approved the L1AssetRouter for `deposit_amount`
/// of the ERC20.
///
/// Returns the predicted L2 priority-tx hash.
pub async fn submit_l1_to_l2_erc20_deposit(
    l1_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    private_key: &str,
    l1_token: &str,
    deposit_amount: U256,
    l2_recipient: &str,
) -> Result<FixedBytes<32>> {
    let submit_start = Instant::now();
    let bridgehub: Address = bridgehub_addr.parse().context("parse bridgehub address")?;
    let l1_token_addr: Address = l1_token.parse().context("parse l1_token address")?;
    let l2_recipient_addr: Address = l2_recipient.parse().context("parse l2_recipient address")?;

    let wallet = EthereumWallet::new(
        LocalSigner::from_str(private_key).context("invalid private key for erc20 deposit")?,
    );
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet.clone())
        .on_builtin(l1_rpc_url)
        .await
        .context("connect L1 RPC")?;

    let bridgehub_contract = IBridgehub::new(bridgehub, provider.clone());

    let asset_router = bridgehub_contract
        .assetRouter()
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Bridgehub.assetRouter: {e}"))?
        ._0;

    let max_priority_fee_per_gas = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(|e| anyhow::anyhow!("eth_maxPriorityFeePerGas: {e}"))?;
    let base_fees = provider
        .estimate_eip1559_fees(Some(deposit_eip1559_estimator))
        .await
        .map_err(|e| anyhow::anyhow!("estimate_eip1559_fees: {e}"))?;
    let max_fee_per_gas = base_fees.max_fee_per_gas + max_priority_fee_per_gas;
    let tx_base_cost = bridgehub_contract
        .l2TransactionBaseCost(
            U256::from(chain_id),
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            U256::from(L2_ERC20_DEPOSIT_GAS_LIMIT),
            U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        )
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Bridgehub.l2TransactionBaseCost: {e}"))?
        ._0;

    // Legacy encoding: abi.encode(address l1Token, uint256 amount, address l2Receiver).
    // First byte of an abi-encoded address is naturally 0x00, which matches
    // LEGACY_ENCODING_VERSION on L1AssetRouter.
    let second_bridge_calldata = alloy::sol_types::SolValue::abi_encode(&(
        l1_token_addr,
        deposit_amount,
        l2_recipient_addr,
    ));

    let sender = wallet.default_signer().address();
    let request = IBridgehub::L2TransactionRequestTwoBridgesOuter {
        chainId: U256::from(chain_id),
        mintValue: tx_base_cost,
        l2Value: U256::ZERO,
        l2GasLimit: U256::from(L2_ERC20_DEPOSIT_GAS_LIMIT),
        l2GasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        refundRecipient: l2_recipient_addr,
        secondBridgeAddress: asset_router,
        secondBridgeValue: U256::ZERO,
        secondBridgeCalldata: second_bridge_calldata.into(),
    };

    let receipt = bridgehub_contract
        .requestL2TransactionTwoBridges(request)
        .value(tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .from(sender)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("send L1 Bridgehub two-bridges deposit tx: {e}"))?
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("get L1 two-bridges deposit receipt: {e}"))?;
    anyhow::ensure!(
        receipt.status(),
        "L1 two-bridges deposit transaction reverted"
    );

    let l1_to_l2_log = receipt
        .inner
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<IMailbox::NewPriorityRequest>().ok())
        .next()
        .context("no L1→L2 NewPriorityRequest log from two-bridges deposit tx")?;
    let l2_tx_hash = l1_to_l2_log.inner.txHash;
    println!(
        "  L1→L2 ERC20 deposit: submitted on L1 in {:.2}s, L2 priority tx {l2_tx_hash}",
        submit_start.elapsed().as_secs_f64()
    );
    Ok(l2_tx_hash)
}
