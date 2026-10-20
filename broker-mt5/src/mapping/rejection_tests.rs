use crate::{mapping, tests::logging};
use mt5_data_source::web::trading::{Lots, OutcomeState, TradeOutcome, TradeResult};
use trade_control_core::{broker::EntryError, recover_entry::outcome_for_entry_error};

fn rejected(result: Option<TradeResult>) -> TradeOutcome {
    TradeOutcome {
        request_id: 42,
        state: OutcomeState::Rejected,
        submission_code: Some(if result.is_some() { 0 } else { 10017 }),
        result,
        uncertainty: None,
    }
}

#[test]
fn final_rejection_reaches_the_timeline_with_broker_evidence() {
    logging();
    let outcome = rejected(Some(TradeResult {
        code: 10019,
        deal: 0,
        order: 0,
        volume_raw: 0,
        price: 0.0,
        bid: 0.69869,
        ask: 0.69872,
        comment: "broker diagnostic fixture".into(),
    }));
    let error =
        mapping::placement(outcome, Lots::from_raw(68_000_000).unwrap(), 0.69904).unwrap_err();
    assert!(matches!(error, EntryError::OrderRejectedWithDetail(_)));
    let timeline = outcome_for_entry_error(&error);
    for evidence in [
        "entry-failed: broker-rejected (",
        "request_id=42",
        "submission_code=0",
        "result_code=10019",
        "broker diagnostic fixture",
        "bid=0.69869",
        "ask=0.69872",
    ] {
        assert!(
            timeline.contains(evidence),
            "missing {evidence}: {timeline}"
        );
    }
}

#[test]
fn acknowledgment_rejection_does_not_invent_a_final_result() {
    logging();
    let error = mapping::placement(rejected(None), Lots::from_raw(1_000_000).unwrap(), 0.69904)
        .unwrap_err();
    let timeline = outcome_for_entry_error(&error);
    assert!(timeline.starts_with("entry-failed: broker-rejected ("));
    assert!(timeline.contains("request_id=42"));
    assert!(timeline.contains("submission_code=10017"));
    assert!(timeline.contains("final result unavailable"));
    assert!(!timeline.contains("result_code=0"));
}
