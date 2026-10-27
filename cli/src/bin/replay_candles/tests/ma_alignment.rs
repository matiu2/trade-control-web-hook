use super::*;

#[tokio::test]
async fn unfavorable_alignment_holds_a_pending_order_then_restores_it() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    // Warmup spans the preceding weekend; the live order runs Tuesday-Friday.
    let start = Utc.with_ymd_and_hms(2026, 5, 29, 0, 0, 0).unwrap();
    let candles: Vec<_> = (0..180)
        .map(|i| {
            let c = match i {
                0..=100 => 1.102 + i as f64 * 0.00004,
                101..=140 => 1.106 - (i - 100) as f64 * 0.00008,
                _ => 1.1028 + (i - 140) as f64 * 0.0001,
            };
            let high = if i == 135 || i == 179 {
                1.112
            } else {
                c + 0.0001
            };
            ohlc_at_spread(
                &(start + Duration::hours(i)).to_rfc3339(),
                c,
                high,
                c - 0.0001,
                c,
                0.00002,
            )
        })
        .collect();
    let mut plan = plain_enter_plan("EUR/USD", 1.105);
    plan.rules[0].trigger = trade_control_core::trade_plan::Trigger::MwEveryBar;
    plan.entry_conditions.require_ma_alignment = true;
    let replay = run(
        &plan,
        &candles,
        Granularity::H1,
        candles[100].time,
        start + Duration::days(20),
        no_marks(),
        None,
    )
    .await;
    let entries: Vec<_> = replay
        .fires
        .iter()
        .filter(|f| f.fired.intent.action == Action::Enter)
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "restore the first order rather than emit another entry"
    );
    let outcome = entries[0].realized.as_ref().expect("entry simulation");
    assert_eq!(
        outcome.fill_at, candles[179].time,
        "the earlier wick during the MA pause must not fill: {outcome:?}"
    );
    assert!(
        replay
            .fires
            .iter()
            .any(|f| f.fired.intent.action == Action::Pause)
    );
    assert!(!replay.final_state.entries_blocked);
}
