use anyhow::Result;
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::{ActivityConfig, Ecosystem};

/// Manual handle: start transfer-only noise, confirm it ran and stays healthy,
/// then stop cleanly.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn manual_handle_runs_and_stops(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let handle = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());

    // Wait until at least a few transfers have been submitted.
    let start = std::time::Instant::now();
    while handle.stats().txs_sent < 3 {
        anyhow::ensure!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "background transfers did not reach 3 within 30s (sent={})",
            handle.stats().txs_sent
        );
        anyhow::ensure!(!handle.stats().failed, "activity task died early");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    assert!(!handle.stats().failed, "activity should still be healthy");
    handle.stop().await;
    Ok(())
}

/// After stop().await the guard is cleared, so activity can be started again.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn restart_after_stop(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let h1 = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());
    h1.stop().await;
    // Should not panic — guard was cleared by stop().
    let h2 = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());
    h2.stop().await;
    Ok(())
}

/// pause()/resume() halts new submissions while paused.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn pause_halts_submissions(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let handle = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());

    // Let some traffic flow.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    handle.pause();

    // Synchronize on observed quiescence rather than assuming a fixed grace
    // window: pause() only stops *future* iterations, so a send in flight when
    // we paused can still complete — and on a loaded box it may outlast any
    // fixed sleep. Wait until two reads 1s apart agree (the in-flight send has
    // drained), then treat that as the quiesced count.
    let quiesce_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let quiesced = loop {
        let a = handle.stats().txs_sent;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let b = handle.stats().txs_sent;
        if a == b {
            break b;
        }
        anyhow::ensure!(
            std::time::Instant::now() < quiesce_deadline,
            "transfers did not quiesce within 20s after pause()"
        );
    };

    // Once quiesced, no further submissions should occur while paused.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        quiesced,
        handle.stats().txs_sent,
        "no new transfers should be submitted while paused"
    );
    handle.stop().await;
    Ok(())
}

/// Fixture parameter auto-starts activity; the chain is moving when the test
/// body runs. A plain ping still finalizes despite the background noise.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn fixture_param_autostarts(
    #[future]
    #[with(vec![6565], Some(ActivityConfig::default()))]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let hash = eco.chain().ping().await?;
    eco.chain().wait_for_tx_finalized(hash).await?;
    Ok(())
}

/// Starting activity twice on the same chain panics (the double-start guard).
#[rstest]
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "already running")]
async fn double_start_panics(#[future] ecosystem: Ecosystem) {
    let eco = ecosystem.await;
    let _h1 = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());
    // Second start without stopping the first must panic.
    let _h2 = eco
        .chain()
        .start_background_activity(ActivityConfig::transfers_only());
}
