use super::*;
use chrono::Duration;
use tracing_subscriber::prelude::*;
use trade_control_core::{
    broker::Granularity,
    signals::{
        DetectorConfig, atr_length_for, detector_lookback_bars, latched_signal_at, wilder_atr,
    },
    trade_plan::TradePlan,
};
use trade_control_engine::{evaluate_plan, seed_plan_state};

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn history() -> Vec<Candle> {
    init();
    // Genuine tick-derived MID OHLC exported from the staging account cache.
    // No estimated ask/mid prices or broker access are used by these tests.
    serde_json::from_str(include_str!("audchf-h4-2026-10-14.json")).unwrap()
}

fn candidates(bars: &[Candle]) -> &[Candle] {
    &bars[bars.len() - 3..]
}

fn want() -> usize {
    detector_lookback_bars(
        &DetectorConfig::pine_defaults(Granularity::H4),
        Granularity::H4,
    )
}

fn since(bar: &Candle) -> DateTime<Utc> {
    bar.time - Duration::hours(4 * want() as i64)
}

fn cached(bars: &[Candle], from: DateTime<Utc>, end: DateTime<Utc>) -> Vec<Candle> {
    bars.iter()
        .filter(|c| c.time > from && c.time < end)
        .copied()
        .collect()
}

#[test]
fn original_weekend_window_has_cold_atr_and_non_golden_signals() {
    let bars = history();
    for bar in candidates(&bars) {
        let window = cached(&bars, since(bar), bar.time + Duration::hours(4));
        assert_eq!(window.len(), 26);
        assert!(wilder_atr(&window, atr_length_for(Granularity::H4)).is_none());
        let signal = latched_signal_at(
            &window,
            window.len() - 1,
            &DetectorConfig::pine_defaults(Granularity::H4),
        )
        .unwrap();
        assert!(!signal.golden);
    }
}

#[test]
fn weekend_backoff_restores_genuine_golden_entry_without_estimated_spread() {
    let bars = history();
    let plan: TradePlan = serde_json::from_value(serde_json::json!({
        "trade_id": "ihs-audchf-2bba5dd0", "instrument": "AUDCHF",
        "direction": "long", "granularity": "h4", "pip_size": 0.0001,
        "armed_at": "2026-10-12T00:42:38.217309683Z",
        "rules": [{
            "rule_id": "05-enter", "kind": "enter", "fire_mode": "once",
            "trigger": {"type": "pine_pattern", "dir": "long"},
            "intent": {
                "v": 1, "id": "ihs-audchf-2bba5dd0-enter", "action": "enter",
                "trade_id": "ihs-audchf-2bba5dd0", "instrument": "AUDCHF",
                "direction": "long", "needs_golden": true, "max_retries": 5,
                "broker": "mt5", "account": "the5ers-competition",
                "pip_size": 0.0001, "tick_size": 0.00001,
                "entry_dedup": "gate_owned",
                "not_after": "2026-10-24T20:08:30.217309683Z",
                "entry": {"type": "stop", "from": "signal_high", "offset_atr_pct": 0.5, "recover_entry": {"action": "limit"}},
                "stop_loss": {"from": "signal_low", "offset_atr_pct": 0.5},
                "take_profit": {"absolute": 0.58706}
            }
        }]
    }))
    .unwrap();
    for (bar, expected_fires) in candidates(&bars).iter().zip([0, 1, 1]) {
        let now = bar.time + Duration::hours(4);
        let mut calls = 0;
        let window = pollster::block_on(fetch_window(since(bar), &[*bar], want(), |from| {
            calls += 1;
            std::future::ready(Ok(cached(&bars, from, now)))
        }))
        .unwrap();
        assert!(calls > 1, "must fetch across the weekend");
        assert!(window.iter().filter(|c| c.time < bar.time).count() >= want());
        let prior = seed_plan_state(
            &plan,
            &bars[..bars.iter().position(|c| c.time == bar.time).unwrap()],
            now + Duration::days(1),
        );
        let eval = evaluate_plan(
            &plan,
            &prior,
            &[*bar],
            &window,
            now,
            now + Duration::days(1),
        );
        let signal = latched_signal_at(
            &window,
            window.len() - 1,
            &DetectorConfig::pine_defaults(Granularity::H4),
        )
        .unwrap();
        tracing::info!(bar = %bar.time, golden = signal.golden, atr = ?signal.atr, fires = eval.fired.len(), "genuine price entry regression");
        assert_eq!(eval.fired.len(), expected_fires, "entry at {}", bar.time);
        if expected_fires > 0 {
            assert_eq!(eval.fired[0].rule_id, "05-enter");
        }
    }
}

#[test]
fn catchup_history_is_sized_before_the_earliest_fresh_bar() {
    let bars = history();
    let fresh = candidates(&bars);
    let window = pollster::block_on(fetch_window(since(&fresh[0]), fresh, want(), |from| {
        std::future::ready(Ok(cached(
            &bars,
            from,
            bars.last().unwrap().time + Duration::hours(4),
        )))
    }))
    .unwrap();
    assert!(window.iter().filter(|c| c.time < fresh[0].time).count() >= want());
    assert_eq!(&window[window.len() - fresh.len()..], fresh);
}

#[test]
fn complete_history_uses_one_fetch_and_merges_missing_fresh_bar() {
    let bars = history();
    let fresh = [*bars.last().unwrap()];
    let mut calls = 0;
    let window = pollster::block_on(fetch_window(
        bars[0].time - Duration::hours(4),
        &fresh,
        want(),
        |_| {
            calls += 1;
            std::future::ready(Ok(bars[..bars.len() - 1].to_vec()))
        },
    ))
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(window, bars);
}

#[test]
fn exhausted_or_duplicate_history_returns_available_bars_after_bounded_backoff() {
    let bars = history();
    let fresh = [*bars.last().unwrap()];
    for returned in [Vec::new(), vec![bars[0]; want() + 1]] {
        let mut calls = 0;
        let result = pollster::block_on(fetch_window(since(&fresh[0]), &fresh, want(), |_| {
            calls += 1;
            std::future::ready(Ok(returned.clone()))
        }));
        assert!(result.unwrap().len() < want());
        assert!(calls > 1 && calls <= 7, "bounded fetch attempts: {calls}");
    }
}

#[test]
fn short_history_keeps_price_veto_and_between_bar_controls_live() {
    use trade_control_core::plan_state::{Phase, PlanState};
    let bars = history();
    let fresh = [*bars.last().unwrap()];
    let now = fresh[0].time + Duration::hours(4) + Duration::minutes(30);
    let expires_at = now + Duration::days(1);
    let mut prior = PlanState::seed(Phase::AwaitEntry, expires_at);
    prior.watermark = Some(fresh[0].time - Duration::hours(4));
    let plan: TradePlan = serde_json::from_value(serde_json::json!({
        "trade_id": "short-history", "instrument": "AUDCHF", "direction": "long",
        "granularity": "h4", "pip_size": 0.0001,
        "rules": [
            {"rule_id": "pause", "kind": "control", "fire_mode": "once",
             "trigger": {"type": "time_reached", "at_epoch": now.timestamp()},
             "intent": {"v": 1, "id": "pause", "action": "pause", "instrument": "AUDCHF", "blackout_id": "news", "not_after": expires_at}},
            {"rule_id": "too-high", "kind": "setup_invalidation", "fire_mode": "once",
             "trigger": {"type": "price_value_cross", "level": fresh[0].o, "dir": "either", "bar": "intrabar"},
             "intent": {"v": 1, "id": "too-high", "action": "veto", "instrument": "AUDCHF", "name": "too-high", "level": "stop-next-entry", "not_after": expires_at}},
            {"rule_id": "enter", "kind": "enter", "fire_mode": "once",
             "trigger": {"type": "pine_pattern", "dir": "long"},
             "intent": {"v": 1, "id": "enter", "action": "enter", "instrument": "AUDCHF", "max_retries": 5, "not_after": expires_at}}
        ]
    })).unwrap();
    let window = pollster::block_on(fetch_window(since(&fresh[0]), &fresh, want(), |_| {
        std::future::ready(Ok(Vec::new()))
    }))
    .unwrap();
    let eval = evaluate_plan(&plan, &prior, &fresh, &window, now, expires_at);
    assert!(eval.fired.iter().any(|f| f.rule_id == "too-high"));
    assert!(!eval.fired.iter().any(|f| f.rule_id == "enter"));
    assert!(
        !eval.new_state.fired.contains("enter"),
        "entry must remain retryable"
    );
    assert_eq!(eval.new_state.watermark, Some(fresh[0].time));
    let control = trade_control_engine::evaluate_controls_only(
        &plan,
        &eval.new_state,
        &fresh[0],
        now,
        expires_at,
    );
    assert!(control.fired.iter().any(|f| f.rule_id == "pause"));
    assert_eq!(control.new_state.watermark, eval.new_state.watermark);
}

#[test]
fn newly_available_history_warms_from_incoming_bars() {
    let bars = history();
    for (available, warm) in [(&bars[..6], false), (&bars[..bars.len() - 1], true)] {
        let fresh = [*available.last().unwrap()];
        let end = fresh[0].time + Duration::hours(4);
        let window = pollster::block_on(fetch_window(since(&fresh[0]), &fresh, want(), |from| {
            std::future::ready(Ok(cached(available, from, end)))
        }))
        .unwrap();
        assert_eq!(
            wilder_atr(&window, atr_length_for(Granularity::H4)).is_some(),
            warm
        );
        if warm {
            let signal = latched_signal_at(
                &window,
                window.len() - 1,
                &DetectorConfig::pine_defaults(Granularity::H4),
            )
            .unwrap();
            assert!(
                signal.golden && signal.fires,
                "incoming bars restore golden detection"
            );
        }
    }
}

#[test]
fn history_fetch_failure_propagates_immediately() {
    let bars = history();
    let fresh = [*bars.last().unwrap()];
    let result = pollster::block_on(fetch_window(since(&fresh[0]), &fresh, want(), |_| {
        std::future::ready(Err("candle fetch failed (transient)".into()))
    }));
    assert_eq!(result.unwrap_err(), "candle fetch failed (transient)");
}

#[test]
fn trendline_only_window_does_not_require_pine_warmup() {
    let bars = history();
    let fresh = [*bars.last().unwrap()];
    let window = pollster::block_on(fetch_window(since(&fresh[0]), &fresh, 0, |_| {
        std::future::ready(Ok(Vec::new()))
    }))
    .unwrap();
    assert_eq!(window, fresh);
}
