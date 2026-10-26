use super::*;
use tracing_subscriber::prelude::*;

fn setup() -> (TradePlan, Vec<Candle>) {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let start = ts("2026-06-01T00:00:00Z");
    let window: Vec<_> = (0..205)
        .map(|i| {
            let c = if i < 203 {
                100.0
            } else if i == 203 {
                99.0
            } else {
                101.0
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
    let mut guard = pine_enter_rule(None, Direction::Short, false);
    guard.rule_id = "01-veto-sma-200-break".into();
    guard.trigger = Trigger::AdverseMaBreak {
        average: "200 sma".parse().unwrap(),
        dir: Direction::Short,
    };
    guard.kind = RuleKind::SetupInvalidation;
    guard.intent.action = Action::Veto;
    guard.intent.name = Some("sma-200-break".into());
    guard.intent.level = Some(trade_control_core::intent::VetoLevel::CancelPending);
    let mut p = plan(vec![guard]);
    p.armed_at = Some(window[201].time);
    (p, window)
}
#[test]
fn sma_200_break_veto_latches_across_restart_without_retiring_exit_guards() {
    let (p, window) = setup();
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let first = run_window(&p, &prior, &window[201..204], &window);
    assert!(
        first.fired.is_empty(),
        "a favourable close below the SMA must not veto short"
    );
    let hit = run_window(&p, &first.new_state, &window[204..], &window);
    assert_eq!(hit.fired.len(), 1);
    assert!(hit.new_state.entries_blocked);
    assert!(!hit.done);
    let saved = serde_json::from_str(&serde_json::to_string(&hit.new_state).unwrap()).unwrap();
    let replayed = run_window(&p, &saved, &window[204..], &window);
    assert!(replayed.fired.is_empty());
    assert!(replayed.new_state.entries_blocked);
    let batch = run_window(&p, &prior, &window[201..], &window);
    assert_eq!(batch.new_state, hit.new_state);
    assert_eq!(p.ma_veto_history_bars(), 201);
}
#[test]
fn sma_200_break_veto_ignores_pre_arm_breaks_and_wicks() {
    let (mut p, mut window) = setup();
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    p.armed_at = Some(window[204].time + chrono::Duration::hours(1));
    assert!(
        run_window(&p, &prior, &window[201..], &window)
            .fired
            .is_empty()
    );
    p.armed_at = Some(window[201].time);
    window[204].c = 99.0;
    window[204].h = 110.0;
    assert!(
        run_window(&p, &prior, &window[201..], &window)
            .fired
            .is_empty()
    );
}
#[test]
fn sma_200_break_veto_blocks_cold_entries_and_keeps_single_shot_monitoring() {
    let (mut p, window) = setup();
    p.rules.push(enter_rule("05-enter", &[], 0));
    let prior = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
    let cold = run_window(&p, &prior, &window[201..202], &window[2..202]);
    assert!(
        cold.fired.is_empty(),
        "200 bars cannot establish both sides of the break"
    );
    let entered = run_window(&p, &prior, &window[201..202], &window);
    assert_eq!(entered.fired.len(), 1);
    assert!(!entered.done);
    let hit = run_window(&p, &entered.new_state, &window[202..], &window);
    assert_eq!(hit.fired.len(), 1);
    assert_eq!(hit.fired[0].intent.action, Action::Veto);
}
