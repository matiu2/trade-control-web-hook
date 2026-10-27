use super::*;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::{Breakeven, BreakevenTrigger};

#[test]
fn sma_200_break_replay_keeps_201_bar_warmup_and_moves_stop_on_next_bar() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    let path: Vec<_> = (0..207)
        .map(|i| {
            let c = match i {
                0..203 => 100.0,
                203 => 99.0,
                204 => 101.0,
                205 => 102.0,
                _ => 106.0,
            };
            let mut c = candle("2026-06-01T00:00:00Z", c, c + 0.1, c - 0.1, c);
            c.time = start + chrono::Duration::hours(i);
            c
        })
        .collect();
    let mut intent = short_stop_intent();
    i_set_levels(&mut intent, 105.0, 110.0, 60.0);
    intent.entry = Some(EntrySpec::Market {});
    intent.breakeven = Some(Breakeven {
        sma_200_break: true,
        trigger: BreakevenTrigger::MaProtectionOnly,
        ..Default::default()
    });
    let mut shell = Shell::from_candle(&path[201].mid());
    shell.close = 105.0;
    let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).unwrap();
    assert!(
        matches!(simulate_fill_resolved(&resolved,&intent,&shell,0.0001,&path),SimOutcome::StoppedOut{exit_price,..} if exit_price==105.0)
    );
    assert_eq!(
        breakeven_armed_at_resolved(&resolved, &intent, &shell, 0.0001, &path),
        Some(path[204].time)
    );
    assert!(matches!(
        simulate_fill_resolved(&resolved, &intent, &shell, 0.0001, &path[..205]),
        SimOutcome::FilledOpen { .. }
    ));
    intent.breakeven = None;
    let unprotected = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).unwrap();
    assert!(matches!(
        simulate_fill_resolved(&unprotected, &intent, &shell, 0.0001, &path[201..]),
        SimOutcome::FilledOpen { .. }
    ));
}
