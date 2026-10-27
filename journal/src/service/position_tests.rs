use super::*;
use serde_json::json;

fn init() {
    use tracing_subscriber::prelude::*;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    });
}

#[test]
fn tick_market_fill_is_entry_evidence_even_without_an_inbound_enter() {
    init();
    let timeline = json!({"records":[],"ticks":[{"tick_ts":"2026-10-26T05:00:12Z","dispatch_outcomes":[{"outcome":"Ok(entered: order=17074690 size=0.65 @ 156.952)"}]}],"notes":[]});
    assert_eq!(
        crate::timeline::derive_entry_ts(&timeline.to_string()).as_deref(),
        Some("2026-10-26 15:00")
    );
    assert!(
        crate::timeline::derive_outcome(&timeline.to_string())
            .0
            .starts_with("entered:")
    );
    let pending = json!({"records":[],"ticks":[{"tick_ts":"2026-10-26T05:00:12Z","dispatch_outcomes":[{"outcome":"Ok(placed: order=17074690)"}]}]});
    assert!(crate::timeline::derive_entry_ts(&pending.to_string()).is_none());
}

#[test]
fn confirmed_entry_note_and_actual_position_fill_are_entry_evidence() {
    init();
    let timeline = json!({"records":[],"ticks":[],"notes":[{"source":"trade-entered","ts":"2026-10-26T05:01:28Z"}]});
    assert_eq!(
        crate::timeline::derive_entry_ts(&timeline.to_string()).as_deref(),
        Some("2026-10-26 15:01")
    );
    let timeline = json!({"records":[],"ticks":[],"positions":[{"link":{"broker_order_id":"17074690"},"snapshot":{"position":{"opened_at":"2026-10-26T05:00:42Z"}}}]});
    assert_eq!(
        crate::timeline::derive_entry_ts(&timeline.to_string()).as_deref(),
        Some("2026-10-26 15:00")
    );
}

#[test]
fn structured_comparison_uses_rule_fires_instead_of_fills_and_backstop_text() {
    init();
    let timeline = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../tests/fixtures/plan_timeline.json"
    ))
    .unwrap();
    let mut replay = json!({"success":true,"report":"Done: false | final phase: AwaitEntry | fires: 1 | TP: 0 SL: 0 | Net R: +0.00", "outcome":{"done":false,"final_phase":"await_entry","fires":[{"rule_id":"05-enter","action":"enter","candle_time":"2026-10-26T04:00:00Z"}],"outcome":{"open_at_end":1}},"open_positions":[]});
    let comparison = compare_timeline_with_replay_result(&timeline, &replay).unwrap();
    assert_eq!(comparison.replay_fires.len(), 1);
    assert_eq!(comparison.replay_fires[0].rule_id, "05-enter");
    assert_eq!(comparison.replay_fires[0].ts, "2026-10-26 14:00");
    assert_eq!(comparison.replay_open_at_end, Some(1));
    replay["outcome"]["fires"][0]["candle_time"] = json!("invalid");
    assert!(compare_timeline_with_replay_result(&timeline, &replay).is_err());
}

#[test]
fn report_annotations_are_not_rule_ids() {
    init();
    let report = "2026-10-26 15:00:00 +10:00  entry #1 FILLED @ 156.955 (still open at window end)  (close=157.0965)\n2026-10-26 15:00:00 +10:00  entry #2 placed — order UNRESOLVED  (close=157.0965)\n2026-10-26 15:00:00 +10:00  entry #2 BLOCKED — rejected: trade-already-open (backstop) → NO FILL / 0R  (close=157.0965)\n";
    assert!(crate::divergence::parse_replay_fires(report).is_empty());
}

#[test]
fn latest_tick_dispatch_wins_over_an_older_http_request() {
    init();
    let timeline = json!({"records":[{"ts":"2026-10-26T04:00:00Z","outcome":"placed: order=17074690"}],"ticks":[{"tick_ts":"2026-10-26T05:00:12Z","dispatch_outcomes":[{"outcome":"Ok(entered: order=17074690)"}]},{"tick_ts":"2026-10-26T06:00:12Z","dispatch_outcomes":[{"outcome":"Ok(closed 1)"}]}]});
    assert_eq!(
        crate::timeline::derive_outcome(&timeline.to_string()),
        ("closed 1".into(), true)
    );
}

#[test]
fn broker_fill_time_takes_precedence_over_an_earlier_dispatch_receipt() {
    init();
    let timeline = json!({"records":[{"ts":"2026-10-26T04:00:00Z","outcome":"entered: order=17074690"}],"positions":[{"link":{"broker_order_id":"17074690"},"snapshot":{"position":{"opened_at":"2026-10-26T05:00:42Z"}}}]});
    assert_eq!(
        crate::timeline::derive_entry_ts(&timeline.to_string()).as_deref(),
        Some("2026-10-26 15:00")
    );
}

#[test]
fn a_later_observed_reentry_does_not_erase_an_earlier_unobserved_fill() {
    init();
    let timeline = json!({"records":[{"ts":"2026-10-26T04:00:00Z","outcome":"entered: order=first"}],"positions":[{"link":{"broker_order_id":"second"},"snapshot":{"position":{"opened_at":"2026-10-26T06:00:42Z"}}}]});
    assert_eq!(
        crate::timeline::derive_entry_ts(&timeline.to_string()).as_deref(),
        Some("2026-10-26 14:00")
    );
}
