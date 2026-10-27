use super::*;
use tracing_subscriber::prelude::*;

fn logging() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn broker_error() -> &'static str {
    "broker rejected the order: MT5 request_id=1857616817 submission_code=0 result_code=10040 comment=\"\" bid=0 ask=0"
}

fn recovery() -> String {
    format!(
        "stop rejected: Short Stop at 0.8372900000000001 blocked before submission: bid=0.83734 ask=0.83737 distance=0.00005000 minimum=0.0001; \
         limit rejected: Short Limit at 0.8372900000000001 blocked before submission: bid=0.83735 ask=0.83738 distance=-0.00006000 minimum=0.0001; \
         market fallback: fresh bid=0.83734 ask=0.83737 trigger=0.83729 maximum_distance=0.0001; market result: {}",
        broker_error()
    )
}

#[test]
fn eurgbp_warning_numbers_local_blocks_and_one_broker_rejection() {
    logging();
    let outcome = format!(
        "entry-failed: broker-rejected ({}); recovery: {}",
        broker_error(),
        recovery()
    );
    let result = ActionResult::Failed(outcome.clone());
    let warning = result.entry_warning();
    assert!(
        warning.starts_with("entry was not confirmed.\nFinal result: BROKER REJECTION"),
        "{warning}"
    );
    [
        "1. LOCAL BLOCK — Stop entry was not submitted",
        "2. LOCAL BLOCK — Limit entry was not submitted",
        "3. MARKET FALLBACK — Local proximity check passed",
        "BROKER REJECTION — broker rejected the order",
        "distance=0.00005000 minimum=0.0001",
        "distance=-0.00006000 minimum=0.0001",
        "maximum_distance=0.0001",
        "trigger is closer to the market than the minimum allowed distance",
        "trigger is on the wrong side of the market",
    ]
    .into_iter()
    .for_each(|part| assert!(warning.contains(part), "missing {part}: {warning}"));
    assert_eq!(
        warning.matches("request_id=1857616817").count(),
        1,
        "{warning}"
    );
    assert_eq!(result.record_outcome(), outcome);
}

#[test]
fn market_local_guard_is_never_described_as_a_broker_rejection() {
    logging();
    let cause = "fresh entry price 0.8375 is outside the market fallback band: trigger=0.83729 maximum_distance=0.0001";
    let recovery = recovery()
        .split("; market result:")
        .next()
        .unwrap()
        .to_owned();
    let result = ActionResult::Failed(format!(
        "entry-failed: market-recovery-declined ({cause}); recovery: {recovery}; market result: {cause}"
    ));
    let warning = result.entry_warning();
    assert!(warning.contains("Final result: LOCAL BLOCK"), "{warning}");
    assert!(
        warning.contains("LOCAL BLOCK — fresh entry price"),
        "{warning}"
    );
    assert!(!warning.contains("BROKER REJECTION"), "{warning}");
    assert_eq!(warning.matches(cause).count(), 1, "{warning}");
}

#[test]
fn account_read_failure_stops_recovery_without_claiming_a_broker_rejection() {
    logging();
    let result = ActionResult::Failed(
        "entry-failed: account-fetch (failed to fetch account); recovery: stop rejected: Short Stop at 0.83729 blocked before submission: bid=0.83734 ask=0.83737 distance=0.00005 minimum=0.0001; limit failed: failed to fetch account; market not attempted: failure is not verified proximity".into()
    );
    let warning = result.entry_warning();
    assert!(warning.contains("Final result: LOCAL ERROR"), "{warning}");
    assert!(
        warning.contains("2. LOCAL ERROR — Limit entry"),
        "{warning}"
    );
    assert!(
        warning.contains("Market fallback was not attempted"),
        "{warning}"
    );
    assert!(!warning.contains("BROKER REJECTION"), "{warning}");
    assert!(!warning.contains("3. MARKET FALLBACK"), "{warning}");
}

#[test]
fn ambiguous_market_preserves_the_need_to_reconcile_before_retrying() {
    logging();
    let cause = "entry submission outcome is unknown; an order may already exist; reconcile with the broker before retrying (MT5 timeout after send)";
    let recovery = recovery()
        .split("; market result:")
        .next()
        .unwrap()
        .to_owned();
    let result = ActionResult::AmbiguousSuccess(format!(
        "entry-failed: ambiguous-success ({cause}); recovery: {recovery}; market result: {cause}"
    ));
    let warning = result.entry_warning();
    assert!(
        warning.contains("Final result: OUTCOME UNKNOWN"),
        "{warning}"
    );
    assert!(warning.contains("an order may already exist"), "{warning}");
    assert!(
        warning.contains("reconcile with the broker before retrying"),
        "{warning}"
    );
    assert!(!warning.contains("BROKER REJECTION"), "{warning}");
}

#[test]
fn plain_local_rejection_shows_the_gate_and_its_explanation() {
    logging();
    let result = ActionResult::Rejected {
        status: 422,
        body: "entry blocked: stop loss is too close to the spread".into(),
        outcome: "rejected: sl-widen-below-min-r (spread=0.00003)".into(),
    };
    let warning = result.entry_warning();
    assert!(warning.contains("Final result: LOCAL BLOCK"), "{warning}");
    assert!(
        warning.contains("stop loss is too close to the spread"),
        "{warning}"
    );
    assert!(warning.contains("sl-widen-below-min-r"), "{warning}");
    assert!(!warning.contains("BROKER REJECTION"), "{warning}");
}

#[test]
fn broker_diagnostics_with_semicolons_remain_one_result() {
    logging();
    let cause = "broker rejected the order: MT5 result_code=10040 comment=\"limit reached; retry later; market not attempted: diagnostic\"";
    let recovery = recovery()
        .split("; market result:")
        .next()
        .unwrap()
        .to_owned();
    let warning = ActionResult::Failed(format!(
        "entry-failed: broker-rejected ({cause}); recovery: {recovery}; market result: {cause}"
    ))
    .entry_warning();
    assert_eq!(warning.matches(cause).count(), 1, "{warning}");
    assert!(!warning.contains("4."), "{warning}");
}

#[test]
fn unknown_failure_text_is_kept_without_inventing_its_source() {
    logging();
    let warning = ActionResult::Failed("new backend error; diagnostic=42".into()).entry_warning();
    assert!(warning.contains("Final result: ENTRY ERROR"), "{warning}");
    assert!(
        warning.contains("new backend error; diagnostic=42"),
        "{warning}"
    );
    assert!(!warning.contains("LOCAL BLOCK"), "{warning}");
    assert!(!warning.contains("BROKER REJECTION"), "{warning}");
}

#[test]
fn legacy_broker_token_does_not_turn_pre_submission_failures_into_broker_rejections() {
    logging();
    [
        ("MT5 entry validation before submission: invalid volume", "LOCAL BLOCK"),
        ("MT5 entry rejected before transmission: connection lost", "LOCAL ERROR"),
        ("Open API entry not sent: audit write failed", "LOCAL ERROR"),
        ("not submitted: transport unavailable", "LOCAL ERROR"),
        ("market entry not submitted: audit persistence failed: disk full", "LOCAL ERROR"),
        ("cTrader market entries require the audited hybrid backend for post-fill absolute SL/TP normalization", "LOCAL BLOCK"),
    ].into_iter().for_each(|(detail, category)| {
        let error = crate::broker::EntryError::OrderRejectedWithDetail(detail.into());
        let result = ActionResult::Failed(crate::recover_entry::outcome_for_entry_error(&error));
        let warning = result.entry_warning();
        assert!(warning.contains(&format!("Final result: {category}")), "{warning}");
        assert!(warning.contains(detail), "{warning}");
        assert!(!warning.contains("BROKER REJECTION"), "{warning}");
    });
}

#[test]
fn bare_rejection_token_does_not_prove_the_order_reached_the_broker() {
    logging();
    [
        "entry-failed: broker-rejected",
        "entry-failed: broker-rejected (broker rejected the order: invalid cTrader volume or symbol limits)",
    ].into_iter().for_each(|outcome| {
        let warning = ActionResult::Failed(outcome.into()).entry_warning();
        assert!(warning.contains("Final result: ENTRY ERROR"), "{warning}");
        assert!(!warning.contains("BROKER REJECTION"), "{warning}");
    });
}

#[test]
fn mt5_submission_rejection_is_broker_evidence_without_a_final_result() {
    logging();
    let warning = ActionResult::Failed(
        "entry-failed: broker-rejected (broker rejected the order: MT5 request_id=42 submission_code=10017; final result unavailable)".into()
    ).entry_warning();
    assert!(
        warning.contains("Final result: BROKER REJECTION"),
        "{warning}"
    );
    assert!(warning.contains("final result unavailable"), "{warning}");
}
