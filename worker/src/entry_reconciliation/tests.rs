use super::*;
use serde_json::json;
use trade_control_core::{incoming::signed_pairs_from_text, recording::RequestRecord, sig};

const KEY: [u8; 32] = [17; 32];

fn clone_record(record: &RequestRecord) -> RequestRecord {
    serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap()
}

fn rewrite_signed(record: &mut RequestRecord, rewrite: impl FnOnce(&str) -> String) {
    let unsigned = rewrite(record.body.rsplit_once("\nsig:").unwrap().0);
    let signature = sig::sign(&KEY, &signed_pairs_from_text(&unsigned).unwrap()).unwrap();
    record.body = format!("{unsigned}\nsig: \"{signature}\"\n");
}

fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
}

pub(super) fn fixture() -> (RequestRecord, FillProof, DateTime<Utc>) {
    init();
    let unsigned = "close: 1.9702\nhigh: 1.9703\nlow: 1.9701\ntime: \"2026-10-26T13:40:25Z\"\nv: 1\naction: enter\nid: pos-eurnzd-example-enter\ntrade_id: pos-eurnzd-example\ninstrument: EURNZD\nbroker: ctrader\naccount: ctrader-demo\ndirection: long\nentry: {type: market}\nstop_loss: {absolute: 1.9686}\ntake_profit: {absolute: 1.9739}\nrisk_pct: 1.0\npip_size: 0.0001\nnot_after: \"2026-10-26T14:00:00Z\"\n";
    let signature = sig::sign(&KEY, &signed_pairs_from_text(unsigned).unwrap()).unwrap();
    let record = RequestRecord {
        ts: "2026-10-26T13:40:25Z".into(),
        request_id: "request-1".into(),
        method: "POST".into(),
        path: "/".into(),
        headers: vec![],
        body: format!("{unsigned}sig: \"{signature}\"\n"),
        intent_id: Some("pos-eurnzd-example-enter".into()),
        trade_id: Some("pos-eurnzd-example".into()),
        status: 502,
        outcome: "ambiguous: audit_id=manage-example".into(),
        logs: vec![],
    };
    let proof = FillProof {
        account: "ctrader-demo".into(),
        account_id: 42,
        audit_id: "manage-example".into(),
        instrument: "EURNZD".into(),
        order_id: "order-1".into(),
        position_id: "position-1".into(),
        direction: Direction::Long,
        units: 1000.0,
        entry_price: 1.9704,
        stop_loss: 1.9687,
        take_profit: 1.9740,
        pip_size: 0.0001,
        opened_at: "2026-10-26T13:40:33Z".parse().unwrap(),
        requested_stop_loss: 1.9686,
        requested_take_profit: 1.9739,
        audit_intent: json!({"instrument":"EURNZD","direction":"long","entry_kind":"market","entry_price":1.9702,"stop_loss":1.9686,"take_profit":1.9739,"risk_kind":"percent","risk_value":1.0}),
        evidence: json!({"source":"native broker test"}),
        closed_exit: None,
    };
    (record, proof, "2026-10-27T15:00:00Z".parse().unwrap())
}

#[test]
fn verifies_historical_request_and_preserves_accepted_protection_without_inventing_policy() {
    let (record, proof, now) = fixture();
    let adoption = recover(&[record], &KEY, "pos-eurnzd-example", proof, now).unwrap();
    assert_eq!(adoption.attempt.stop_loss_price, Some(1.9687));
    assert_eq!(
        adoption.attempt.broker_trade_id.as_deref(),
        Some("position-1")
    );
    assert!(adoption.attempt.breakeven.is_none());
    assert!(adoption.attempt.order_control.is_none());
    assert!(adoption.attempt.expires_at > now);
}

#[test]
fn refuses_bad_signature_wrong_audit_and_duplicate_different_request() {
    let (record, proof, now) = fixture();
    assert!(
        recover(
            &[clone_record(&record)],
            &[0; 32],
            "pos-eurnzd-example",
            proof.clone(),
            now
        )
        .is_err()
    );
    let mut wrong = clone_record(&record);
    wrong.outcome.push_str("-other");
    assert!(recover(&[wrong], &KEY, "pos-eurnzd-example", proof.clone(), now).is_err());
    let mut different = clone_record(&record);
    different.body.push_str("# different\n");
    assert!(recover(&[record, different], &KEY, "pos-eurnzd-example", proof, now).is_err());
}

#[test]
fn refuses_cross_account_and_changed_audit_risk_or_protection() {
    let (record, proof, now) = fixture();
    let mut other = proof.clone();
    other.account = "other-account".into();
    assert!(
        recover(
            &[clone_record(&record)],
            &KEY,
            "pos-eurnzd-example",
            other,
            now
        )
        .is_err()
    );
    let mut other = proof.clone();
    other.audit_intent["risk_value"] = json!(2.0);
    assert!(
        recover(
            &[clone_record(&record)],
            &KEY,
            "pos-eurnzd-example",
            other,
            now
        )
        .is_err()
    );
    let mut other = proof;
    other.requested_stop_loss = 1.96;
    assert!(recover(&[record], &KEY, "pos-eurnzd-example", other, now).is_err());
}

#[test]
fn rejects_invalid_native_metadata() {
    let (record, mut proof, now) = fixture();
    proof.pip_size = 0.01;
    assert!(recover(&[record], &KEY, "pos-eurnzd-example", proof, now).is_err());
}

#[test]
fn audit_binding_rejects_dot_or_colon_suffix_and_prefix_collisions() {
    let (record, proof, now) = fixture();
    for outcome in [
        "audit_id=manage-example.extra",
        "audit_id=manage-example:extra",
        "audit_id=extra.manage-example",
        "audit_id=extra:manage-example",
    ] {
        let mut other = clone_record(&record);
        other.outcome = outcome.into();
        assert!(
            recover(&[other], &KEY, "pos-eurnzd-example", proof.clone(), now).is_err(),
            "accepted a partial audit ID: {outcome}"
        );
    }
    let mut exact = clone_record(&record);
    exact.outcome = "ambiguous (audit_id=\"manage-example\")".into();
    assert!(recover(&[exact], &KEY, "pos-eurnzd-example", proof, now).is_ok());
}

#[test]
fn signed_tick_rounds_market_reference_stop_and_target_like_native_dispatch() {
    let (mut record, mut proof, now) = fixture();
    rewrite_signed(&mut record, |body| {
        format!(
            "{}\ntick_size: 0.00001",
            body.replace("close: 1.9702", "close: 1.970234")
                .replace("absolute: 1.9686", "absolute: 1.968654")
                .replace("absolute: 1.9739", "absolute: 1.973976")
        )
    });
    proof.requested_stop_loss = 1.96865;
    proof.requested_take_profit = 1.97397;
    proof.audit_intent["entry_price"] = json!(1.97023);
    proof.audit_intent["stop_loss"] = json!(1.96865);
    proof.audit_intent["take_profit"] = json!(1.97397);
    assert!(
        recover(
            &[clone_record(&record)],
            &KEY,
            "pos-eurnzd-example",
            proof.clone(),
            now
        )
        .is_ok()
    );
    // The audit must match the snapped request, never the unrounded drawing.
    proof.audit_intent["stop_loss"] = json!(1.968654);
    assert!(recover(&[record], &KEY, "pos-eurnzd-example", proof, now).is_err());
}

#[test]
fn missing_signed_tick_uses_the_native_pip_fallback() {
    let (mut record, proof, now) = fixture();
    rewrite_signed(&mut record, |body| {
        body.replace("close: 1.9702", "close: 1.970234")
            .replace("absolute: 1.9686", "absolute: 1.968654")
            .replace("absolute: 1.9739", "absolute: 1.973976")
    });
    assert!(recover(&[record], &KEY, "pos-eurnzd-example", proof, now).is_ok());
}

#[test]
fn closed_proof_remains_a_closed_position_and_cannot_refer_to_another_trade() {
    use trade_control_core::broker_exit::{BrokerTradeExit, ExitReason};
    let (record, mut proof, now) = fixture();
    proof.closed_exit = Some(BrokerTradeExit {
        broker_trade_id: proof.position_id.clone(),
        exit_order_id: Some("exit-1".into()),
        transaction_id: Some("deal-2".into()),
        closed_at: Some(now),
        reason: ExitReason::Other,
        broker_reason: Some("manual close".into()),
        entry_price: Some(proof.entry_price),
        exit_price: Some(1.9705),
        expected_price: None,
        realized_pl: Some(0.1),
        account_currency: Some("USD".into()),
        units: Some(proof.units),
        quote_currency: Some("NZD".into()),
    });
    let adoption = recover(
        &[clone_record(&record)],
        &KEY,
        "pos-eurnzd-example",
        proof.clone(),
        now,
    )
    .unwrap();
    assert!(adoption.proof.closed_exit.is_some());
    proof.closed_exit.as_mut().unwrap().broker_trade_id = "another-position".into();
    assert!(recover(&[record], &KEY, "pos-eurnzd-example", proof, now).is_err());
}
