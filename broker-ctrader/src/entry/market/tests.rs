use super::*;
mod script;
use script::*;

#[tokio::test]
async fn accepted_is_not_a_fill_and_slippage_normalizes_both_levels_once() {
    let mut io = successful();
    let p = execute(
        &mut io,
        terms(),
        json!({"relativeStopLoss":100,"relativeTakeProfit":200}),
    )
    .await
    .unwrap();
    assert_eq!(p.order_id, "9");
    assert_eq!(p.size, Some(1000.0));
    assert_eq!(p.price, Some(1.1));
    assert_eq!(
        io.mutations
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>(),
        vec![2106, 2110]
    );
    assert_eq!(
        io.mutations[1].1,
        json!({"positionId":11,"stopLoss":1.099,"takeProfit":1.102})
    );
    assert!(io.notes.iter().any(|p| p == "market-filled-position"));
    assert_eq!(
        io.notes.last().map(String::as_str),
        Some("market-protection-verified")
    );
}
#[tokio::test]
async fn zero_fill_terminal_cancel_is_definitive_but_partial_execution_is_not_rejected() {
    let mut io = script(vec![detail(5, 0)]);
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::OrderRejectedWithDetail(_))
    ));
    assert_eq!(io.mutations.len(), 1);
    let mut io = script(vec![
        detail(5, 50000),
        snapshot(50000, false),
        snapshot(50000, true),
    ]);
    // The simulated amend response has the wrong volume; a positive partial
    // execution must remain unknown, never look like a safe failed entry.
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
    assert_eq!(
        io.mutations
            .iter()
            .filter(|(kind, _)| *kind == 2106)
            .count(),
        1
    );
}
#[tokio::test]
async fn wrong_order_account_symbol_side_or_client_identity_never_amends() {
    for (path, value) in [
        (vec!["ctidTraderAccountId"], json!(99)),
        (vec!["order", "orderId"], json!(99)),
        (vec!["order", "clientOrderId"], json!("someone-else")),
        (vec!["order", "tradeData", "symbolId"], json!(99)),
        (vec!["order", "tradeData", "tradeSide"], json!(2)),
    ] {
        let mut d = detail(2, 100000);
        let mut field = &mut d;
        for key in path {
            field = &mut field[key];
        }
        *field = value;
        let mut io = script(vec![d]);
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
}
#[tokio::test]
async fn old_position_missing_initial_protection_and_volume_mismatch_are_unknown() {
    for mode in 0..3 {
        let mut t = terms();
        let mut s = snapshot(100000, false);
        match mode {
            0 => {
                t.before.insert(11);
            }
            1 => {
                s["position"][0].as_object_mut().unwrap().remove("stopLoss");
            }
            _ => {
                s["position"][0]["tradeData"]["volume"] = json!(50000);
            }
        }
        let mut io = script(vec![detail(2, 100000), s]);
        assert!(matches!(
            execute(&mut io, t, json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
}
#[tokio::test]
async fn amendment_rejection_disconnect_and_unchanged_verification_are_unknown_without_resubmit() {
    for mode in 0..4 {
        let mut io = successful();
        match mode {
            0 => io.fail_amend = true,
            1 => io.disconnect = true,
            2 => {
                io.replies.pop_back();
                io.replies.push_back(snapshot(100000, false));
            }
            _ => io.amended_volume = 50000,
        }
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(
            io.mutations
                .iter()
                .filter(|(kind, _)| *kind == 2106)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn partial_or_filled_acknowledgement_cannot_become_a_definitive_zero_fill_failure() {
    for kind in [3, 11] {
        let mut io = script(vec![detail(5, 0)]);
        io.initial["executionType"] = json!(kind);
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
    let mut io = script(vec![detail(5, 0)]);
    io.initial["executionType"] = json!(7);
    io.initial["deal"] = json!({"filledVolume":50000});
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
}

#[tokio::test]
async fn filled_status_requires_full_volume_and_initial_protection_on_correct_sides() {
    let mut io = script(vec![detail(2, 50000)]);
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
    for key in ["stopLoss", "takeProfit"] {
        let mut s = snapshot(100000, false);
        s["position"][0][key] = json!(if key == "stopLoss" { 1.1003 } else { 1.1001 });
        let mut io = script(vec![detail(2, 100000), s]);
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
}

#[tokio::test]
async fn verification_timeout_and_audit_failure_never_resubmit() {
    for failure in ["timeout", "audit"] {
        let mut io = successful();
        io.stall = failure == "timeout";
        io.audit_failure = (failure == "audit").then(|| "market-filled-position".into());
        let budget = if failure == "timeout" {
            Duration::from_millis(5)
        } else {
            Duration::from_secs(1)
        };
        assert!(matches!(
            execute_with_timeout(&mut io, terms(), json!({}), budget).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(
            io.mutations
                .iter()
                .filter(|(kind, _)| *kind == 2106)
                .count(),
            1
        );
        if failure == "audit" {
            assert!(
                io.notes
                    .iter()
                    .any(|phase| phase == "market-filled-position")
            );
        }
    }
}

#[tokio::test]
async fn audit_failure_before_send_creates_no_broker_order() {
    let mut io = successful();
    io.audit_failure = Some("market-before-send".into());
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::OrderRejectedWithDetail(_))
    ));
    assert!(io.mutations.is_empty());
}

#[tokio::test]
async fn short_fill_normalizes_both_absolute_levels_after_slippage() {
    let (t, mut io) = short_case();
    let p = execute(&mut io, t, json!({})).await.unwrap();
    assert_eq!(p.size, Some(1000.0));
    assert_eq!(
        io.mutations[1].1,
        json!({"positionId":11,"stopLoss":1.101,"takeProfit":1.098})
    );
}

#[test]
fn market_protection_rejects_off_grid_prices_before_submission() {
    init();
    let mut req = EntryRequest {
        instrument: "EURUSD",
        direction: Direction::Long,
        entry: ResolvedEntry::Market {
            reference_price: 1.1,
        },
        stop_loss: 1.099,
        take_profit: 1.102,
        risk: RiskBudget::Units(1000.0),
        dry_run: false,
        contract_multiplier: None,
    };
    grid(&req, 5).unwrap();
    req.stop_loss = 1.099001;
    assert!(grid(&req, 5).is_err());
}

#[tokio::test]
async fn rejected_initial_response_is_safe_only_without_execution_and_with_matching_identity() {
    let mut io = script(vec![]);
    io.initial = json!({"ctidTraderAccountId":7,"executionType":7,"errorCode":"NOT_ENOUGH_MONEY"});
    let error = execute(&mut io, terms(), json!({})).await.unwrap_err();
    assert!(
        matches!(error, EntryError::OrderRejectedWithDetail(ref reason) if reason == "NOT_ENOUGH_MONEY")
    );
    assert_eq!(io.mutations.len(), 1);
    let mut io = script(vec![]);
    io.initial = json!({"ctidTraderAccountId":7,"executionType":7,"errorCode":"REJECTED","order":{"clientOrderId":"another"}});
    assert!(matches!(
        execute(&mut io, terms(), json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
}

#[tokio::test]
async fn closing_orders_or_contradictory_deals_cannot_confirm_a_new_entry() {
    for mode in 0..4 {
        let mut d = detail(2, 100000);
        match mode {
            0 => d["order"]["closingOrder"] = json!(true),
            1 => d["deal"][0]["symbolId"] = json!(99),
            2 => d["deal"][0]["tradeSide"] = json!(2),
            _ => d["deal"][0]["closePositionDetail"] = json!({"closedVolume":100000}),
        }
        let mut io = script(vec![d]);
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
}

#[tokio::test]
async fn adverse_fill_cannot_widen_absolute_stop_beyond_authorized_risk() {
    let mut t = terms();
    t.max_stop_distance = 0.001;
    let mut io = successful();
    assert!(matches!(
        execute(&mut io, t, json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
    assert_eq!(
        io.mutations.len(),
        1,
        "over-risk fill must retain its initial protection without amendment"
    );
}

#[tokio::test]
async fn nonterminal_positive_execution_cannot_later_become_a_definitive_zero_fill_failure() {
    for with_deals in [false, true] {
        let mut partial = detail(1, 50000);
        if !with_deals {
            partial["deal"] = json!([]);
        }
        let mut io = script(vec![partial, detail(5, 0)]);
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(io.mutations.len(), 1);
    }
}

#[tokio::test]
async fn short_adverse_fill_retains_relative_protection_when_absolute_stop_exceeds_risk() {
    let (mut t, mut io) = short_case();
    t.max_stop_distance = 0.001;
    assert!(matches!(
        execute(&mut io, t, json!({})).await,
        Err(EntryError::AmbiguousSuccess(_))
    ));
    assert_eq!(io.mutations.len(), 1);
    assert!(
        io.notes
            .iter()
            .any(|phase| phase == "market-filled-position")
    );
}

#[tokio::test]
async fn amended_response_and_independent_readback_must_still_respect_risk_budget() {
    for response_changed in [true, false] {
        let mut io = successful();
        if response_changed {
            let mut p = position(100000, true);
            p["price"] = json!(1.1006);
            io.amended_override = Some(p);
        } else {
            io.replies.back_mut().unwrap()["position"][0]["price"] = json!(1.1006);
        }
        assert!(matches!(
            execute(&mut io, terms(), json!({})).await,
            Err(EntryError::AmbiguousSuccess(_))
        ));
        assert_eq!(
            io.mutations
                .iter()
                .filter(|(kind, _)| *kind == 2106)
                .count(),
            1
        );
    }
}
