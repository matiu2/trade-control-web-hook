use super::*;
use serde_json::json;
use tracing_subscriber::prelude::*;
fn init() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
#[test]
fn volume_is_hundredths_and_rounds_down() {
    init();
    assert_eq!(
        entry::volume(1234.567, 100, 1_000_000, 100).unwrap(),
        123400
    );
    assert!(entry::volume(0.5, 100, 1_000_000, 100).is_err());
}
#[test]
fn money_digits_not_hardcoded_cents() {
    init();
    assert_eq!(
        wire::money(&json!({"balance":"10053099944","moneyDigits":8}), "balance").unwrap(),
        100.53099944
    );
}
#[test]
fn market_order_protects_with_relative_distances() {
    init();
    let req = EntryRequest {
        instrument: "EURUSD",
        direction: trade_control_core::intent::Direction::Long,
        entry: trade_control_core::intent::ResolvedEntry::Market {
            reference_price: 1.1,
        },
        stop_loss: 1.099,
        take_profit: 1.102,
        risk: trade_control_core::intent::RiskBudget::Percent(1.0),
        dry_run: false,
        contract_multiplier: None,
    };
    let p = entry::payload(42, 7, 100000, &req, 1.1).unwrap();
    assert!(p.get("stopLoss").is_none());
    assert_eq!(p["relativeStopLoss"], 100);
    assert_eq!(p["relativeTakeProfit"], 200);
}

#[test]
fn volume_invalid_limits_nonfinite_and_oversize_are_refused() {
    init();
    for (units, min, max, step) in [
        (f64::NAN, 100, 1000, 100),
        (10.0, 0, 1000, 100),
        (10.0, 100, 1000, 0),
        (20.0, 100, 1000, 100),
        (f64::INFINITY, 100, 1000, 100),
    ] {
        assert!(entry::volume(units, min, max, step).is_err());
    }
}
fn request(
    direction: trade_control_core::intent::Direction,
    entry: trade_control_core::intent::ResolvedEntry,
) -> EntryRequest<'static> {
    let (stop_loss, take_profit) = if direction == trade_control_core::intent::Direction::Long {
        (1.09, 1.12)
    } else {
        (1.11, 1.08)
    };
    EntryRequest {
        instrument: "EURUSD",
        direction,
        entry,
        stop_loss,
        take_profit,
        risk: trade_control_core::intent::RiskBudget::Units(1000.0),
        dry_run: true,
        contract_multiplier: None,
    }
}
#[test]
fn pending_orders_use_absolute_protection_and_distinct_trigger_fields() {
    init();
    use trade_control_core::intent::{Direction, ResolvedEntry};
    for (entry, kind, key) in [
        (ResolvedEntry::Stop { trigger_price: 1.1 }, 3, "stopPrice"),
        (ResolvedEntry::Limit { trigger_price: 1.1 }, 2, "limitPrice"),
    ] {
        let req = request(Direction::Long, entry);
        let p = entry::payload(42, 7, 100000, &req, 1.1).unwrap();
        assert_eq!(p["orderType"], kind);
        assert_eq!(p[key], 1.1);
        assert_eq!(p["stopLoss"], 1.09);
        assert_eq!(p["takeProfit"], 1.12);
        assert!(p.get("relativeStopLoss").is_none());
    }
}
#[test]
fn short_market_relative_protection_has_positive_directional_distances() {
    init();
    use trade_control_core::intent::{Direction, ResolvedEntry};
    let req = request(
        Direction::Short,
        ResolvedEntry::Market {
            reference_price: 1.1,
        },
    );
    let p = entry::payload(42, 7, 100000, &req, 1.1).unwrap();
    assert_eq!(p["tradeSide"], 2);
    assert_eq!(p["relativeStopLoss"], 1000);
    assert_eq!(p["relativeTakeProfit"], 2000);
}
#[test]
fn invalid_geometry_cannot_reach_order_submission() {
    init();
    use trade_control_core::intent::{Direction, ResolvedEntry};
    let mut req = request(
        Direction::Long,
        ResolvedEntry::Market {
            reference_price: 1.1,
        },
    );
    req.stop_loss = 1.2;
    assert!(entry::payload(42, 7, 100000, &req, 1.1).is_err());
    req.stop_loss = f64::NAN;
    assert!(entry::payload(42, 7, 100000, &req, 1.1).is_err());
}
#[test]
fn absence_or_partially_filled_cancellation_does_not_prove_dead_attempt() {
    init();
    assert!(!history::cancelled_without_fill(&json!({}), 0));
    assert!(!history::cancelled_without_fill(
        &json!({"orderStatus":5}),
        100
    ));
    assert!(history::cancelled_without_fill(
        &json!({"orderStatus":"ORDER_STATUS_CANCELLED"}),
        0
    ));
}
fn opening() -> Value {
    json!({"orderId":7,"positionId":11,"dealId":1,"dealStatus":2,"filledVolume":100000,"executionTimestamp":1000,"executionPrice":1.1})
}
fn closing(volume: i64) -> Value {
    json!({"orderId":9,"positionId":11,"dealId":2,"dealStatus":2,"filledVolume":volume,"executionTimestamp":2000,"executionPrice":1.2,"closePositionDetail":{"entryPrice":1.1,"grossProfit":10000000,"swap":-100,"commission":-500,"balance":1000000000,"closedVolume":volume,"moneyDigits":4,"pnlConversionFee":100}})
}
#[test]
fn partial_closing_deal_does_not_report_position_closed() {
    init();
    assert!(
        history::closed(&[opening(), closing(50000)])
            .unwrap()
            .is_none()
    );
    assert!(history::closed(&[closing(100000)]).unwrap().is_none());
}
#[test]
fn fully_matched_closure_retains_unknown_net_pnl_until_fee_signs_verified() {
    init();
    let closed = history::closed(&[opening(), closing(100000)])
        .unwrap()
        .unwrap();
    assert!(closed.net.is_none());
}
#[test]
fn multiple_origin_orders_are_not_attributed_to_one_plan() {
    init();
    let orders = json!([{"orderId":7,"positionId":11,"closingOrder":false,"orderStatus":2},{"orderId":8,"positionId":11,"closingOrder":false,"orderStatus":2}]);
    assert!(history::origin(orders.as_array().unwrap(), 11).is_err());
}
#[test]
fn unknown_money_precision_stays_unknown() {
    init();
    assert!(wire::money(&json!({"balance":10000}), "balance").is_err());
    assert!(wire::money(&json!({"balance":10000,"moneyDigits":99}), "balance").is_err());
}
#[tokio::test]
async fn dry_run_does_not_invoke_mutation_transport() {
    init();
    let called = std::cell::Cell::new(false);
    let result = entry::submit(true, 100000, 1.1, || {
        called.set(true);
        async { Ok(json!({})) }
    })
    .await
    .unwrap();
    assert_eq!(result.order_id, "dry-run");
    assert!(!called.get());
}
#[tokio::test]
async fn uncertain_submission_is_ambiguous_and_never_retried() {
    init();
    let calls = std::cell::Cell::new(0);
    let result = entry::submit(false, 100000, 1.1, || {
        calls.set(calls.get() + 1);
        async {
            Err(ctrader_api::RequestError::Unknown(
                "connection dropped after send".into(),
            ))
        }
    })
    .await;
    assert!(matches!(result, Err(EntryError::AmbiguousSuccess(_))));
    assert_eq!(calls.get(), 1);
}
#[tokio::test]
async fn definitive_rejection_and_not_sent_are_distinct_from_unknown() {
    init();
    for error in [
        ctrader_api::RequestError::Rejected {
            code: "NO_MONEY".into(),
        },
        ctrader_api::RequestError::NotSent("disconnected before send".into()),
    ] {
        let result = entry::submit(false, 100000, 1.1, || async { Err(error) }).await;
        assert!(matches!(
            result,
            Err(EntryError::OrderRejectedWithDetail(_))
        ));
    }
}
#[test]
fn accepted_response_without_order_id_is_ambiguous() {
    init();
    assert!(matches!(
        entry::response(&json!({"executionType":2}), 100000, 1.1),
        Err(EntryError::AmbiguousSuccess(_))
    ));
}
#[test]
fn successful_entry_reports_requested_units_and_price() {
    init();
    let p = entry::response(
        &json!({"executionType":"ORDER_ACCEPTED","order":{"orderId":"77"}}),
        100000,
        1.1,
    )
    .unwrap();
    assert_eq!(p.order_id, "77");
    assert_eq!(p.size, Some(1000.0));
    assert_eq!(p.price, Some(1.1));
}
#[test]
fn stop_amend_preserves_target_and_other_protection_settings() {
    init();
    let source = json!({"positionId":12,"takeProfit":1.2,"stopLoss":1.0,"trailingStopLoss":true,"guaranteedStopLoss":false,"stopLossTriggerMethod":2});
    let p = management::amend_payload(&source, true, 1.05).unwrap();
    assert_eq!(p["positionId"], 12);
    assert_eq!(p["stopLoss"], 1.05);
    assert_eq!(p["takeProfit"], 1.2);
    assert_eq!(p["trailingStopLoss"], true);
    assert_eq!(p["stopLossTriggerMethod"], 2);
}
#[test]
fn pending_stop_amend_preserves_entry_trigger_and_target() {
    init();
    let source = json!({"orderId":7,"stopPrice":1.1,"takeProfit":1.2});
    let p = management::amend_payload(&source, false, 1.05).unwrap();
    assert_eq!(p["orderId"], 7);
    assert_eq!(p["stopPrice"], 1.1);
    assert_eq!(p["takeProfit"], 1.2);
}
#[test]
fn conflicting_position_and_order_id_namespaces_refuse_amendment() {
    init();
    let snapshot = json!({"position":[{"positionId":7}],"order":[]});
    assert!(
        management::resolve_amend(&snapshot, Some(&json!({"orderId":7,"positionId":19})), 7)
            .is_err()
    );
    let snapshot = json!({"position":[{"positionId":7}],"order":[{"orderId":7}]});
    assert!(management::resolve_amend(&snapshot, None, 7).is_err());
}
#[test]
fn originating_order_resolves_to_exact_linked_position() {
    init();
    let snapshot = json!({"position":[{"positionId":11},{"positionId":12}],"order":[]});
    let (position, row) =
        management::resolve_amend(&snapshot, Some(&json!({"orderId":7,"positionId":12})), 7)
            .unwrap()
            .unwrap();
    assert!(position);
    assert_eq!(row["positionId"], 12);
}
#[test]
fn market_entry_refused_before_network_for_real_and_dry_run() {
    init();
    use trade_control_core::intent::{Direction, ResolvedEntry};
    for dry_run in [true, false] {
        let mut req = request(
            Direction::Long,
            ResolvedEntry::Market {
                reference_price: 1.1,
            },
        );
        req.dry_run = dry_run;
        let error = entry::validate_entry_kind(&req).unwrap_err();
        assert!(error.to_string().contains("post-fill absolute SL/TP"));
    }
    for entry in [
        ResolvedEntry::Stop { trigger_price: 1.1 },
        ResolvedEntry::Limit { trigger_price: 1.1 },
    ] {
        assert!(entry::validate_entry_kind(&request(Direction::Long, entry)).is_ok());
    }
}
#[test]
fn malformed_snapshot_cannot_be_misreported_as_flat() {
    init();
    assert!(wire::validate_snapshot(&json!({"position":"malformed"})).is_err());
    assert!(wire::validate_snapshot(&json!({"position":[{"positionId":11,"tradeData":{"symbolId":7,"volume":100,"tradeSide":1}}]})).is_err());
    assert!(wire::validate_snapshot(&json!({"position":[],"order":[]})).is_ok());
}
