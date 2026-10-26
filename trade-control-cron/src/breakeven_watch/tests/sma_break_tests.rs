use super::*;
use tracing_subscriber::prelude::*;
use trade_control_core::intent::BreakevenTrigger;

#[test]
fn sma_200_break_watch_tightens_actual_fill_and_requires_full_warmup() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    let path: Vec<_> = (0..205)
        .map(|i| {
            let c = if i < 203 {
                100.0
            } else if i == 203 {
                99.0
            } else {
                101.0
            };
            Candle {
                time: start + Duration::hours(i),
                o: c,
                h: c + 0.1,
                l: c - 0.1,
                c,
            }
        })
        .collect();
    let attempt = attempt_with_be(
        "EUR_USD",
        Direction::Short,
        None,
        Some("POS-REAL"),
        Some(BreakevenSnapshot {
            rule: Breakeven {
                sma_200_break: true,
                trigger: BreakevenTrigger::MaProtectionOnly,
                ..Default::default()
            },
            entry_price: 106.0,
            take_profit: 60.0,
            granularity: Granularity::H1,
        }),
    );
    let position = OpenPosition {
        entry_price: Some(105.0),
        opened_at: Some(path[201].time),
        stop_loss: Some(110.0),
        ..alias_position()
    };
    for (skip, expected) in [(0, vec![105.0]), (5, Vec::new())] {
        let broker = SpyBroker::with(path[skip..].to_vec());
        pollster::block_on(watch_one(
            &broker,
            None,
            std::slice::from_ref(&attempt),
            &position,
            path[204].time + Duration::hours(1),
        ));
        assert_eq!(*broker.amends.borrow(), expected);
    }
}
