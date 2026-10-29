use super::*;
use tracing_subscriber::prelude::*;

#[tokio::test]
async fn entry_sl_veto_blocks_a_real_replay_before_it_can_open() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let inputs = load(&require_fixture(GBP_NZD_INVALIDATION)).expect("historical long setup");
    let expires_at = inputs.candles.last().expect("candles").time + chrono::Duration::days(365);
    let mark_cfg = DetectorMarkConfig::new(
        DirectionFilter::None,
        GoldenFilter::None,
        inputs.plan.direction,
    );
    let replay = super::super::super::replay::run(
        &inputs.plan,
        &inputs.candles,
        inputs.meta.granularity,
        inputs.meta.start,
        expires_at,
        mark_cfg,
        None,
        CronCadence::PER_BAR,
    )
    .await;
    assert!(
        replay.fires.iter().any(|fire| fire
            .rejected_reason()
            .is_some_and(|reason| reason.starts_with("rejected: sl-widen-past-veto (too-low"))),
        "the shared live entry gate must reject this historical widening"
    );
    let rendered = super::super::super::report::render(
        &inputs.plan,
        &replay,
        true,
        false,
        None,
        &mark_cfg,
        None,
    );
    assert!(rendered.economics.legs.is_empty(), "no trade may open");
    assert!(rendered.text.contains("sl-widen-past-veto"));
}
