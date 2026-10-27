use super::*;
use std::collections::BTreeMap;
use tracing_subscriber::prelude::*;
fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
fn profile() -> PropAccount {
    PropAccount {
        enabled: true,
        account_id: 123,
        environment: ctrader_api::Environment::Demo,
        state_directory: "/tmp/not-a-live-account".into(),
        history_from: "2026-01-01T22:00:00Z".parse().unwrap(),
        initial_balance: Decimal::from(10000),
        rulebook_verified: true,
        fees_verified: true,
        fee_mode: FeeMode::PerDeal,
        commission_is_positive_cost: true,
        risk: RiskPolicy::default(),
        costs: BTreeMap::new(),
        firm_evidence_file: None,
    }
}
#[test]
fn setup_history_includes_partial_closes_and_all_net_charges() -> Result<()> {
    init();
    let p = profile();
    let time = p.history_from.timestamp_millis() + 1;
    let orders = vec![json!({"orderId":1,"clientOrderId":"r1","tradeData":{"label":"risk:r1"}})];
    let deals = vec![
        json!({"dealId":1,"orderId":1,"positionId":11,"dealStatus":2,"filledVolume":200,"executionTimestamp":time,"commission":10,"moneyDigits":2}),
        json!({"dealId":2,"orderId":2,"positionId":11,"dealStatus":2,"filledVolume":100,"executionTimestamp":time+1,"commission":5,"moneyDigits":2,"closePositionDetail":{"closedVolume":100,"balance":1000073,"grossProfit":100,"swap":-10,"commission":10,"pnlConversionFee":2,"moneyDigits":2}}),
        json!({"dealId":3,"orderId":3,"positionId":11,"dealStatus":2,"filledVolume":100,"executionTimestamp":time+2,"commission":5,"moneyDigits":2,"closePositionDetail":{"closedVolume":100,"balance":999856,"grossProfit":-200,"swap":-10,"commission":10,"pnlConversionFee":2,"moneyDigits":2}}),
    ];
    let ledger = history::normalize(&p, &deals, orders, p.history_from)?;
    assert_eq!(ledger.events.len(), 3);
    assert_eq!(ledger.position_requests[&11], "r1");
    let net = ledger.events.iter().map(|e| e.net_pnl).sum::<Decimal>();
    assert_eq!(net, "-1.44".parse::<Decimal>()?);
    assert_eq!(
        ledger
            .events
            .iter()
            .map(|e| e.volume_delta)
            .sum::<Decimal>(),
        Decimal::ZERO
    );
    Ok(())
}
#[test]
fn unknown_attribution_missing_stops_and_unverified_rules_fail_closed() -> Result<()> {
    init();
    let mut p = profile();
    p.rulebook_verified = false;
    assert!(p.validate().is_err());
    p.rulebook_verified = true;
    p.fees_verified = false;
    assert!(p.validate().is_err());
    p.fees_verified = true;
    let deal = json!({"dealId":1,"orderId":1,"positionId":11,"dealStatus":2,"filledVolume":100,"executionTimestamp":p.history_from.timestamp_millis(),"commission":0,"moneyDigits":2});
    assert!(history::normalize(&p, &[deal], vec![], p.history_from).is_err());
    assert!(stop_tightens(&json!({"tradeData":{"tradeSide":1}}), 1.0).is_err());
    Ok(())
}
#[test]
fn protected_stop_amendments_only_tighten_both_directions() -> Result<()> {
    init();
    let buy = json!({"stopLoss":1.0,"tradeData":{"tradeSide":1}});
    let sell = json!({"stopLoss":2.0,"tradeData":{"tradeSide":2}});
    assert!(stop_tightens(&buy, 0.9).is_err());
    assert!(stop_tightens(&sell, 2.1).is_err());
    stop_tightens(&buy, 1.1)?;
    stop_tightens(&sell, 1.9)?;
    Ok(())
}

#[test]
fn aliases_resolve_physical_account_and_activation_is_sticky() -> Result<()> {
    init();
    let p = profile();
    #[derive(serde::Serialize)]
    struct Document {
        accounts: BTreeMap<String, PropAccount>,
    }
    let document = toml::to_string(&Document {
        accounts: BTreeMap::from([("canonical".into(), p)]),
    })?;
    let selected = config::parse(&document, "alias", 123, ctrader_api::Environment::Demo)?
        .ok_or_else(|| eyre!("missing alias policy"))?;
    assert_eq!(selected.account_id, 123);
    assert!(config::parse(&document, "canonical", 999, ctrader_api::Environment::Demo).is_err());
    let directory = tempfile::tempdir()?;
    binding::check_at(
        directory.path(),
        Some(&selected),
        123,
        ctrader_api::Environment::Demo,
    )?;
    assert!(
        binding::check_at(directory.path(), None, 123, ctrader_api::Environment::Demo).is_err()
    );
    binding::check_at(directory.path(), None, 999, ctrader_api::Environment::Demo)?;
    let mut changed = selected;
    changed.state_directory = "/tmp/other-risk-directory".into();
    assert!(
        binding::check_at(
            directory.path(),
            Some(&changed),
            123,
            ctrader_api::Environment::Demo
        )
        .is_err()
    );
    Ok(())
}
fn opening(id: i64, order: i64, position: i64, time: i64) -> Value {
    json!({"dealId":id,"orderId":order,"positionId":position,"dealStatus":2,"filledVolume":100,"executionTimestamp":time,"commission":0,"moneyDigits":2})
}
fn closing(id: i64, position: i64, gross: i64, balance: i64, time: i64) -> Value {
    json!({"dealId":id,"orderId":id,"positionId":position,"dealStatus":2,"filledVolume":100,"executionTimestamp":time,"commission":0,"moneyDigits":2,"closePositionDetail":{"closedVolume":100,"grossProfit":gross,"swap":0,"commission":0,"balance":balance,"moneyDigits":2}})
}
fn setup_orders() -> Vec<Value> {
    vec![
        json!({"orderId":1,"clientOrderId":"r1","tradeData":{"label":"risk:shared"}}),
        json!({"orderId":2,"clientOrderId":"r2","tradeData":{"label":"risk:shared"}}),
    ]
}
#[test]
fn numeric_deal_order_and_shared_setup_reconstruct_true_balance_peak() -> Result<()> {
    init();
    let p = profile();
    let time = p.history_from.timestamp_millis() + 1;
    let deals = vec![
        opening(97, 1, 11, time),
        opening(98, 2, 12, time),
        closing(99, 11, 10000, 1010000, time),
        closing(100, 12, -10000, 1000000, time),
    ];
    let ledger = history::normalize(&p, &deals, setup_orders(), p.history_from)?;
    assert_eq!(ledger.high, Decimal::from(10100));
    assert!(ledger.events.iter().all(|e| e.setup_id == "shared"));
    assert_eq!(ledger.request_setups["r1"], ledger.request_setups["r2"]);
    Ok(())
}
#[test]
fn recovered_balance_does_not_erase_historical_stop_breaches() -> Result<()> {
    init();
    let p = profile();
    let time = p.history_from.timestamp_millis() + 1;
    let deals = vec![
        opening(1, 1, 11, time),
        closing(2, 11, -50000, 950000, time + 1),
        opening(3, 2, 12, time + 2),
        closing(4, 12, 50000, 1000000, time + 3),
    ];
    let ledger = history::normalize(&p, &deals, setup_orders(), p.history_from)?;
    assert!(ledger.account_breached);
    assert!(
        ledger
            .daily_breached
            .contains(&p.risk.calendar.day_at(p.history_from)?.start)
    );
    Ok(())
}
#[test]
fn equity_and_reserved_risk_use_the_same_broker_pnl_sample() -> Result<()> {
    init();
    let entry = Decimal::from(100);
    let stop = Decimal::from(99);
    let volume = Decimal::from(100);
    let first = exposure::mark_from_pnl(entry, Decimal::ZERO, volume, Decimal::ONE, Side::Buy)?;
    let later =
        exposure::mark_from_pnl(entry, Decimal::from(100), volume, Decimal::ONE, Side::Buy)?;
    assert_eq!(
        (later - stop) * volume - (first - stop) * volume,
        Decimal::from(100)
    );
    // Any 100 increase in equity's floating profit also increases the monetary
    // mark->stop loss by 100; asynchronous live quotes cannot invent budget.
    Ok(())
}
#[test]
fn typed_submission_evidence_distinguishes_not_sent_from_ambiguity() {
    init();
    let mut evidence = crate::entry::SubmissionEvidence {
        attempted: true,
        absent: false,
    };
    evidence.observe(&Err(ctrader_api::RequestError::Unknown(
        "disconnect".into(),
    )));
    assert!(!evidence.absent);
    evidence.observe(&Err(ctrader_api::RequestError::NotSent(
        "queue full".into(),
    )));
    assert!(evidence.absent);
}
#[test]
fn slippage_allowance_is_not_spent_twice_when_deriving_fill_distance() -> Result<()> {
    use adaptive_risk::{AccountSnapshot, DayReference, LossEstimate, RiskState, VolumeLimits};
    init();
    let mut p = profile();
    let directory = tempfile::tempdir()?;
    p.state_directory = directory.path().to_owned();
    let time = p.history_from + chrono::Duration::seconds(1);
    let day = DayReference {
        day: p.risk.calendar.day_at(time)?,
        balance: p.initial_balance,
        equity: p.initial_balance,
    };
    let state = RiskState::enroll(p.identity(), p.initial_balance, day.clone(), p.risk.clone())?;
    adaptive_risk::store::FileStore::new(directory.path()).enroll(&state)?;
    let mut session = p.manager()?.begin()?;
    let a = AccountSnapshot {
        account_id: p.identity(),
        observed_at: time,
        quote_at: time,
        balance: p.initial_balance,
        equity: p.initial_balance,
        connected: true,
        complete: true,
        sequence: 1,
        day_reference: day,
        balance_high: p.initial_balance,
        equity_high: p.initial_balance,
        exposures: vec![],
        resolved_requests: Default::default(),
        setups_reconciled: true,
    };
    session.reconcile(&a, &StandardLossRule, time)?;
    let trade = ProposedTrade {
        instrument_id: "test".into(),
        overlapping: false,
        request_id: "r1".into(),
        setup_id: "setup".into(),
        strategy_id: "strategy".into(),
        estimate: LossEstimate {
            side: Side::Buy,
            price: Decimal::from(100),
            stop: Some(Decimal::from(99)),
            contract_size: Decimal::ONE,
            conversion: Decimal::ONE,
            costs_per_unit: Decimal::ZERO,
            fixed_costs: Decimal::ZERO,
            is_open_position: false,
        },
        volume: VolumeLimits {
            min: Decimal::ONE,
            max: Decimal::from(1000),
            step: Decimal::ONE,
        },
        requested_volume: Decimal::from(1000),
        requested_risk: Decimal::from(100),
    };
    let authorization = session.authorize(&a, trade.clone(), &StandardLossRule, time)?;
    let mut guard = Guard {
        session,
        profile: p,
        authorization,
        trade,
        cap: Decimal::ONE,
        transport_started: false,
    };
    assert_eq!(guard.max_stop_distance()?, 1.0);
    guard.outcome(true)?;
    guard.transport_started = true;
    drop(guard);
    let recovered = adaptive_risk::store::FileStore::new(directory.path())
        .lock()?
        .load()?;
    assert!(recovered.reservations.is_empty());
    Ok(())
}
#[test]
fn registration_activation_document_is_accepted_by_runtime_binding() -> Result<()> {
    init();
    let p = profile();
    let root = tempfile::tempdir()?;
    let (path, bytes) = registration::activation_file(&p, root.path())?;
    std::fs::write(&path, &bytes)?;
    registration::validate_activation(&p, &bytes)?;
    binding::check_at(root.path(), Some(&p), p.account_id, p.environment)?;
    let mut wrong = p;
    wrong.account_id += 1;
    assert!(registration::validate_activation(&wrong, &bytes).is_err());
    Ok(())
}
