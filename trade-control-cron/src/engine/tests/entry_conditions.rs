use super::*;
use trade_control_core::trade_plan::{RetraceLine, TradePlan};

fn plan() -> TradePlan {
    serde_json::from_value(serde_json::json!({
        "trade_id":"ma-history", "instrument":"EUR_USD", "direction":"short",
        "granularity":"h1", "pip_size":0.0001,
        "rules":[{
            "rule_id":"05-enter", "fire_mode":"once",
            "trigger":{"type":"pine_pattern", "dir":"short"},
            "intent":{"v":1,"id":"entry","action":"enter","instrument":"EUR_USD",
                "not_after":"2026-06-20T00:00:00Z"}
        }]
    }))
    .unwrap()
}

#[test]
fn live_history_request_covers_ma_warmup_and_pattern_confirmation() {
    let mut p = plan();
    let now = ts("2026-06-17T20:00:00Z");
    let original = pine_lookback_since(&p, now).unwrap();
    p.entry_conditions.must_touch =
        trade_control_core::moving_average::MovingAverage::standard_set();
    assert_eq!(
        pine_lookback_since(&p, now),
        Some(now - chrono::Duration::hours(205))
    );
    assert!(pine_lookback_since(&p, now).unwrap() < original);
}

#[test]
fn enabling_average_alone_gets_full_live_history() {
    let mut p = plan();
    p.entry_conditions.enable_on_ma_touch = vec!["200 sma".parse().unwrap()];
    let now = ts("2026-06-17T20:00:00Z");
    assert!(p.entry_conditions.must_touch.is_empty());
    assert_eq!(
        pine_lookback_since(&p, now),
        Some(now - chrono::Duration::hours(205))
    );
}

#[test]
fn retrace_anchors_are_fetched_even_without_bcr_rules() {
    let mut p = plan();
    assert_eq!(trendline_anchor_since(&p), None);
    let oldest = ts("2026-06-10T10:00:00Z");
    p.entry_conditions.retrace = Some(RetraceLine {
        a: LinePoint {
            at_epoch: oldest.timestamp(),
            price: 1.20,
        },
        b: LinePoint {
            at_epoch: oldest.timestamp() + 7200,
            price: 1.19,
        },
    });
    assert_eq!(
        trendline_anchor_since(&p),
        Some(oldest - chrono::Duration::hours(1))
    );
}
