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

fn enabled_plan() -> TradePlan {
    init();
    let p = plan(vec![pine_enter_rule(None, Direction::Short, false)]);
    let mut wire = serde_json::to_value(p).unwrap();
    wire["entry_conditions"] = serde_json::json!({
        "enable_on_ma_touch": [{"kind":"sma","period":3}]
    });
    serde_json::from_value(wire).unwrap()
}

fn touch_then_signal() -> Vec<Candle> {
    vec![
        candle("2026-06-16T08:00:00Z", 1.10, 1.10, 1.10, 1.10),
        candle("2026-06-16T09:00:00Z", 1.10, 1.10, 1.10, 1.10),
        candle("2026-06-16T10:00:00Z", 1.10, 1.10, 1.10, 1.10),
        // Bearish pinbar entirely above its 3 SMA (about 1.152).
        candle("2026-06-16T11:00:00Z", 1.26, 1.50, 1.24, 1.255),
    ]
}

#[test]
fn ma_touch_enables_a_later_signal_away_from_the_average() {
    let mut p = enabled_plan();
    p.entry_conditions
        .enable_on_ma_touch
        .insert(0, "200 sma".parse().unwrap());
    let window = touch_then_signal();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let first = run_window(&p, &prior, &window[2..3], &window[..3]);
    assert!(first.new_state.advanced_vs(&prior));
    assert!(
        first.fired.is_empty(),
        "touching the MA is not itself an entry signal"
    );
    let encoded = serde_json::to_string(&first.new_state).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded).unwrap()["ma_touch_seen_at"],
        "2026-06-16T10:00:00Z"
    );
    let saved = serde_json::from_str(&encoded).unwrap();
    let later = run_window(&p, &saved, &window[3..], &window);
    assert_eq!(
        later.fired.len(),
        1,
        "the later signal need not touch again"
    );
    assert!(
        !"3 sma"
            .parse::<trade_control_core::moving_average::MovingAverage>()
            .unwrap()
            .touches_pattern(&window, window[3].time, window[3].time)
    );
    let batch = run_window(&p, &prior, &window[2..], &window);
    assert_eq!(batch.new_state, later.new_state);
    assert_eq!(batch.fired.len(), later.fired.len());
}

#[test]
fn warmup_touch_does_not_enable_the_setup() {
    let p = enabled_plan();
    let window = touch_then_signal();
    let prior = seed_plan_state(&p, &window[..3], ts("2026-06-30T00:00:00Z"));
    let later = run_window(&p, &prior, &window[3..], &window);
    assert!(
        later.fired.is_empty(),
        "only the warmup touched, not any live candle"
    );
    assert!(!later.done, "keep waiting for a live touch");
    assert!(
        enter_preconditions_by_leg(&p, &later.new_state, window[3].time, Some(&window))
            .iter()
            .any(|leg| leg.contains(&EnterBlock::NeedsEnablingMaTouch))
    );
}

#[test]
fn touch_and_signal_can_share_a_candle_but_must_touch_remains_per_signal() {
    let p = enabled_plan();
    let window = bearish_pinbar_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    let same_bar = run_window(&p, &prior, &window[3..], &window);
    assert_eq!(same_bar.fired.len(), 1);
    assert_eq!(same_bar.new_state.ma_touch_seen_at, Some(window[3].time));

    let mut both = enabled_plan();
    both.entry_conditions.must_touch = vec!["3 sma".parse().unwrap()];
    let away = touch_then_signal();
    let first = run_window(&both, &prior, &away[2..3], &away[..3]);
    let later = run_window(&both, &first.new_state, &away[3..], &away);
    assert!(
        later.fired.is_empty(),
        "enablement cannot bypass the separate signal-touch filter"
    );
    assert!(later.new_state.ma_touch_seen_at.is_some());
}

#[test]
fn wick_touch_enables_without_needing_a_close_at_the_ma() {
    let p = enabled_plan();
    let mut window = touch_then_signal();
    window[2] = candle("2026-06-16T10:00:00Z", 1.12, 1.13, 1.10, 1.12);
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let first = run_window(&p, &prior, &window[2..3], &window[..3]);
    assert_eq!(first.new_state.ma_touch_seen_at, Some(window[2].time));
    let later = run_window(&p, &first.new_state, &window[3..], &window);
    assert_eq!(later.fired.len(), 1);
}

#[test]
fn partially_pre_arm_candle_cannot_enable_a_later_signal() {
    let mut p = enabled_plan();
    let window = touch_then_signal();
    p.armed_at = Some(ts("2026-06-16T10:30:00Z"));
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let eval = run_window(&p, &prior, &window[2..], &window);
    assert!(
        eval.new_state.ma_touch_seen_at.is_none(),
        "the 10:00 wick could have occurred before the 10:30 arm"
    );
    assert!(eval.fired.is_empty());
}

#[test]
fn a_signal_printed_before_enablement_cannot_enter_when_it_confirms() {
    let mut p = enabled_plan();
    p.rules[0].intent.needs_confirmed = true;
    p.rules[0].intent.entry = Some(trade_control_core::intent::EntrySpec::Limit {
        from: trade_control_core::intent::PriceAnchor::SignalLow,
        offset_pips: 0.0,
        offset_atr_pct: None,
        at: None,
        recover_entry: None,
    });
    let mut window = two_short_engulfers_window();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let first = run_window(&p, &prior, &window[1..], &window);
    assert_eq!(first.new_state.ma_touch_seen_at, Some(window[2].time));
    assert!(
        first.fired.is_empty(),
        "first signal printed before the enabling touch"
    );
    window.push(candle("2026-06-16T13:00:00Z", 105.3, 106.0, 102.0, 103.0));
    let later = run_window(&p, &first.new_state, &window[4..], &window);
    assert_eq!(later.fired.len(), 1);
    assert_eq!(
        later.fired[0].signal.unwrap().signal_bar_time,
        window[2].time
    );
}

#[test]
fn cold_200_sma_blocks_then_a_live_touch_with_history_enables() {
    let mut p = enabled_plan();
    p.entry_conditions.enable_on_ma_touch = vec!["200 sma".parse().unwrap()];
    let short = touch_then_signal();
    let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
    let cold = run_window(&p, &prior, &short[2..], &short);
    assert!(cold.fired.is_empty());
    assert!(cold.new_state.ma_touch_seen_at.is_none());

    let start = ts("2026-06-01T00:00:00Z");
    let mut window: Vec<_> = (0..=200)
        .map(|i| Candle {
            time: start + chrono::Duration::hours(i),
            o: 1.10,
            h: 1.101,
            l: 1.099,
            c: 1.10,
        })
        .collect();
    let mut signal = short[3];
    signal.time = start + chrono::Duration::hours(201);
    window.push(signal);
    p.armed_at = Some(window[200].time);
    let mut prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    prior.watermark = Some(window[199].time);
    let eval = run_window(&p, &prior, &window[200..], &window);
    assert_eq!(eval.new_state.ma_touch_seen_at, Some(window[200].time));
    assert_eq!(eval.fired.len(), 1);
}
