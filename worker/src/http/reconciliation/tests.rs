use super::*;
use serde_json::json;
use trade_control_core::{
    account::AccountCaps,
    broker_exit::{BrokerTradeExit, ExitReason},
    incoming::{parse_and_verify, signed_pairs_from_text},
    intent::Direction,
    sig,
};

fn fixture() -> (Verified, DispatchConfig, MarketReconciliationProof) {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let now = "2026-10-26T13:40:25Z".parse().unwrap();
    let unsigned = "close: 1.9702\nhigh: 1.9703\nlow: 1.9701\ntime: \"2026-10-26T13:40:25Z\"\nv: 1\naction: enter\nid: pos-example-enter\ntrade_id: pos-example\ninstrument: EURNZD\nbroker: ctrader\naccount: ctrader-demo\ndirection: long\nentry: {type: market}\nstop_loss: {absolute: 1.9686}\ntake_profit: {absolute: 1.9739}\nrisk_pct: 1.0\npip_size: 0.0001\ntick_size: 0.00001\nnot_after: \"2026-10-26T14:00:00Z\"\n";
    let key = [17; 32];
    let signature = sig::sign(&key, &signed_pairs_from_text(unsigned).unwrap()).unwrap();
    let verified =
        parse_and_verify(&format!("{unsigned}sig: \"{signature}\"\n"), &key, now).unwrap();
    let cfg = DispatchConfig {
        worker_max_risk_pct: 1.0,
        worker_max_open_positions: 3,
        pip_size: 0.0001,
        tick_size: Some(0.00001),
        caps: AccountCaps::default(),
    };
    let proof = MarketReconciliationProof {
        account: "ctrader-demo".into(),
        account_id: 42,
        audit_id: "manage-example".into(),
        instrument: "EURNZD".into(),
        direction: Direction::Long,
        pip_size: 0.0001,
        order_id: "325083610".into(),
        position_id: "299677164".into(),
        units: 7_410_000.0,
        entry_price: 1.9704,
        stop_loss: 1.9687,
        take_profit: 1.9740,
        opened_at: now,
        requested_stop_loss: 1.9686,
        requested_take_profit: 1.9739,
        audit_intent: json!({"instrument":"EURNZD","direction":"long","entry_kind":"market","entry_price":1.9702,"stop_loss":1.9686,"take_profit":1.9739,"risk_kind":"percent","risk_value":1.0,"max_risk_pct":1.0,"max_positions":3}),
        evidence: json!({}),
        closed_exit: None,
    };
    (verified, cfg, proof)
}

#[test]
fn audit_binding_requires_one_pending_exact_opaque_token() {
    fixture();
    let ids = vec!["manage-example".into()];
    assert_eq!(
        candidate(&ids, "unknown (manage-example may have executed)"),
        Some("manage-example")
    );
    for outcome in [
        "manage-example-other",
        "xmanage-example",
        "manage-example.foo",
        "manage-example:other",
        "different",
    ] {
        assert_eq!(candidate(&ids, outcome), None, "{outcome}");
    }
    assert_eq!(candidate(&[], "manage-example"), None);
    assert_eq!(
        candidate(&["manage-example".into(), "other".into()], "manage-example"),
        None
    );
    assert_eq!(candidate(&[String::new()], "anything"), None);
}

#[test]
fn current_request_must_match_broker_identity_risk_and_original_geometry() {
    let (verified, cfg, proof) = fixture();
    assert!(matches_request(&verified, &cfg, Some(42), &proof).is_ok());
    assert!(matches_request(&verified, &cfg, Some(43), &proof).is_err());
    assert!(matches_request(&verified, &cfg, None, &proof).is_err());
    for field in [
        "account",
        "instrument",
        "direction",
        "risk_value",
        "stop_loss",
        "take_profit",
        "entry_price",
        "max_risk_pct",
        "max_positions",
    ] {
        let mut other = proof.clone();
        match field {
            "account" => other.account = "another-account".into(),
            "instrument" => other.instrument = "EURGBP".into(),
            "direction" => other.direction = Direction::Short,
            _ => other.audit_intent[field] = json!(99.0),
        }
        assert!(
            matches_request(&verified, &cfg, Some(42), &other).is_err(),
            "{field}"
        );
    }
    let mut other = proof;
    other.requested_stop_loss = 1.96;
    assert!(matches_request(&verified, &cfg, Some(42), &other).is_err());
}

#[test]
fn known_fill_reports_real_protection_without_marking_success_or_management() {
    let (_, _, proof) = fixture();
    let (status, body) = filled_response(&proof);
    assert_eq!(status, StatusCode::CONFLICT);
    for expected in [
        "filled, needs reconciliation",
        "order=325083610",
        "position=299677164",
        "units=7410000",
        "fill=1.9704",
        "SL=1.9687",
        "TP=1.974",
        "requested SL=1.9686",
        "requested TP=1.9739",
        "do not retry",
        "management is not attached",
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    assert!(!body.contains("response unreadable"));
    assert!(!body.contains("action failed"));
}

#[test]
fn closed_fill_reports_broker_proven_exit() {
    let (_, _, mut proof) = fixture();
    proof.closed_exit = Some(BrokerTradeExit {
        broker_trade_id: proof.position_id.clone(),
        exit_order_id: Some("exit-1".into()),
        transaction_id: None,
        closed_at: Some(proof.opened_at),
        reason: ExitReason::StopLoss,
        broker_reason: None,
        entry_price: Some(proof.entry_price),
        exit_price: Some(1.9687),
        expected_price: Some(1.9687),
        realized_pl: None,
        account_currency: None,
        units: Some(proof.units),
        quote_currency: None,
    });
    let (status, body) = filled_response(&proof);
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("broker confirms position closed"), "{body}");
    assert!(body.contains("exit=1.9687"), "{body}");
    assert!(!body.contains("management is not attached"), "{body}");
}

#[test]
fn fallback_does_not_claim_the_response_was_unreadable_or_echo_unsafe_outcome() {
    fixture();
    let outcome = ActionResult::AmbiguousSuccess("secret unsafe text: failed".into());
    let (status, body) = super::super::action_to_parts(&outcome);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body.contains("unconfirmed execution/protection state"),
        "{body}"
    );
    assert!(body.contains("do not retry"), "{body}");
    assert!(!body.contains("secret unsafe text"), "{body}");
    assert!(!body.contains("response unreadable"), "{body}");
}
