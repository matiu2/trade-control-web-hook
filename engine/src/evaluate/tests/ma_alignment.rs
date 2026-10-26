use super::*;

fn setup(direction: Direction) -> (TradePlan, Vec<Candle>, PlanState) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let entry = rule(
        "05-enter",
        Trigger::MwEveryBar,
        FireMode::EveryBar,
        Action::Enter,
    );
    let mut p = plan(vec![entry]);
    p.direction = direction;
    let mut json = serde_json::to_value(p).unwrap();
    json["entry_conditions"] = serde_json::json!({"require_ma_alignment":true});
    let p = serde_json::from_value(json).unwrap();
    let start = ts("2026-06-01T00:00:00Z");
    let window = (0..180)
        .map(|i| {
            let c = match i {
                0..110 => 100.0 - i as f64 * 0.1,
                110..140 => 89.0 + (i - 110) as f64,
                _ => 119.0 - (i - 140) as f64,
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
    (
        p,
        window,
        PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z")),
    )
}

#[test]
fn alignment_pauses_and_resumes_repeatedly_without_latching_a_veto() {
    for direction in [Direction::Long, Direction::Short] {
        let (p, window, mut state) = setup(direction);
        for index in [100, 139, 179] {
            let favorable = (direction == Direction::Short) == (index != 139);
            let eval = run_window(&p, &state, &window[index..=index], &window);
            assert_eq!(
                eval.fired[0].intent.action,
                if favorable {
                    Action::Resume
                } else {
                    Action::Pause
                }
            );
            assert_eq!(
                eval.fired[0].intent.blackout_id.as_deref(),
                Some("ma-alignment")
            );
            assert_eq!(
                eval.fired
                    .iter()
                    .filter(|f| f.intent.action == Action::Enter)
                    .count(),
                usize::from(favorable)
            );
            assert!(!eval.new_state.entries_blocked);
            assert!(!eval.done);
            state = serde_json::from_str(&serde_json::to_string(&eval.new_state).unwrap()).unwrap();
        }
    }
}

#[test]
fn cold_alignment_pauses_but_does_not_disable_other_guards() {
    let (mut p, window, prior) = setup(Direction::Short);
    let cold = run_window(&p, &prior, &window[100..101], &window[95..101]);
    assert_eq!(cold.fired.len(), 1);
    assert_eq!(cold.fired[0].intent.action, Action::Pause);
    assert!(!cold.done);
    p.rules.push(rule(
        "veto-other",
        Trigger::MwEveryBar,
        FireMode::Once,
        Action::Veto,
    ));
    let vetoed = run_window(&p, &prior, &window[100..101], &window);
    assert!(vetoed.fired.iter().any(|f| f.intent.action == Action::Veto));
    assert!(vetoed.new_state.entries_blocked);
    let recovered = run_window(&p, &vetoed.new_state, &window[179..180], &window);
    assert!(
        !recovered
            .fired
            .iter()
            .any(|f| matches!(f.intent.action, Action::Enter | Action::Resume))
    );
    assert!(recovered.new_state.entries_blocked);
}

#[test]
fn single_shot_entry_keeps_monitoring_for_pending_order_pauses() {
    let (mut p, window, prior) = setup(Direction::Short);
    p.rules = vec![enter_rule("05-enter", &[], 0)];
    let entered = run_window(&p, &prior, &window[100..101], &window);
    assert!(
        entered
            .fired
            .iter()
            .any(|f| f.intent.action == Action::Enter)
    );
    assert!(!entered.done);
    let paused = run_window(&p, &entered.new_state, &window[139..140], &window);
    assert_eq!(paused.fired.len(), 1);
    assert_eq!(paused.fired[0].intent.action, Action::Pause);
    let resumed = run_window(&p, &paused.new_state, &window[179..180], &window);
    assert_eq!(
        resumed.fired.len(),
        1,
        "restore the held order, do not submit a second entry"
    );
    assert_eq!(resumed.fired[0].intent.action, Action::Resume);
}
