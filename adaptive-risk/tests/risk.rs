use adaptive_risk::{calendar::*, ledger::Execution, store::FileStore, *};
use chrono::{DateTime, Duration, Utc};
use color_eyre::Result;
use std::collections::BTreeSet;
use tracing_subscriber::prelude::*;
fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}
fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
fn now() -> DateTime<Utc> {
    "2026-10-27T12:00:00Z".parse().unwrap()
}
fn fixture() -> (RiskPolicy, RiskState, AccountSnapshot, ProposedTrade) {
    init();
    let p = RiskPolicy::default();
    let time = now();
    let day = DayReference {
        day: p.calendar.day_at(time).unwrap(),
        balance: d("10000"),
        equity: d("10000"),
    };
    let mut s = RiskState::enroll("test".into(), d("10000"), day.clone(), p.clone()).unwrap();
    s.needs_reconciliation = false;
    s.last_sequence = Some(1);
    s.last_reconciled_at = Some(time);
    let a = AccountSnapshot {
        account_id: "test".into(),
        observed_at: time,
        quote_at: time,
        balance: d("10000"),
        equity: d("10000"),
        connected: true,
        complete: true,
        sequence: 1,
        day_reference: day,
        balance_high: d("10000"),
        equity_high: d("10000"),
        exposures: vec![],
        resolved_requests: BTreeSet::new(),
        setups_reconciled: true,
    };
    let t = ProposedTrade {
        instrument_id: "TEST".into(),
        overlapping: false,
        request_id: "r1".into(),
        strategy_id: "strategy".into(),
        setup_id: "s1".into(),
        estimate: LossEstimate {
            side: Side::Buy,
            price: d("100"),
            stop: Some(d("99")),
            contract_size: Decimal::ONE,
            conversion: Decimal::ONE,
            costs_per_unit: Decimal::ZERO,
            fixed_costs: Decimal::ZERO,
            is_open_position: false,
        },
        volume: VolumeLimits {
            min: d("0.01"),
            max: d("1000000"),
            step: d("0.01"),
        },
        requested_volume: d("1000000"),
        requested_risk: d("100"),
    };
    (p, s, a, t)
}
fn eval(p: &RiskPolicy, s: &RiskState, a: &AccountSnapshot, t: &ProposedTrade) -> RiskDecision {
    evaluate_trade(p, a, t, s, &StandardLossRule, now())
}
fn close(s: &mut RiskState, id: &str, pnl: &str, offset: i64) {
    let at = now() + Duration::seconds(offset);
    for (eid, v, n) in [("open", d("1"), d("0")), ("close", d("-1"), d(pnl))] {
        s.on_execution(Execution {
            event_id: format!("{id}:{eid}"),
            setup_id: id.into(),
            position_id: id.into(),
            occurred_at: at,
            volume_delta: v,
            net_pnl: n,
        })
        .unwrap();
    }
    s.on_setup_closed(id, at, true).unwrap();
    s.needs_reconciliation = false;
    s.invalidated_after = None;
}
#[test]
fn loss_tiers_win_break_even_and_duplicate_events() {
    let (p, mut s, a, t) = fixture();
    for (n, max) in [
        (0, "100"),
        (1, "75"),
        (2, "50"),
        (3, "25"),
        (4, "10"),
        (5, "10"),
    ] {
        let result = eval(&p, &s, &a, &t);
        assert!(result.approved);
        assert!(result.allowed_risk_amount <= d(max));
        assert!(result.allowed_risk_amount > d(max) - d("0.02"));
        close(&mut s, &format!("loss-{n}"), "-1", n);
    }
    let losses = s.consecutive_losses;
    close(&mut s, "be", "0", 10);
    assert_eq!(losses, s.consecutive_losses);
    close(&mut s, "win", "2", 11);
    assert_eq!(s.consecutive_losses, 0);
    let e = s.executions.values().next().unwrap().clone();
    s.on_execution(e.clone()).unwrap();
    let mut bad = e;
    bad.net_pnl = d("9");
    assert!(s.on_execution(bad).is_err());
    assert!(s.needs_reconciliation);
}
#[test]
fn drawdown_boundaries_and_recovery() {
    let (p, s, mut a, t) = fixture();
    for (equity, ceiling) in [
        ("9750.01", "100"),
        ("9750", "50"),
        ("9650", "25"),
        ("9550", "10"),
        ("9500", "0"),
    ] {
        // Isolate account drawdown from daily allowance by supplying a matching lower daily reference.
        a.equity = d(equity);
        a.day_reference.balance = d(equity);
        a.day_reference.equity = d(equity);
        let mut state = s.clone();
        state.day = a.day_reference.clone();
        let result = eval(&p, &state, &a, &t);
        assert!(result.allowed_risk_amount <= d(ceiling));
        if ceiling != "0" {
            assert!(result.approved);
            assert!(result.allowed_risk_amount > d(ceiling) - d("0.02"));
        }
    }
    a.equity = d("10000");
    a.day_reference = s.day.clone();
    assert!(eval(&p, &s, &a, &t).allowed_risk_amount > d("99"));
}
#[test]
fn stop_staleness_missing_data_and_minimum_reject() {
    let (p, s, mut a, mut t) = fixture();
    t.estimate.stop = None;
    assert!(!eval(&p, &s, &a, &t).approved);
    t.estimate.stop = Some(d("99"));
    t.volume.min = d("100");
    assert_eq!(
        eval(&p, &s, &a, &t).limiting_policy,
        RiskLimitReason::BelowMinimum
    );
    t.volume.min = d("0.01");
    a.observed_at -= Duration::seconds(6);
    assert_eq!(
        eval(&p, &s, &a, &t).limiting_policy,
        RiskLimitReason::StaleData
    );
    a.observed_at = now();
    a.complete = false;
    assert!(!eval(&p, &s, &a, &t).approved);
    a.complete = true;
    a.connected = false;
    assert_eq!(
        eval(&p, &s, &a, &t).limiting_policy,
        RiskLimitReason::Disconnected
    );
}
#[test]
fn existing_exposure_costs_slippage_and_no_double_counted_float_loss() {
    let (p, s, mut a, t) = fixture();
    a.equity = d("9900");
    let mut estimate = t.estimate.clone();
    estimate.price = d("99.5");
    estimate.is_open_position = true;
    estimate.costs_per_unit = d("0.1");
    a.exposures.push(Exposure {
        broker_id: "position:1".into(),
        request_id: None,
        volume: d("100"),
        estimate,
        overlapping: false,
    });
    // Equity already contains 100 loss; only mark->stop (50), future fees (10), slip (10) reserved.
    let r = eval(&p, &s, &a, &t);
    assert!(r.approved);
    assert_eq!(r.reserved_exposure, d("70"));
    assert!(r.allowed_risk_amount <= d("30"));
    assert!(
        t.estimate
            .loss(r.max_volume, p.execution.slippage_buffer_pct_of_risk)
            .unwrap()
            <= d("30")
    );
}
#[test]
fn daily_dst_boundaries_are_half_open() {
    init();
    let c = TradingCalendar::default();
    for (stamp, hours) in [("2026-03-08T20:59:59Z", 23), ("2026-11-01T21:59:59Z", 25)] {
        let at: DateTime<Utc> = stamp.parse().unwrap();
        let day = c.day_at(at).unwrap();
        assert_eq!((day.end - day.start).num_hours(), hours);
        assert_ne!(c.day_at(day.end).unwrap(), day);
        assert_eq!(c.day_at(day.end - Duration::milliseconds(1)).unwrap(), day);
    }
}
#[test]
fn static_trailing_balance_and_equity_floors() {
    let (mut p, mut s, mut a, _) = fixture();
    a.balance_high = d("11000");
    a.equity_high = d("12000");
    assert_eq!(
        StandardLossRule
            .thresholds(&p, &s, &a)
            .unwrap()
            .firm_account_floor,
        d("9400")
    );
    p.drawdown_reference = DrawdownReference::TrailingBalance;
    s.policy = p.clone();
    assert_eq!(
        StandardLossRule
            .thresholds(&p, &s, &a)
            .unwrap()
            .firm_account_floor,
        d("10400")
    );
    p.drawdown_reference = DrawdownReference::TrailingEquity;
    assert_eq!(
        StandardLossRule
            .thresholds(&p, &s, &a)
            .unwrap()
            .firm_account_floor,
        d("11400")
    );
}
#[test]
fn partial_closes_multiple_positions_and_deterministic_attribution() {
    let (_, mut s, _, _) = fixture();
    for (id, pos, v, pnl) in [
        ("o1", "p1", "2", "-1"),
        ("o2", "p2", "1", "-1"),
        ("c1", "p1", "-1", "5"),
        ("c2", "p1", "-1", "-6"),
        ("c3", "p2", "-1", "1"),
    ] {
        s.on_execution(Execution {
            event_id: id.into(),
            setup_id: "setup".into(),
            position_id: pos.into(),
            occurred_at: now(),
            volume_delta: d(v),
            net_pnl: d(pnl),
        })
        .unwrap();
    }
    assert!(s.on_setup_closed("setup", now(), false).is_err());
    s.on_setup_closed("setup", now(), true).unwrap();
    assert_eq!(s.consecutive_losses, 1);
    assert_eq!(s.outcomes["setup"].net_pnl, d("-2"));
    close(&mut s, "z-win", "1", 1);
    close(&mut s, "a-loss", "-1", 1);
    assert_eq!(s.consecutive_losses, 0);
}
#[test]
fn persisted_reservations_concurrency_restart_ambiguity_and_reconnection() -> Result<()> {
    let (p, s, mut a, t) = fixture();
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    store.enroll(&s)?;
    let manager = RiskManager::new(store, "test".into(), p)?;
    let mut session = manager.begin()?;
    assert!(manager.begin().is_err());
    session.reconcile(&a, &StandardLossRule, now())?;
    let auth = session.authorize(&a, t.clone(), &StandardLossRule, now())?;
    session.before_submit(&auth, &t, &a, &StandardLossRule, now())?;
    assert!(
        session
            .before_submit(&auth, &t, &a, &StandardLossRule, now())
            .is_err()
    );
    drop(session);
    let mut session = manager.begin()?;
    assert!(session.state().needs_reconciliation);
    assert!(session.reconcile(&a, &StandardLossRule, now()).is_err());
    a.resolved_requests.insert(t.request_id);
    session.reconcile(&a, &StandardLossRule, now())?;
    assert!(session.state().reservations.is_empty());
    let event = Execution {
        event_id: "persist-open".into(),
        setup_id: "persist".into(),
        position_id: "p".into(),
        occurred_at: now(),
        volume_delta: d("1"),
        net_pnl: Decimal::ZERO,
    };
    session.on_execution(event)?;
    session.on_execution(Execution {
        event_id: "persist-close".into(),
        setup_id: "persist".into(),
        position_id: "p".into(),
        occurred_at: now(),
        volume_delta: d("-1"),
        net_pnl: d("-1"),
    })?;
    session.on_setup_closed("persist", now(), true)?;
    drop(session);
    assert_eq!(manager.begin()?.state().consecutive_losses, 1);
    Ok(())
}
#[test]
fn reservations_for_two_orders_cannot_spend_same_budget() -> Result<()> {
    let (p, s, a, mut t) = fixture();
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    store.enroll(&s)?;
    let manager = RiskManager::new(store, "test".into(), p)?;
    let mut session = manager.begin()?;
    session.reconcile(&a, &StandardLossRule, now())?;
    let one = session.authorize(&a, t.clone(), &StandardLossRule, now())?;
    t.request_id = "r2".into();
    let two = session.authorize(&a, t.clone(), &StandardLossRule, now())?;
    t.request_id = "r3".into();
    let third = session.authorize(&a, t, &StandardLossRule, now());
    assert!(one.decision().allowed_risk_amount + two.decision().allowed_risk_amount <= d("200"));
    assert!(third.is_err());
    Ok(())
}
#[test]
fn lockouts_latch_and_only_daily_lock_resets_at_verified_boundary() -> Result<()> {
    let (p, s, mut a, t) = fixture();
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    store.enroll(&s)?;
    let manager = RiskManager::new(store, "test".into(), p)?;
    let mut session = manager.begin()?;
    a.equity = d("9800");
    session.reconcile(&a, &StandardLossRule, now())?;
    assert!(session.state().daily_locked);
    a.equity = d("10000");
    session.reconcile(&a, &StandardLossRule, now())?;
    assert!(
        session
            .authorize(&a, t.clone(), &StandardLossRule, now())
            .is_err()
    );
    let tomorrow = now() + Duration::days(1);
    a.observed_at = tomorrow;
    a.quote_at = tomorrow;
    a.day_reference.day = session.state().policy.calendar.day_at(tomorrow)?;
    a.sequence += 1;
    session.reconcile(&a, &StandardLossRule, tomorrow)?;
    assert!(!session.state().daily_locked);
    a.equity = d("9500");
    session.reconcile(&a, &StandardLossRule, tomorrow)?;
    assert!(session.state().account_locked);
    drop(session);
    let session = manager.begin()?;
    assert!(session.state().account_locked);
    assert!(session.state().daily_locked);
    Ok(())
}

#[test]
fn fresh_price_or_fx_increase_invalidates_authorization() -> Result<()> {
    let (p, s, a, t) = fixture();
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    store.enroll(&s)?;
    let manager = RiskManager::new(store, "test".into(), p)?;
    let mut session = manager.begin()?;
    session.reconcile(&a, &StandardLossRule, now())?;
    let token = session.authorize(&a, t.clone(), &StandardLossRule, now())?;
    let mut moved = t.clone();
    moved.estimate.price = d("101");
    assert!(
        session
            .before_submit(&token, &moved, &a, &StandardLossRule, now())
            .is_err()
    );
    moved = t.clone();
    moved.estimate.conversion = d("1.1");
    assert!(
        session
            .before_submit(&token, &moved, &a, &StandardLossRule, now())
            .is_err()
    );
    session.before_submit(&token, &t, &a, &StandardLossRule, now())?;
    Ok(())
}
#[test]
fn tokens_cannot_cross_accounts_and_fills_invalidate_snapshots() -> Result<()> {
    let (p, s, a, t) = fixture();
    let one = tempfile::tempdir()?;
    let two = tempfile::tempdir()?;
    let store = FileStore::new(one.path());
    store.enroll(&s)?;
    let mgr = RiskManager::new(store, "test".into(), p.clone())?;
    let mut session = mgr.begin()?;
    session.reconcile(&a, &StandardLossRule, now())?;
    let token = session.authorize(&a, t.clone(), &StandardLossRule, now())?;
    let mut other = s.clone();
    other.account_id = "other".into();
    let store = FileStore::new(two.path());
    store.enroll(&other)?;
    let mgr = RiskManager::new(store, "other".into(), p)?;
    let mut other = mgr.begin()?;
    let mut snapshot = a.clone();
    snapshot.account_id = "other".into();
    other.reconcile(&snapshot, &StandardLossRule, now())?;
    other.authorize(&snapshot, t.clone(), &StandardLossRule, now())?;
    assert!(
        other
            .before_submit(&token, &t, &snapshot, &StandardLossRule, now())
            .is_err()
    );
    assert!(other.rejected_without_fill(&token).is_err());
    session.on_execution(Execution {
        event_id: "fill".into(),
        setup_id: "s1".into(),
        position_id: "p1".into(),
        occurred_at: now(),
        volume_delta: Decimal::ONE,
        net_pnl: Decimal::ZERO,
    })?;
    assert!(session.state().needs_reconciliation);
    session.disconnect()?;
    assert!(session.reconcile(&a, &StandardLossRule, now()).is_err());
    let mut next = t;
    next.request_id = "next".into();
    assert!(
        session
            .authorize(&a, next, &StandardLossRule, now())
            .is_err()
    );
    Ok(())
}
#[test]
fn correlation_surcharge_is_reserved_before_submission_and_completed_setups_reject() {
    let (mut p, mut s, a, mut t) = fixture();
    p.execution.overlap_multiplier = d("2");
    s.policy = p.clone();
    t.overlapping = true;
    let result = eval(&p, &s, &a, &t);
    assert!(result.approved);
    assert!(result.max_volume < d("42"));
    assert!(result.allowed_risk_amount <= d("100"));
    assert_eq!(
        result.allowed_risk_amount,
        t.estimate
            .loss(result.max_volume, p.execution.slippage_buffer_pct_of_risk)
            .unwrap()
            * d("2")
    );
    close(&mut s, &t.setup_id, "1", 0);
    assert_eq!(
        eval(&p, &s, &a, &t).limiting_policy,
        RiskLimitReason::InvalidTrade
    );
}
#[test]
fn corrupt_missing_or_changed_policy_state_never_enrolls_implicitly() -> Result<()> {
    let (p, s, _, _) = fixture();
    let dir = tempfile::tempdir()?;
    let store = FileStore::new(dir.path());
    let mgr = RiskManager::new(store.clone(), "test".into(), p.clone())?;
    assert!(mgr.begin().is_err());
    store.enroll(&s)?;
    let mut changed = p;
    changed.base_risk_pct = d("0.5");
    changed.drawdown_backoff.enabled = false;
    changed
        .drawdown_backoff
        .levels
        .iter_mut()
        .for_each(|(_, r)| *r = (*r).min(d("0.5")));
    assert!(
        RiskManager::new(store, "test".into(), changed)?
            .begin()
            .is_err()
    );
    std::fs::write(dir.path().join("state.json"), b"truncated")?;
    assert!(mgr.begin().is_err());
    Ok(())
}
