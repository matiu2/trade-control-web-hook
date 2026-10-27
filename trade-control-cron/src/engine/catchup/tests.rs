use chrono::{DateTime, Duration, Utc};
use tracing_subscriber::prelude::*;
use trade_control_core::{
    broker::{Candle, filter_new_candles},
    recording::CronNoteSeverity,
    state::StoredPlan,
    trade_plan::TradePlan,
};
use trade_control_engine::{evaluate_plan, seed_plan_state};

fn ts(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn history() -> Vec<Candle> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    // Genuine tick-derived MID OHLC from competition account 12590470's
    // verified paired cache. Candle timestamps are UTC open times.
    serde_json::from_str(include_str!("eurgbp-h1-2026-10-26.json")).unwrap()
}

fn plan() -> TradePlan {
    // Isolate the reported rule; detector entries and future news controls do
    // not affect whether these closes crossed the too-high level.
    serde_json::from_value(serde_json::json!({
        "trade_id": "hs-eurgbp-67943809", "instrument": "EURGBP",
        "direction": "short", "granularity": "h1", "pip_size": 0.0001,
        "armed_at": "2026-10-26T13:21:51.507933467Z",
        "rules": [{
            "rule_id": "01-veto-too-high", "kind": "setup_invalidation",
            "trigger": {"type": "horizontal_cross", "level": 0.8395,
                "dir": "up", "bar": "on_close"},
            "fire_mode": "once",
            "intent": {"v": 1, "id": "hs-eurgbp-67943809-too-high",
                "not_after": "2026-10-30T20:30:00Z",
                "action": "veto", "instrument": "EURGBP", "broker": "mt5",
                "account": "the5ers-competition", "name": "too-high",
                "ttl_hours": 104, "level": "close-positions",
                "trade_id": "hs-eurgbp-67943809"}
        }]
    }))
    .unwrap()
}

fn stored() -> StoredPlan {
    StoredPlan {
        account: Some("the5ers-competition".into()),
        plan: plan(),
    }
}

#[test]
fn eurgbp_prearming_breaches_do_not_retroactively_veto() {
    let bars = history();
    let plan = plan();
    let armed = plan.armed_at.unwrap();
    let seed: Vec<_> = bars
        .iter()
        .filter(|c| c.time + Duration::hours(1) <= armed)
        .copied()
        .collect();
    assert_eq!(bars[8].time, ts("2026-10-26T08:00:00Z"));
    assert_eq!(bars[8].c, 0.839615);
    assert_eq!(bars[9].c, 0.839610);
    let now = ts("2026-10-26T23:00:38Z");
    let expires = now + Duration::days(1);
    let prior = seed_plan_state(&plan, &seed, expires);
    let fresh = filter_new_candles(bars.clone(), prior.watermark.unwrap());
    assert_eq!(fresh.len(), 10);
    assert!(fresh.iter().all(|c| c.c < 0.8395));
    let eval = evaluate_plan(&plan, &prior, &fresh, &bars, now, expires);
    assert!(eval.fired.is_empty());
    assert!(!eval.done);
    assert_eq!(eval.new_state.watermark, Some(ts("2026-10-26T22:00:00Z")));
    assert_eq!(eval.new_state.last_close["01-veto-too-high"], 0.837555);
}

#[test]
fn eurgbp_catchup_vetoes_on_earlier_cross_even_when_latest_close_is_below() {
    let bars = history();
    let mut plan = plan();
    // Counterfactual: arm before the observed 18:00 Brisbane crossing. The
    // same real candles must veto even if recovery waits until after midnight.
    plan.armed_at = Some(ts("2026-10-26T07:21:00Z"));
    let now = ts("2026-10-26T23:00:38Z");
    let expires = now + Duration::days(1);
    let prior = seed_plan_state(&plan, &bars[..8], expires);
    let fresh = filter_new_candles(bars.clone(), prior.watermark.unwrap());
    let eval = evaluate_plan(&plan, &prior, &fresh, &bars, now, expires);
    assert!(eval.done);
    assert_eq!(eval.fired.len(), 1);
    assert_eq!(eval.fired[0].rule_id, "01-veto-too-high");
    assert_eq!(eval.new_state.watermark, Some(bars[8].time));
    let latest_only = evaluate_plan(&plan, &prior, &bars[bars.len() - 1..], &bars, now, expires);
    assert!(
        latest_only.fired.is_empty(),
        "latest-only evaluation would miss the veto"
    );
}

#[test]
fn catchup_evaluates_every_intermediate_bar_and_control_in_chronological_order() {
    // End before the rollover entry-suppression hour, so every evaluated bar
    // can produce an observable heartbeat under the normal entry rules.
    let bars = history()[..21].to_vec();
    let mut plan = plan();
    plan.rules = serde_json::from_value(serde_json::json!([
        {"rule_id": "05-enter", "kind": "enter", "fire_mode": "every_bar",
         "trigger": {"type": "mw_every_bar"},
         "intent": {"v": 1, "id": "catchup-enter", "action": "enter",
            "not_after": "2026-10-30T20:30:00Z", "instrument": "EURGBP",
            "direction": "short", "trade_id": plan.trade_id}},
        {"rule_id": "pause", "kind": "control", "fire_mode": "once",
         "trigger": {"type": "time_reached", "at_epoch": ts("2026-10-26T16:00:00Z").timestamp()},
         "intent": {"v": 1, "id": "catchup-pause", "action": "pause",
            "not_after": "2026-10-30T20:30:00Z", "instrument": "EURGBP",
            "trade_id": plan.trade_id}},
        {"rule_id": "resume", "kind": "control", "fire_mode": "once",
         "trigger": {"type": "time_reached", "at_epoch": ts("2026-10-26T17:00:00Z").timestamp()},
         "intent": {"v": 1, "id": "catchup-resume", "action": "resume",
            "not_after": "2026-10-30T20:30:00Z", "instrument": "EURGBP",
            "trade_id": plan.trade_id}}
    ]))
    .unwrap();
    let now = ts("2026-10-26T21:00:38Z");
    let expires = now + Duration::days(1);
    let prior = seed_plan_state(&plan, &bars[..13], expires);
    // Simulate an unordered broker response; the shared watermark filter
    // restores chronology and excludes the previously processed bars.
    let fresh = filter_new_candles(
        bars.iter().rev().copied().collect(),
        prior.watermark.unwrap(),
    );
    let batch = evaluate_plan(&plan, &prior, &fresh, &bars, now, expires);
    let entries: Vec<_> = batch
        .fired
        .iter()
        .filter(|f| f.rule_id == "05-enter")
        .map(|f| f.candle.time)
        .collect();
    assert_eq!(entries, fresh.iter().map(|c| c.time).collect::<Vec<_>>());
    assert_eq!(
        entries.len(),
        8,
        "every missed bar gets evaluated, not just the latest"
    );
    let controls: Vec<_> = batch
        .fired
        .iter()
        .filter(|f| f.rule_id != "05-enter")
        .map(|f| (f.rule_id.as_str(), f.candle.time))
        .collect();
    assert_eq!(
        controls,
        [
            ("pause", ts("2026-10-26T15:00:00Z")),
            ("resume", ts("2026-10-26T16:00:00Z"))
        ]
    );
    let (sequential, sequential_fires) =
        fresh
            .iter()
            .fold((prior, Vec::new()), |(state, mut fires), bar| {
                let eval = evaluate_plan(&plan, &state, &[*bar], &bars, now, expires);
                fires.extend(eval.fired);
                (eval.new_state, fires)
            });
    assert_eq!(batch.new_state, sequential);
    let identity = |fires: &[trade_control_core::plan_eval::FiredIntent]| {
        fires
            .iter()
            .map(|f| (f.rule_id.clone(), f.candle.time))
            .collect::<Vec<_>>()
    };
    assert_eq!(identity(&batch.fired), identity(&sequential_fires));
}

#[test]
fn catchup_warning_reports_backlog_delay_and_account() {
    let bars = history();
    let fresh = &bars[14..17];
    let note = super::observation(
        &stored(),
        bars[13].time,
        fresh,
        Some(bars[16].time),
        ts("2026-10-26T17:00:20Z"),
    )
    .unwrap();
    assert_eq!(note.source, "engine-catchup");
    assert_eq!(note.severity, CronNoteSeverity::Warn);
    assert_eq!(note.account.as_deref(), Some("the5ers-competition"));
    assert!(note.message.contains("3 closed candles"));
    assert!(note.message.contains("10820s"));
    assert!(
        note.message
            .contains("oldest pending candle opened at 2026-10-26 14:00:00 UTC")
    );
    assert!(note.message.contains("evaluated 3"));
    assert!(note.message.contains("2026-10-26 16:00:00 UTC"));
}

#[test]
fn terminal_veto_warning_does_not_claim_unprocessed_candles_were_evaluated() {
    let bars = history();
    let fresh = &bars[8..];
    let note = super::observation(
        &stored(),
        bars[7].time,
        fresh,
        Some(bars[8].time),
        ts("2026-10-26T23:00:38Z"),
    )
    .unwrap();
    assert!(note.message.contains("15 closed candles"));
    assert!(note.message.contains("evaluated 1"));
    assert!(note.message.contains("2026-10-26 08:00:00 UTC"));
}

#[test]
fn normal_tick_and_weekend_gap_without_multiple_bars_do_not_warn() {
    let bars = history();
    let now = ts("2026-10-26T23:00:38Z");
    assert!(super::observation(&stored(), bars[21].time, &[], Some(bars[21].time), now).is_none());
    assert!(
        super::observation(
            &stored(),
            bars[21].time,
            &bars[22..],
            Some(bars[22].time),
            now
        )
        .is_none()
    );
    assert!(
        super::observation(
            &stored(),
            bars[22].time - Duration::days(3),
            &bars[22..],
            Some(bars[22].time),
            now
        )
        .is_none()
    );
}
