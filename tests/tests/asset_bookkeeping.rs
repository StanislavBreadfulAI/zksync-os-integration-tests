//! Chain-local asset bookkeeping on the L2AssetTracker-less (reduced) contracts lineage.
//!
//! era-contracts #2309 + #2360 delete the `L2AssetTracker` system contract: 0x1000f is a
//! reserved gap in the ZKsync OS genesis, and the per-asset interop bookkeeping lives in
//! `L2NativeTokenVault` (`interopInfo` / `bridgedOut`), with `BaseTokenHolder` reporting
//! base-token flows via `recordBaseTokenBridgingToChain` / `recordBaseTokenBridgingFromChain`.
//!
//! One design consequence asserted here: on ZKsync OS the VM credits L1→L2 base-token
//! deposits by moving the holder's balance directly (the bootloader's legacy notification
//! targets the empty 0x1000f gap and is a silent no-op), so inbound base-token deposits are
//! intentionally *not* recorded in `interopInfo` — while outbound withdrawals go through
//! `L2BaseToken.withdraw` → `BaseTokenHolder.burnAndStartBridging` → the vault, and are.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, Address, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolValue;
use anyhow::{ensure, Context, Result};
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::Ecosystem;
use zk_deployer::l1_l2_deposit::{deposit_eth, wait_for_l2_balance, DEFAULT_L1_TO_L2_GAS_PRICE};

sol! {
    #[sol(rpc)]
    interface IL2BaseToken {
        function withdraw(address _l1Receiver) external payable;
    }
    #[sol(rpc)]
    interface IL2NativeTokenVault {
        function BASE_TOKEN_ASSET_ID() external view returns (bytes32);
        function interopInfo(bytes32 assetId) external view returns (uint256 totalWithdrawalsToL1, uint256 totalSuccessfulDepositsFromL1);
        function bridgedOut(bytes32 assetId) external view returns (uint256);
        function registerToken(address token) external;
        function assetId(address token) external view returns (bytes32);
    }
    #[sol(rpc)]
    interface IL2AssetRouter {
        function withdraw(bytes32 _assetId, bytes _assetData) external returns (bytes32);
    }
    #[sol(rpc)]
    interface ITestnetERC20 {
        function mint(address to, uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

const L2_BASE_TOKEN: Address = address!("000000000000000000000000000000000000800a");
const NATIVE_TOKEN_VAULT: Address = address!("0000000000000000000000000000000000010004");
const ASSET_ROUTER: Address = address!("0000000000000000000000000000000000010003");
const BASE_TOKEN_HOLDER: Address = address!("0000000000000000000000000000000000010011");
// Removed L2AssetTracker (reserved gap since era-contracts #2309).
const L2_ASSET_TRACKER_GAP: Address = address!("000000000000000000000000000000000001000f");
// Retired GWAssetTracker: keeps a compatibility stub so new-chain genesis matches upgraded chains.
const GW_ASSET_TRACKER_STUB: Address = address!("0000000000000000000000000000000000010010");
// InteropAttributeParser, new to the reduced lineage's genesis.
const INTEROP_ATTRIBUTE_PARSER: Address = address!("0000000000000000000000000000000000010015");

const TX_GAS: u64 = 5_000_000;

async fn l2_signer_provider(chain: &tests::Chain) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .wallet(EthereumWallet::from(chain.wallet(0).clone()))
        .connect(chain.l2_rpc_url())
        .await
        .context("connect L2 with wallet")?
        .erased())
}

/// Deploy a fresh TestnetERC20Token on L2 (creation bytecode from the era-contracts forge `out/`
/// the `ecosystem` fixture built its deployment from).
async fn deploy_token(provider: &DynProvider) -> Result<Address> {
    let era_root = protocol_ops::common::paths::contracts_root();
    let artifact_path =
        era_root.join("l1-contracts/out/TestnetERC20Token.sol/TestnetERC20Token.json");
    let json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&artifact_path)
            .with_context(|| format!("read token artifact {}", artifact_path.display()))?,
    )?;
    let bytecode_hex = json["bytecode"]["object"]
        .as_str()
        .context("token artifact missing bytecode.object")?;
    let mut init =
        hex::decode(bytecode_hex.trim_start_matches("0x")).context("decode token bytecode")?;
    init.extend_from_slice(
        &(
            "BookkeepingTest".to_string(),
            "BKT".to_string(),
            U256::from(18u8),
        )
            .abi_encode_params(),
    );
    let tx = TransactionRequest::default()
        .with_deploy_code(init)
        .with_gas_limit(TX_GAS);
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    ensure!(receipt.status(), "token deploy reverted");
    receipt
        .contract_address
        .context("no contract address in deploy receipt")
}

/// Genesis shape + base-token and ERC20 deposit/withdrawal bookkeeping without the L2AssetTracker.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn asset_bookkeeping_without_l2_asset_tracker(
    #[future] ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chain = eco.chain();
    let provider = l2_signer_provider(chain).await?;
    let user = chain.wallet(0).address();

    // ── Genesis shape: 0x1000f gone, 0x10010 stub + 0x10015 parser present ──
    let tracker_code = provider.get_code_at(L2_ASSET_TRACKER_GAP).await?;
    ensure!(
        tracker_code.is_empty(),
        "0x1000f must be an empty reserved gap, found {} bytes of code",
        tracker_code.len()
    );
    ensure!(
        !provider.get_code_at(GW_ASSET_TRACKER_STUB).await?.is_empty(),
        "0x10010 must keep the retired GWAssetTracker compatibility stub"
    );
    ensure!(
        !provider
            .get_code_at(INTEROP_ATTRIBUTE_PARSER)
            .await?
            .is_empty(),
        "0x10015 must hold the InteropAttributeParser on the reduced lineage"
    );

    let ntv = IL2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &provider);
    let base_asset_id = ntv.BASE_TOKEN_ASSET_ID().call().await?;
    ensure!(
        base_asset_id != B256::ZERO,
        "vault must know the base token asset id after genesis init"
    );

    // ── L1→L2 base-token deposits are intentionally unrecorded on ZKsync OS ──
    // The fixture already funded every wallet through real L1→L2 deposits; make one more
    // explicit deposit to a fresh address, then check the vault's inbound counter stayed 0.
    let fresh: Address = address!("00000000000000000000000000000000000b00c4");
    let one_eth = U256::from(10u64).pow(U256::from(18u64));
    deposit_eth(
        chain.l1_rpc_url(),
        chain.bridgehub_addr(),
        chain.chain_id(),
        fresh,
        one_eth,
        DEFAULT_L1_TO_L2_GAS_PRICE,
        // Anvil #1: rich on L1, independent of the test wallets' L2 funds.
        tests::WALLET_KEYS[1],
    )
    .await
    .context("explicit L1->L2 deposit")?;
    wait_for_l2_balance(chain.l2_rpc_url(), fresh, 120).await?;

    let base_info = ntv.interopInfo(base_asset_id).call().await?;
    ensure!(
        base_info.totalSuccessfulDepositsFromL1 == U256::ZERO,
        "VM-credited base-token deposits must not be recorded in interopInfo (got {})",
        base_info.totalSuccessfulDepositsFromL1
    );
    ensure!(
        base_info.totalWithdrawalsToL1 == U256::ZERO,
        "no base-token withdrawal happened yet (got {})",
        base_info.totalWithdrawalsToL1
    );

    // ── Base-token withdrawal: holder escrow + vault bookkeeping ──
    let holder_before = provider.get_balance(BASE_TOKEN_HOLDER).await?;
    let withdraw_amount = one_eth;
    let receipt = IL2BaseToken::new(L2_BASE_TOKEN, &provider)
        .withdraw(user)
        .value(withdraw_amount)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(receipt.status(), "base-token withdraw reverted");

    let base_info = ntv.interopInfo(base_asset_id).call().await?;
    ensure!(
        base_info.totalWithdrawalsToL1 == withdraw_amount,
        "base-token withdrawal must be recorded under BASE_TOKEN_ASSET_ID \
         (want {withdraw_amount}, got {})",
        base_info.totalWithdrawalsToL1
    );
    ensure!(
        base_info.totalSuccessfulDepositsFromL1 == U256::ZERO,
        "inbound counter must stay untouched by a withdrawal"
    );
    let holder_after = provider.get_balance(BASE_TOKEN_HOLDER).await?;
    ensure!(
        holder_after == holder_before + withdraw_amount,
        "withdrawn value must be escrowed in BaseTokenHolder (before {holder_before}, after {holder_after})"
    );

    // ── ERC20 (chain-native) withdrawal: vault escrow + bookkeeping ──
    let token = deploy_token(&provider).await?;
    let erc20 = ITestnetERC20::new(token, &provider);
    let mint = one_eth * U256::from(100u64);
    let r = erc20.mint(user, mint).gas(TX_GAS).send().await?.get_receipt().await?;
    ensure!(r.status(), "mint reverted");
    let r = ntv
        .registerToken(token)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "registerToken reverted");
    let token_asset_id = ntv.assetId(token).call().await?;
    ensure!(token_asset_id != B256::ZERO, "token must get an asset id");
    let r = erc20
        .approve(NATIVE_TOKEN_VAULT, mint)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "approve reverted");

    let erc20_amount = one_eth * U256::from(3u64);
    let withdraw_hash = {
        // `DataEncoding.encodeBridgeBurnData(amount, receiver, token)`. The legacy
        // `withdraw(address,address,uint256)` entry is for L1-origin tokens only
        // (`TokenNotLegacy`), so a chain-native token goes through the asset-id form.
        let burn_data = (erc20_amount, user, token).abi_encode_params();
        let r = IL2AssetRouter::new(ASSET_ROUTER, &provider)
            .withdraw(token_asset_id, burn_data.into())
            .gas(TX_GAS)
            .send()
            .await?
            .get_receipt()
            .await?;
        ensure!(r.status(), "ERC20 withdraw reverted");
        r.transaction_hash
    };

    ensure!(
        ntv.bridgedOut(token_asset_id).call().await? == erc20_amount,
        "vault must escrow the withdrawn native-token amount in bridgedOut"
    );
    let token_info = ntv.interopInfo(token_asset_id).call().await?;
    ensure!(
        token_info.totalWithdrawalsToL1 == erc20_amount,
        "ERC20 withdrawal to L1 must be recorded in interopInfo (want {erc20_amount}, got {})",
        token_info.totalWithdrawalsToL1
    );
    ensure!(
        erc20.balanceOf(NATIVE_TOKEN_VAULT).call().await? == erc20_amount,
        "withdrawn tokens must sit in the vault"
    );

    // ── The whole flow settles: commit → prove → execute on L1 ──
    chain.wait_for_tx_finalized(withdraw_hash).await?;
    Ok(())
}
