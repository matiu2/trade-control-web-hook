use super::*;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::{Breakeven, BreakevenTrigger};

fn path() -> Vec<BidAskCandle> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    (0..135)
        .map(|i| {
            let c = if i < 110 {
                100.0 - i as f64 * 0.1
            } else {
                89.0 + (i - 110) as f64
            };
            let mut bar = candle("2026-06-01T00:00:00Z", c, c + 0.1, c - 0.1, c);
            bar.time = start + chrono::Duration::hours(i);
            bar
        })
        .collect()
}

#[test]
fn ma_cross_replay_uses_prefill_warmup_and_moves_stop_from_next_bar() {
    let path = path();
    let mut intent = short_stop_intent();
    i_set_levels(&mut intent, 100.0, 110.0, 60.0);
    intent.entry = Some(EntrySpec::Market {});
    intent.breakeven = Some(Breakeven {
        trigger: BreakevenTrigger::MaCrossOnly,
        ..Default::default()
    });
    let mut shell = Shell::from_candle(&path[105].mid());
    shell.close = 100.0;
    let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).unwrap();
    let out = simulate_fill_resolved(&resolved, &intent, &shell, 0.0001, &path);
    assert!(
        matches!(out,SimOutcome::StoppedOut{exit_price,..} if exit_price==100.0),
        "{out:?}"
    );
    let cross = breakeven_armed_at_resolved(&resolved, &intent, &shell, 0.0001, &path).unwrap();
    assert!(cross > shell.time);
    let cross_idx = path.iter().position(|c| c.time == cross).unwrap();
    let partial = simulate_fill_resolved(&resolved, &intent, &shell, 0.0001, &path[..=cross_idx]);
    assert!(
        matches!(partial, SimOutcome::FilledOpen { .. }),
        "moved stop cannot stop out on its arming bar: {partial:?}"
    );
    intent.breakeven = None;
    let plain = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).unwrap();
    let baseline = simulate_fill_resolved(&plain, &intent, &shell, 0.0001, &path[105..]);
    assert!(
        matches!(baseline,SimOutcome::StoppedOut{exit_price,..} if exit_price==110.0),
        "{baseline:?}"
    );
}

#[test]
fn ma_cross_replay_does_not_bank_a_losing_cross_or_use_warmup_for_fills() {
    let path = path();
    let mut intent = short_stop_intent();
    i_set_levels(&mut intent, 90.0, 120.0, 40.0);
    intent.entry = Some(EntrySpec::Market {});
    intent.breakeven = Some(Breakeven {
        trigger: BreakevenTrigger::MaCrossOnly,
        ..Default::default()
    });
    let mut shell = Shell::from_candle(&path[105].mid());
    shell.close = 90.0;
    let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).unwrap();
    assert_eq!(
        breakeven_armed_at_resolved(&resolved, &intent, &shell, 0.0001, &path),
        None
    );
    assert!(matches!(
        simulate_fill_resolved(&resolved, &intent, &shell, 0.0001, &path),
        SimOutcome::FilledOpen { .. }
    ));
}
