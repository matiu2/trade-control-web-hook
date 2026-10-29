use super::*;

fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn with_retrace(mut p: TradePlan, level: f64) -> TradePlan {
    init();
    let mut json = serde_json::to_value(&p).expect("serialize plan");
    json["entry_conditions"] = serde_json::json!({
        "retrace": {
            "a": {"at_epoch": ts("2026-06-16T08:00:00Z").timestamp(), "price": level},
            "b": {"at_epoch": ts("2026-06-16T09:00:00Z").timestamp(), "price": level}
        }
    });
    p = serde_json::from_value(json).expect("read plan with retracement requirement");
    p
}

#[test]
fn short_retrace_wick_is_insufficient_even_with_a_signal() {
    let p = with_retrace(
        plan(vec![pine_enter_rule(None, Direction::Short, false)]),
        1.20,
    );
    let window = bearish_pinbar_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert!(
        eval.fired.is_empty(),
        "high 1.30 passes, but close 1.115 does not"
    );
    assert!(!eval.done, "a shallow retracement leaves the setup waiting");
}

#[test]
fn short_retrace_close_allows_same_bar_signal_without_retest() {
    let p = with_retrace(
        plan(vec![pine_enter_rule(None, Direction::Short, false)]),
        1.11,
    );
    let window = bearish_pinbar_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert_eq!(
        eval.fired.len(),
        1,
        "close 1.115 qualifies the retracement and signal"
    );
}

#[test]
fn long_retrace_needs_a_close_below_the_line() {
    let enter = rule(
        "05-enter",
        Trigger::MwEveryBar,
        FireMode::EveryBar,
        Action::Enter,
    );
    let mut p = with_retrace(plan(vec![enter]), 1.10);
    p.direction = Direction::Long;
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let wick = candle("2026-06-16T11:00:00Z", 1.12, 1.13, 1.08, 1.11);
    let first = run(&p, &prior, &[wick]);
    assert!(
        first.fired.is_empty(),
        "wick below is insufficient for a long too"
    );
    let close = candle("2026-06-16T12:00:00Z", 1.11, 1.12, 1.08, 1.09);
    let second = run(&p, &first.new_state, &[close]);
    assert_eq!(second.fired.len(), 1);
}

#[test]
fn retrace_close_persists_across_ticks_and_allows_signal_back_below_line() {
    let p = with_retrace(
        plan(vec![pine_enter_rule(None, Direction::Short, false)]),
        1.20,
    );
    let mut window = bearish_pinbar_window();
    window[2] = candle("2026-06-16T10:00:00Z", 1.201, 1.22, 1.20, 1.219);
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let first = run_window(&p, &prior, &window[2..3], &window[..3]);
    assert!(first.fired.is_empty(), "retracement alone is not a signal");
    assert_eq!(first.new_state.retrace_seen_at, Some(window[2].time));
    assert!(first.new_state.advanced_vs(&prior));
    let saved = serde_json::to_string(&first.new_state).unwrap();
    let restored = serde_json::from_str(&saved).unwrap();
    let second = run_window(&p, &restored, &window[3..], &window);
    assert_eq!(
        second.fired.len(),
        1,
        "signal closes below the already-satisfied line"
    );
    let batch = run_window(&p, &prior, &window[2..], &window);
    assert_eq!(batch.new_state, second.new_state);
    assert_eq!(batch.fired.len(), second.fired.len());
}

#[test]
fn pre_arm_history_does_not_satisfy_retracement() {
    let p = with_retrace(
        plan(vec![pine_enter_rule(None, Direction::Short, false)]),
        1.20,
    );
    let mut window = bearish_pinbar_window();
    window[2].c = 1.21;
    let prior = seed_plan_state(&p, &window[..3], ts("2026-06-30T00:00:00Z"));
    assert!(prior.retrace_seen_at.is_none());
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert!(eval.fired.is_empty());
}

#[test]
fn confirmed_signal_printed_before_retracement_cannot_enter_afterward() {
    let mut enter = pine_enter_rule(None, Direction::Short, false);
    enter.intent.needs_confirmed = true;
    let p = with_retrace(plan(vec![enter]), 115.0);
    let mut window = two_short_engulfers_window();
    // The first signal prints at 10:00 below the retracement threshold; its
    // confirmation candle alone closes above it. That old signal is ineligible.
    window[3] = candle("2026-06-16T12:00:00Z", 105.0, 119.0, 104.0, 116.0);
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert_eq!(eval.new_state.retrace_seen_at, Some(window[3].time));
    assert!(eval.fired.is_empty());
}

#[test]
fn ma_filter_is_any_of_and_fails_closed_without_history() {
    init();
    let base = plan(vec![pine_enter_rule(None, Direction::Short, false)]);
    let window = bearish_pinbar_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let mut p = base.clone();
    p.entry_conditions.must_touch = vec!["200 sma".parse().unwrap()];
    let cold = run_window(&p, &prior, &window[3..], &window);
    assert!(cold.fired.is_empty());
    assert!(!cold.done);
    p.entry_conditions.must_touch.push("3 sma".parse().unwrap());
    let warm = run_window(&p, &prior, &window[3..], &window);
    assert_eq!(warm.fired.len(), 1, "one of the listed MAs suffices");
    let legacy = run_window(&base, &prior, &window[3..], &window);
    assert_eq!(legacy.fired.len(), warm.fired.len());
}

#[test]
fn offside_short_enters_fully_above_average_while_legacy_touch_does_not() {
    init();
    let mut p = plan(vec![pine_enter_rule(None, Direction::Short, false)]);
    p.entry_conditions.be_offside = vec!["9 ema".parse().unwrap()];
    let signal = bearish_pinbar_window();
    let mut window: Vec<_> = (0..90)
        .map(|i| Candle {
            time: signal[0].time - chrono::Duration::hours(90 - i),
            o: 0.5,
            h: 0.5,
            l: 0.5,
            c: 0.5,
        })
        .collect();
    window.extend(signal);
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let eval = run_window(&p, &prior, &window[93..], &window);
    assert_eq!(eval.fired.len(), 1);
    p.entry_conditions.must_touch = std::mem::take(&mut p.entry_conditions.be_offside);
    let strict = run_window(&p, &prior, &window[93..], &window);
    assert!(strict.fired.is_empty());
}

#[test]
fn offside_is_enforced_during_confirmation_and_blocks_non_pattern_entries() {
    init();
    let mut enter = pine_enter_rule(None, Direction::Short, false);
    enter.intent.needs_confirmed = true;
    let mut p = plan(vec![enter]);
    p.entry_conditions.be_offside = vec!["200 sma".parse().unwrap()];
    let window = two_short_engulfers_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert!(
        eval.fired.is_empty(),
        "a confirmed signal still needs MA history"
    );
    p.rules[0].trigger = Trigger::MwEveryBar;
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert!(
        eval.fired.is_empty(),
        "non-pattern entry cannot bypass the filter"
    );
}

#[test]
fn ma_confirmation_touch_cannot_qualify_an_earlier_pattern() {
    init();
    let mut enter = pine_enter_rule(None, Direction::Short, false);
    enter.intent.needs_confirmed = true;
    enter.intent.entry = Some(trade_control_core::intent::EntrySpec::Limit {
        from: trade_control_core::intent::PriceAnchor::SignalLow,
        offset_pips: 0.0,
        offset_atr_pct: None,
        at: None,
        recover_entry: None,
    });
    enter.intent.take_profit = Some(trade_control_core::intent::TakeProfit::Anchored(
        trade_control_core::intent::PriceRef::Absolute { absolute: 90.0 },
    ));
    let mut p = plan(vec![enter]);
    // The first two bars of a 4-bar window cannot have a 3 SMA yet.
    // The confirmation bar can touch its SMA, but it is outside the pattern.
    p.entry_conditions.must_touch = vec!["3 sma".parse().unwrap()];
    let mut window = two_short_engulfers_window();
    window[3].h = 108.0; // This confirmation candle touches its 3 SMA.
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
    let eval = run_window(&p, &prior, &window[3..], &window);
    assert!(eval.fired.is_empty());
    assert!(!eval.done);
    window.push(candle("2026-06-16T13:00:00Z", 105.3, 106.0, 102.0, 103.0));
    let next = run_window(&p, &eval.new_state, &window[4..], &window);
    assert_eq!(
        next.fired.len(),
        1,
        "the rejected first confirmation cannot hide the next qualifying signal"
    );
    assert_eq!(
        next.fired[0].signal.unwrap().signal_bar_time,
        window[2].time
    );
}
