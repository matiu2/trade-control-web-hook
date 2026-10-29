use super::EntryConditions;
use super::tests::candles;
use crate::intent::Direction;

fn conditions(average: &str) -> EntryConditions {
    EntryConditions {
        be_offside: vec![average.parse().unwrap()],
        ..Default::default()
    }
}

#[test]
fn offside_accepts_touching_or_beyond_in_the_trade_direction() {
    let filter = conditions("3 sma");
    for (closes, direction, qualifies) in [
        ([10.0, 10.0, 12.0], Direction::Short, true),
        ([10.0, 10.0, 8.0], Direction::Short, false),
        ([10.0, 10.0, 8.0], Direction::Long, true),
        ([10.0, 10.0, 12.0], Direction::Long, false),
        ([10.0, 10.0, 10.0], Direction::Short, true),
        ([10.0, 10.0, 10.0], Direction::Long, true),
    ] {
        let data = candles(&closes);
        assert_eq!(
            filter.pattern_qualifies(&data, data[2].time, data[2].time, direction),
            qualifies
        );
    }
    let mut data = candles(&[10.0, 10.0, 8.0]);
    data[2].h = 10.0;
    assert!(filter.pattern_qualifies(&data, data[2].time, data[2].time, Direction::Short));
    let mut data = candles(&[10.0, 10.0, 12.0]);
    data[2].l = 10.0;
    assert!(filter.pattern_qualifies(&data, data[2].time, data[2].time, Direction::Long));
}

#[test]
fn offside_uses_any_selected_average_and_requires_warm_history() {
    let mut filter = conditions("200 sma");
    let data = candles(&[10.0, 10.0, 12.0]);
    assert!(!filter.pattern_qualifies(&data, data[2].time, data[2].time, Direction::Short));
    filter.be_offside.push("3 sma".parse().unwrap());
    assert!(filter.pattern_qualifies(&data, data[2].time, data[2].time, Direction::Short));
    assert_eq!(filter.history_bars(), 200);
    assert!(!filter.is_empty());
    let wire = serde_json::to_string(&filter).unwrap();
    let decoded: EntryConditions = serde_json::from_str(&wire).unwrap();
    assert_eq!(decoded.be_offside, filter.be_offside);
    assert!(!wire.contains("must_touch"));
}

#[test]
fn offside_checks_only_pattern_constituents_and_requires_both_endpoints() {
    let filter = conditions("3 sma");
    let data = candles(&[10.0, 10.0, 12.0, 8.0, 7.0, 14.0]);
    assert!(!filter.pattern_qualifies(&data, data[3].time, data[4].time, Direction::Short));
    assert!(filter.pattern_qualifies(&data, data[2].time, data[4].time, Direction::Short));
    assert!(!filter.pattern_qualifies(&data[..4], data[2].time, data[4].time, Direction::Short));
    assert!(!filter.pattern_qualifies(&data[3..], data[2].time, data[4].time, Direction::Short));
}

#[test]
fn offside_supports_ema_and_leaves_legacy_touch_strict() {
    let filter = conditions("9 ema");
    let mut closes = vec![10.0; 90];
    closes.push(12.0);
    let data = candles(&closes);
    assert!(filter.pattern_qualifies(&data, data[90].time, data[90].time, Direction::Short));
    let strict = EntryConditions {
        must_touch: filter.be_offside.clone(),
        ..Default::default()
    };
    assert!(!strict.pattern_qualifies(&data, data[90].time, data[90].time, Direction::Short));
    assert!(EntryConditions::default().pattern_qualifies(
        &[],
        data[0].time,
        data[0].time,
        Direction::Short
    ));
}
