use super::*;
use chrono::{Duration, TimeZone, Utc};
use tracing_subscriber::prelude::*;

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn path() -> Vec<Candle> {
    init();
    let start = Utc.with_ymd_and_hms(2026, 6, 16, 0, 0, 0).unwrap();
    (0..140)
        .map(|i| {
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
        })
        .collect()
}

#[test]
fn only_a_new_adverse_closed_cross_counts_and_warmup_is_required() {
    let candles = path();
    let hits: Vec<_> = (0..candles.len())
        .filter(|&i| adverse_cross_at(&candles, i, Direction::Short))
        .collect();
    assert_eq!(hits.len(), 1);
    assert!(hits[0] > 110);
    assert!(!(0..110).any(|i| adverse_cross_at(&candles, i, Direction::Short)));
    assert!(!(0..candles.len()).any(|i| adverse_cross_at(&candles, i, Direction::Long)));
    let mirrored: Vec<_> = candles
        .iter()
        .map(|c| Candle {
            o: 200.0 - c.o,
            h: 200.0 - c.l,
            l: 200.0 - c.h,
            c: 200.0 - c.c,
            ..*c
        })
        .collect();
    assert!(adverse_cross_at(&mirrored, hits[0], Direction::Long));
    assert!(!adverse_cross_at(
        &candles[hits[0] - 10..=hits[0]],
        10,
        Direction::Short
    ));
    assert_eq!(
        adverse_cross_at(&candles, hits[0], Direction::Short),
        adverse_cross_at(&candles[..=hits[0]], hits[0], Direction::Short)
    );
}

#[test]
fn break_even_requires_profit_and_must_never_loosen_a_stop() {
    init();
    assert_eq!(
        tighten_to_fill(Direction::Long, 100.0, 95.0, 101.0),
        Some(100.0)
    );
    assert_eq!(
        tighten_to_fill(Direction::Short, 100.0, 105.0, 99.0),
        Some(100.0)
    );
    for price in [99.0, 100.0, f64::NAN] {
        assert_eq!(tighten_to_fill(Direction::Long, 100.0, 95.0, price), None);
    }
    for price in [100.0, 101.0, f64::NAN] {
        assert_eq!(tighten_to_fill(Direction::Short, 100.0, 105.0, price), None);
    }
    assert_eq!(tighten_to_fill(Direction::Long, 100.0, 102.0, 103.0), None);
    assert_eq!(tighten_to_fill(Direction::Short, 100.0, 98.0, 97.0), None);
    assert_eq!(
        tighten_to_fill(Direction::Long, f64::NAN, 95.0, 101.0),
        None
    );
}

#[test]
fn prolonged_equal_averages_have_the_same_cross_with_live_or_full_history() {
    init();
    let start = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    let candles: Vec<_> = (0..100)
        .map(|i| i as f64)
        .chain(std::iter::repeat_n(100.0, 100))
        .chain([101.0])
        .enumerate()
        .map(|(i, c)| Candle {
            time: start + Duration::hours(i as i64),
            o: c,
            h: c,
            l: c,
            c,
        })
        .collect();
    let index = candles.len() - 1;
    assert!(adverse_cross_at(&candles, index, Direction::Short));
    let live = &candles[candles.len() - HISTORY_BARS..];
    assert!(adverse_cross_at(live, live.len() - 1, Direction::Short));
    assert!(
        !adverse_cross_at(&candles, index - 1, Direction::Short),
        "equality alone does not cross"
    );
}
