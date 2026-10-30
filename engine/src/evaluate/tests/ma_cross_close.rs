use super::*;
use tracing_subscriber::prelude::*;

fn setup(direction: Direction) -> (TradePlan, Vec<Candle>, Vec<usize>) {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    let candles: Vec<_> = (0..340)
        .map(|i| {
            let close = match i {
                0..110 => 100.0 - i as f64 * 0.1,
                110..170 => 89.0 + (i - 110) as f64,
                170..230 => 149.0 - (i - 170) as f64,
                230..290 => 89.0 + (i - 230) as f64,
                _ => 149.0 - (i - 290) as f64,
            };
            Candle {
                time: start + chrono::Duration::hours(i),
                o: close,
                h: close + 0.1,
                l: close - 0.1,
                c: close,
            }
        })
        .collect();
    let crosses = (100..candles.len())
        .filter(|&i| trade_control_core::ma_cross::adverse_cross_at(&candles, i, direction))
        .collect();
    let mut close = enter_rule("08-close-on-ma-cross", &[], 0);
    close.trigger = Trigger::AdverseMaCross { dir: direction };
    close.intent = intent(Action::Close);
    close.kind = RuleKind::PerPositionClose;
    close.fire_mode = FireMode::EveryBar;
    let mut p = plan(vec![close]);
    p.direction = direction;
    p.armed_at = Some(candles[100].time);
    (p, candles, crosses)
}

#[test]
fn close_cross_repeats_without_vetoing_entries_and_survives_restart() {
    for direction in [Direction::Long, Direction::Short] {
        let (p, candles, crosses) = setup(direction);
        assert_eq!(crosses.len(), 2);
        let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
        let first = run_window(&p, &prior, &candles[100..=crosses[0]], &candles);
        assert_eq!(first.fired.len(), 1);
        assert_eq!(first.fired[0].intent.action, Action::Close);
        assert!(!first.new_state.entries_blocked);
        assert!(!first.done);
        let saved =
            serde_json::from_str(&serde_json::to_string(&first.new_state).unwrap()).unwrap();
        let later = run_window(&p, &saved, &candles[crosses[0] + 1..], &candles);
        assert_eq!(
            later.fired.len(),
            1,
            "a later position also needs protection"
        );
        assert_eq!(later.fired[0].candle.time, candles[crosses[1]].time);
        assert!(!later.new_state.entries_blocked);
        assert!(!later.done);
        let batch = run_window(&p, &prior, &candles[100..], &candles);
        assert_eq!(batch.fired.len(), 2);
        assert_eq!(batch.new_state, later.new_state);
    }
}

#[test]
fn close_cross_never_gates_entries_even_with_cold_history() {
    let (mut p, candles, crosses) = setup(Direction::Short);
    let mut entry = enter_rule("05-enter", &[], 0);
    entry.trigger = Trigger::MwEveryBar;
    p.rules.push(entry);
    assert!(!p.has_ma_veto());
    assert!(p.detector_history_bars() >= trade_control_core::ma_cross::HISTORY_BARS);
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let entered = run_window(&p, &prior, &candles[100..101], &candles[100..101]);
    assert_eq!(entered.fired.len(), 1);
    assert_eq!(entered.fired[0].intent.action, Action::Enter);
    assert!(
        !entered.done,
        "single-shot entry must retain MA close monitoring"
    );
    let later = run_window(&p, &entered.new_state, &candles[101..=crosses[0]], &candles);
    assert_eq!(later.fired.len(), 1);
    assert_eq!(later.fired[0].intent.action, Action::Close);
    assert!(!later.new_state.entries_blocked);
}

#[test]
fn close_cross_ignores_pre_arm_events_and_stays_active_after_entry_veto() {
    let (mut p, candles, crosses) = setup(Direction::Short);
    p.armed_at = Some(candles[crosses[0] + 1].time);
    let mut prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    prior.entries_blocked = true;
    let result = run_window(&p, &prior, &candles[100..], &candles);
    assert_eq!(result.fired.len(), 1);
    assert_eq!(result.fired[0].candle.time, candles[crosses[1]].time);
    assert!(!result.done);
}
