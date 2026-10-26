use super::*;
use serde_json::json;
use tracing_subscriber::prelude::*;

fn records() -> Vec<Value> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    vec![
        json!({"at":"2026-10-26T13:40:25Z","client_order_id":"audit-1","phase":"intent","evidence":{"request":{
        "client_order_id":"audit-1","operation":"open-api-entry","entry_kind":"market","instrument":"EURNZD","direction":"long","stop_loss":1.9686,"take_profit":1.9739}}}),
        json!({"client_order_id":"audit-1","phase":"market-before-send","evidence":{"absolute_stop":1.9686,"absolute_target":1.9739,
            "authorized_max_stop_distance":0.00170012,"requested_reference_price":1.9703,"pre_position_ids":[5],
            "request":{"ctidTraderAccountId":7,"orderType":1,"clientOrderId":"audit-1","symbolId":66,"tradeSide":1,"volume":741000000}}}),
        json!({"client_order_id":"audit-1","phase":"market-response","evidence":{"order":{"orderId":9}}}),
    ]
}

#[test]
fn exact_audit_request_is_required_and_response_can_be_missing_for_history_recovery() {
    let mut rows = records();
    let ctx = evidence::context(&rows, 7, "audit-1").unwrap();
    assert_eq!(ctx.instrument, "EURNZD");
    assert_eq!(ctx.terms.volume, 741000000);
    assert_eq!(ctx.order_id, Some(9));
    assert!(evidence::context(&rows, 8, "audit-1").is_err());
    assert!(evidence::context(&rows, 7, "other").is_err());
    rows.pop();
    assert_eq!(
        evidence::context(&rows, 7, "audit-1").unwrap().order_id,
        None
    );
    rows[1]["evidence"]["absolute_stop"] = json!(1.96);
    assert!(evidence::context(&rows, 7, "audit-1").is_err());
}

fn fill() -> MarketReconciliationProof {
    MarketReconciliationProof {
        account: "ctrader-demo".into(),
        account_id: 7,
        audit_id: "audit-1".into(),
        instrument: "EURNZD".into(),
        direction: Direction::Long,
        pip_size: 0.0001,
        order_id: "9".into(),
        position_id: "10".into(),
        units: 7410000.0,
        entry_price: 1.9704,
        stop_loss: 1.9687,
        take_profit: 1.9740,
        opened_at: DateTime::parse_from_rfc3339("2026-10-26T13:40:33Z")
            .unwrap()
            .with_timezone(&Utc),
        requested_stop_loss: 1.9686,
        requested_take_profit: 1.9739,
        audit_intent: Value::Null,
        evidence: Value::Null,
        closed_exit: None,
    }
}

#[test]
fn resolution_requires_committed_receipt_for_the_exact_broker_fill() {
    records();
    let proof = fill();
    let receipt = json!({"version":1,"operation":"keep-protection-attach","committed":true,
        "trade_id":"trade-1","account":"ctrader-demo","account_id":7,"audit_id":"audit-1",
        "order_id":"9","position_id":"10","stop_loss":1.9687,"take_profit":1.9740,
        "entry_price":1.9704,"units":7410000.0});
    evidence::receipt(&proof, &receipt).unwrap();
    for (field, wrong) in [
        ("committed", json!(false)),
        ("account_id", json!(8)),
        ("position_id", json!("11")),
        ("stop_loss", json!(1.9686)),
        ("units", json!(1.0)),
        ("operation", json!("preview")),
    ] {
        let mut changed = receipt.clone();
        changed[field] = wrong;
        assert!(
            evidence::receipt(&proof, &changed).is_err(),
            "accepted changed {field}"
        );
    }
}

#[test]
fn recheck_refuses_changed_identity_volume_and_protection() {
    records();
    let old = fill();
    evidence::unchanged(&old, &old).unwrap();
    let mut changed = old.clone();
    changed.stop_loss = old.requested_stop_loss;
    assert!(evidence::unchanged(&old, &changed).is_err());
    changed = old.clone();
    changed.position_id = "11".into();
    assert!(evidence::unchanged(&old, &changed).is_err());
    changed = old.clone();
    changed.units = 1000.0;
    assert!(evidence::unchanged(&old, &changed).is_err());
}
