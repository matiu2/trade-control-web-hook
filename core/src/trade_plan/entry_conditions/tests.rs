use super::EntryConditions;
use crate::broker::Candle;
use crate::moving_average::{AverageKind, MovingAverage};
use chrono::{DateTime, Utc};

fn candles(closes: &[f64]) -> Vec<Candle> {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    closes
        .iter()
        .enumerate()
        .map(|(i, &c)| Candle {
            time: DateTime::<Utc>::from_timestamp(i as i64 * 3600, 0).unwrap(),
            o: c,
            h: c,
            l: c,
            c,
        })
        .collect()
}

#[test]
fn average_specs_parse_and_reject_invalid_periods_or_kinds() {
    let ma: MovingAverage = "9 EMA".parse().unwrap();
    assert_eq!(ma.kind, AverageKind::Ema);
    assert_eq!(ma.period.get(), 9);
    assert_eq!(ma.to_string(), "9 ema");
    for bad in [
        "0 sma",
        "-9 ema",
        "20 wma",
        "ema",
        "9 ema extra",
        "65536 sma",
    ] {
        assert!(bad.parse::<MovingAverage>().is_err(), "accepted {bad}");
    }
    assert!(serde_json::from_str::<MovingAverage>(r#"{"kind":"sma","period":0}"#).is_err());
}

#[test]
fn sma_uses_close_prices_and_fails_closed_until_warm() {
    let ma: MovingAverage = "3 sma".parse().unwrap();
    let data = candles(&[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(ma.value_at(&data, 1), None);
    assert_eq!(ma.value_at(&data, 2), Some(2.0));
    assert_eq!(ma.value_at(&data, 3), Some(3.0));
}

#[test]
fn ema_seed_is_deterministic_across_live_and_replay_window_depths() {
    let ma: MovingAverage = "3 ema".parse().unwrap();
    let data = candles(&(0..80).map(|i| i as f64).collect::<Vec<_>>());
    assert_eq!(
        ma.value_at(&data, 20),
        None,
        "EMA requires ten periods of warmup"
    );
    let full = ma.value_at(&data, 79).unwrap();
    assert_eq!(Some(full), ma.value_at(&data[50..], 29));
    assert!((full - 78.0).abs() < 1e-9);
}

#[test]
fn touch_checks_each_pattern_candle_against_its_contemporary_average() {
    let ma: MovingAverage = "3 sma".parse().unwrap();
    let mut data = candles(&[10.0, 10.0, 10.0, 12.0, 14.0]);
    data[3].l = 11.0;
    data[3].h = 13.0; // SMA 10.667 is below the bar.
    data[4].l = 13.0;
    data[4].h = 15.0; // SMA 12 is below this bar too.
    assert!(!ma.touches_pattern(&data, data[3].time, data[4].time));
    // Combining the bars' extremes and the last MA would falsely count 12.
    data[3].l = 10.5;
    assert!(ma.touches_pattern(&data, data[3].time, data[4].time));
    assert!(
        !ma.touches_pattern(&data, data[4].time, data[4].time),
        "a touch before the pattern cannot qualify it"
    );
}

#[test]
fn standard_set_is_any_of_9_ema_20_sma_200_sma() {
    assert_eq!(
        MovingAverage::standard_set()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["9 ema", "20 sma", "200 sma"]
    );
}

#[test]
fn legacy_conditions_are_empty_and_the_ma_history_floor_covers_200_sma() {
    let json = r#"{"trade_id":"test","instrument":"EUR_USD","direction":"short","granularity":"h1","pip_size":0.0001,"rules":[]}"#;
    let mut plan: crate::trade_plan::TradePlan = serde_json::from_str(json).unwrap();
    assert!(plan.entry_conditions.is_empty());
    assert!(
        serde_json::to_value(&plan)
            .unwrap()
            .get("entry_conditions")
            .is_none()
    );
    plan.entry_conditions = EntryConditions {
        enable_on_ma_touch: Vec::new(),
        must_touch: MovingAverage::standard_set(),
        retrace: None,
    };
    assert!(plan.detector_history_bars() >= 205);
    let serialized = serde_json::to_string(&plan).unwrap();
    let loaded: crate::trade_plan::TradePlan = serde_json::from_str(&serialized).unwrap();
    assert_eq!(
        plan.entry_conditions.must_touch,
        loaded.entry_conditions.must_touch
    );
}

#[test]
fn enabling_200_sma_alone_requests_full_history_without_a_signal_filter() {
    let conditions = EntryConditions {
        enable_on_ma_touch: vec!["200 sma".parse().unwrap()],
        ..Default::default()
    };
    assert_eq!(conditions.history_bars(), 200);
    assert!(!conditions.is_empty());
    assert!(
        conditions.pattern_touches(&[], Utc::now(), Utc::now()),
        "an enabling average must not become a signal-location filter"
    );
    let encoded = serde_json::to_string(&conditions).unwrap();
    assert!(!encoded.contains("must_touch"));
    let decoded: EntryConditions = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.enable_on_ma_touch, conditions.enable_on_ma_touch);
}
