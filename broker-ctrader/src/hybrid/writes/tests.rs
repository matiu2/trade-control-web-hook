use super::*;

#[test]
fn unknown_companion_entry_and_amend_keep_the_account_interlocked() {
    crate::hybrid::tests::init();
    let (evidence, resolved) = entry_receipt(
        "audit-1",
        &Err(EntryError::AmbiguousSuccess("response lost".into())),
    );
    assert!(!resolved);
    assert_eq!(evidence["client_order_id"], "audit-1");
    assert_eq!(evidence["outcome"], "unknown");
    assert!(!amendment_confirmed(&Err(AmendError::Transient)));
    assert!(amendment_confirmed(&Ok(())));
    assert!(amendment_confirmed(&Err(AmendError::NotFound)));
    assert!(entry_receipt("audit-2", &Err(EntryError::UnitsBelowMinimum)).1);
}

#[test]
fn accepted_entry_keeps_exact_broker_order_and_audit_identity() {
    crate::hybrid::tests::init();
    let result = Ok(Placement {
        order_id: "777".into(),
        size: Some(1000.0),
        price: Some(1.1),
    });
    let (evidence, resolved) = entry_receipt("audit-3", &result);
    assert!(resolved);
    assert_eq!(evidence["order_id"], "777");
    assert_eq!(evidence["size"], 1000.0);
}

#[test]
fn companion_client_id_reaches_broker_payload_without_changing_protection() {
    crate::hybrid::tests::init();
    let payload = json!({"stopLoss":1.09,"takeProfit":1.12,"volume":100000});
    let bound = entry::with_client_id(payload.clone(), Some("manage-123")).unwrap();
    assert_eq!(bound["clientOrderId"], "manage-123");
    assert_eq!(bound["stopLoss"], payload["stopLoss"]);
    assert_eq!(bound["takeProfit"], payload["takeProfit"]);
    assert_eq!(
        entry::with_client_id(payload.clone(), None).unwrap(),
        payload
    );
    for invalid in ["".to_owned(), "x".repeat(51), "bad\nvalue".to_owned()] {
        assert!(entry::with_client_id(payload.clone(), Some(&invalid)).is_err());
    }
}
