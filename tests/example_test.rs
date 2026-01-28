use alloy::providers::Provider;
use zksync_os_integration_tests::Tester;

#[test_log::test(tokio::test)]
async fn example_test() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    let l1_chain_id = tester.l1_provider.get_chain_id().await?;
    assert_eq!(l1_chain_id, 31337);
    let l2_chain_id = tester.l2_provider.get_chain_id().await?;
    assert_eq!(l2_chain_id, 6565);
    Ok(())
    
}


