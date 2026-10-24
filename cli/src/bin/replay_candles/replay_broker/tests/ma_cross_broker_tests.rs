use super::*;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::{Breakeven, BreakevenTrigger};

#[test]
fn ma_cross_held_broker_retains_warmup_without_filling_before_fire() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    let path: Vec<_> = (0..135)
        .map(|i| {
            let c = if i < 110 {
                100.0 - i as f64 * 0.1
            } else {
                89.0 + (i - 110) as f64
            };
            candle(start.timestamp() + i * 3600, c)
        })
        .collect();
    let mut intent = short_enter_intent();
    intent.entry = Some(trade_control_core::intent::EntrySpec::Market {});
    intent.stop_loss = Some(trade_control_core::intent::PriceRef::Absolute { absolute: 110.0 });
    intent.take_profit = Some(trade_control_core::intent::TakeProfit::Anchored(
        trade_control_core::intent::PriceRef::Absolute { absolute: 60.0 },
    ));
    intent.breakeven = Some(Breakeven {
        trigger: BreakevenTrigger::MaCrossOnly,
        ..Default::default()
    });
    let mut shell = Shell::from_candle(&path[105].mid());
    shell.close = 100.0;
    let now = path.last().unwrap().time;
    let broker = ReplayBroker::new(path, 0.0001);
    let (_, outcome) = broker.step_outcome(&intent, &shell, &None, now).unwrap();
    assert!(
        matches!(outcome,SimOutcome::StoppedOut{fill_at,exit_price,..} if fill_at==shell.time && exit_price==100.0),
        "{outcome:?}"
    );
}
