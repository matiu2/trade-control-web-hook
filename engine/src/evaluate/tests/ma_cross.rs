use super::*;
use tracing_subscriber::prelude::*;

fn setup() -> (TradePlan, Vec<Candle>, usize) {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    let candles: Vec<_> = (0..140)
        .map(|i| {
            let c = if i < 110 {
                100.0 - i as f64 * 0.1
            } else {
                89.0 + (i - 110) as f64
            };
            Candle {
                time: start + chrono::Duration::hours(i),
                o: c,
                h: c + 0.1,
                l: c - 0.1,
                c,
            }
        })
        .collect();
    let cross = (0..candles.len())
        .find(|&i| trade_control_core::ma_cross::adverse_cross_at(&candles, i, Direction::Short))
        .unwrap();
    let mut guard = pine_enter_rule(None, Direction::Short, false);
    guard.rule_id = "01-veto-ma-cross".into();
    guard.trigger = Trigger::AdverseMaCross {
        dir: Direction::Short,
    };
    guard.kind = RuleKind::SetupInvalidation;
    guard.intent.action = Action::Veto;
    guard.intent.name = Some("ma-cross".into());
    guard.intent.level = Some(trade_control_core::intent::VetoLevel::CancelPending);
    let mut p = plan(vec![guard]);
    p.armed_at = Some(candles[100].time);
    (p, candles, cross)
}

#[test]
fn adverse_cross_latches_veto_without_retiring_position_monitoring() {
    let (p, window, cross) = setup();
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let before = run_window(&p, &prior, &window[100..cross], &window);
    assert!(before.fired.is_empty());
    let hit = run_window(&p, &before.new_state, &window[cross..=cross], &window);
    assert_eq!(hit.fired.len(), 1);
    assert_eq!(hit.fired[0].intent.action, Action::Veto);
    assert!(hit.new_state.entries_blocked);
    assert!(!hit.done);
    let saved = serde_json::from_str(&serde_json::to_string(&hit.new_state).unwrap()).unwrap();
    let later = run_window(&p, &saved, &window[cross + 1..], &window);
    assert!(later.fired.is_empty());
    assert!(later.new_state.entries_blocked);
    let batch = run_window(&p, &prior, &window[100..], &window);
    assert_eq!(batch.new_state, later.new_state);
    assert_eq!(batch.fired.len(), 1);
}

#[test]
fn pre_arm_and_favourable_crosses_do_not_veto() {
    let (mut p, window, cross) = setup();
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    p.armed_at = Some(window[cross].time + chrono::Duration::hours(1));
    assert!(
        run_window(&p, &prior, &window[100..], &window)
            .fired
            .is_empty()
    );
    p.armed_at = None;
    p.rules[0].trigger = Trigger::AdverseMaCross {
        dir: Direction::Long,
    };
    assert!(
        run_window(&p, &prior, &window[100..], &window)
            .fired
            .is_empty()
    );
}

#[test]
fn cold_ma_history_blocks_entry_without_disabling_guards() {
    let (mut p, window, cross) = setup();
    let mut entry = pine_enter_rule(None, Direction::Short, false);
    entry.trigger = Trigger::MwEveryBar;
    p.rules.push(entry);
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let result = run_window(
        &p,
        &prior,
        &window[cross..=cross],
        &window[cross - 5..=cross],
    );
    assert!(result.fired.is_empty());
    assert!(!result.done);
}

#[test]
fn a_single_shot_entry_stays_monitored_until_its_later_ma_cross() {
    let (mut p, window, cross) = setup();
    p.rules.push(enter_rule("05-enter", &[], 0));
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let entered = run_window(&p, &prior, &window[100..101], &window);
    assert_eq!(entered.fired.len(), 1);
    assert_eq!(entered.fired[0].intent.action, Action::Enter);
    assert!(!entered.done, "a resting order still needs the MA veto");
    assert!(entered.new_state.fired.contains("05-enter"));
    let later = run_window(&p, &entered.new_state, &window[101..=cross], &window);
    assert_eq!(
        later.fired.len(),
        1,
        "the single-shot entry must not re-fire"
    );
    assert_eq!(later.fired[0].intent.action, Action::Veto);
    assert!(later.new_state.entries_blocked);
    assert!(!later.done);
}
