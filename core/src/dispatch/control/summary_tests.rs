use super::*;
use crate::plan_state::{Phase, PlanState};
use crate::trade_plan::TradePlan;
use tracing_subscriber::prelude::*;

#[test]
fn entries_blocked_survives_live_and_archived_plan_summaries() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let now = Utc::now();
    let plan: TradePlan = serde_json::from_value(serde_json::json!({
        "trade_id":"hs-audnzd-test", "instrument":"AUDNZD", "direction":"short",
        "granularity":"m15", "pip_size":0.0001, "rules":[]
    }))
    .expect("plan");
    let stored = StoredPlan {
        account: None,
        plan: plan.clone(),
    };
    let mut state = PlanState::seed(Phase::AwaitEntry, now);
    state.entries_blocked = true;
    let summary =
        serde_json::to_value(plan_summary(&stored, Some(state.clone()))).expect("summary");
    assert_eq!(summary["entries_blocked"], true);
    assert_eq!(summary["phase"], "await_entry");

    state.phase = Phase::Done;
    let archived = ArchivedPlan {
        account: None,
        plan,
        final_state: state,
        archived_at: now,
        settlement: None,
    };
    let summary = serde_json::to_value(archived_plan_summary(&archived)).expect("archive summary");
    assert_eq!(summary["entries_blocked"], true);
    assert_eq!(summary["phase"], "done");
    let unseeded = serde_json::to_value(plan_summary(&stored, None)).expect("unseeded summary");
    assert_eq!(unseeded["entries_blocked"], false);
}
