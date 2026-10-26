use super::*;
use crate::intent::{Breakeven, BreakevenTrigger};
use chrono::{Duration, TimeZone, Utc};
use tracing_subscriber::prelude::*;

fn candles() -> Vec<Candle> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    (0..205)
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
        .collect()
}

#[test]
fn sma_200_break_requires_a_new_adverse_close_and_201_bars() {
    let mut window = candles();
    let average = "200 sma".parse().unwrap();
    let last = window.len() - 1;
    assert!(average_break_at(&window, last, average, Direction::Short));
    assert!(!average_break_at(&window, last, average, Direction::Long));
    assert!(!average_break_at(
        &window[last - 199..],
        199,
        average,
        Direction::Short
    ));
    assert!(average_break_at(
        &window[last - 200..],
        200,
        average,
        Direction::Short
    ));
    window[last].c = 99.0;
    window[last].h = 110.0;
    assert!(
        !average_break_at(&window, last, average, Direction::Short),
        "a wick cannot break and close"
    );
    window[last].c = 100.0; // SMA includes yesterday's 99 close: compare to today's SMA, not yesterday's.
    assert!(average_break_at(&window, last, average, Direction::Short));
    let mirrored: Vec<_> = window
        .iter()
        .map(|c| Candle {
            o: 200.0 - c.o,
            h: 200.0 - c.l,
            l: 200.0 - c.h,
            c: 200.0 - c.c,
            ..*c
        })
        .collect();
    assert!(average_break_at(&mirrored, last, average, Direction::Long));
}

#[test]
fn sma_200_break_stop_shares_profit_checks_and_respects_no_percentage_trigger() {
    let window = candles();
    let last = window.len() - 1;
    let rule = Breakeven {
        sma_200_break: true,
        trigger: BreakevenTrigger::MaProtectionOnly,
        ..Default::default()
    };
    assert!(rule.has_ma_veto());
    assert_eq!(rule.ma_veto_history_bars(), 201);
    assert!(!rule.close_arms(Direction::Short, 150.0, 101.0));
    let inputs = StopInputs {
        direction: Direction::Short,
        fill: 105.0,
        current_stop: 110.0,
        exit_price: 101.1,
        event_exit_price: 101.1,
    };
    assert_eq!(cross_stop_at(rule, &window, last, inputs), Some(105.0));
    assert_eq!(
        cross_stop_at(
            rule,
            &window,
            last,
            StopInputs {
                exit_price: 106.0,
                ..inputs
            }
        ),
        None
    );
    assert_eq!(
        cross_stop_at(
            rule,
            &window,
            last,
            StopInputs {
                event_exit_price: 106.0,
                ..inputs
            }
        ),
        None
    );
    assert_eq!(
        cross_stop_at(
            rule,
            &window,
            last,
            StopInputs {
                current_stop: 104.0,
                ..inputs
            }
        ),
        None
    );
}

#[test]
fn sma_200_break_veto_does_not_implicitly_enable_the_ema_cross_veto() {
    let mut window = candles();
    window.truncate(200);
    window.iter_mut().for_each(|bar| {
        bar.o = 200.0;
        bar.h = 200.1;
        bar.l = 199.9;
        bar.c = 200.0;
    });
    let start = window.last().unwrap().time + Duration::hours(1);
    window.extend((0..140).map(|i| {
        let c = if i < 110 {
            100.0 - i as f64 * 0.1
        } else {
            89.0 + (i - 110) as f64
        };
        Candle {
            time: start + Duration::hours(i),
            o: c,
            h: c + 0.1,
            l: c - 0.1,
            c,
        }
    }));
    let cross = (310..window.len())
        .find(|&i| adverse_cross_at(&window, i, Direction::Short))
        .unwrap();
    assert!(!average_break_at(
        &window,
        cross,
        "200 sma".parse().unwrap(),
        Direction::Short
    ));
    let inputs = StopInputs {
        direction: Direction::Short,
        fill: 150.0,
        current_stop: 200.0,
        exit_price: window[cross].c,
        event_exit_price: window[cross].c,
    };
    let only_sma = Breakeven {
        sma_200_break: true,
        trigger: BreakevenTrigger::MaProtectionOnly,
        ..Default::default()
    };
    assert_eq!(cross_stop_at(only_sma, &window, cross, inputs), None);
    let both = Breakeven {
        trigger: BreakevenTrigger::MaCrossOnly,
        ..only_sma
    };
    assert_eq!(cross_stop_at(both, &window, cross, inputs), Some(150.0));
}
