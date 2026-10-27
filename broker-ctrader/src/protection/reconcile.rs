use super::*;
use adaptive_risk::{AccountSnapshot, DailyReference, DayReference, DrawdownReference};
use std::collections::BTreeSet;

pub(super) async fn collect(
    b: &OpenApiBroker,
    p: &PropAccount,
    s: &mut RiskSession,
) -> Result<AccountSnapshot> {
    ensure!(
        b.account_id() == p.account_id && b.shared.client.environment() == p.environment,
        "prop profile points to another broker account"
    );
    let revision = b.shared.risk_revision();
    // observed_at is the START of collection, so intervening fills cannot be hidden by
    // stamping an old account snapshot with the completion time of a later request.
    let captured = Utc::now();
    let day = p.risk.calendar.day_at(captured)?;
    let before = b.snapshot().await?;
    let trader = metadata::trader(b).await?;
    ensure!(
        wire::is_enum(&trader, "accountType", 0, "HEDGED"),
        "protected entries require confirmed hedged account"
    );
    let ledger = history::fetch(b, p, captured, day.start).await?;
    let expected_balance = ledger
        .events
        .iter()
        .try_fold(p.initial_balance, |total, e| plus(total, e.net_pnl))?;
    let balance = money(&trader, "balance")?;
    ensure!(
        expected_balance == balance,
        "broker balance does not reconcile with verified net executions; check fees/history/cashflows"
    );
    let event_ids: BTreeSet<_> = ledger.events.iter().map(|e| e.event_id.clone()).collect();
    ensure!(
        s.state().executions.keys().all(|id| event_ids.contains(id)),
        "broker history lost previously persisted events"
    );
    inventory::verify(&ledger.events, &before)?;
    for event in &ledger.events {
        s.on_execution(event.clone())?;
    }
    let mut oldest = captured;
    let unrealized = b.read(2187, 2188, json!({})).await?;
    wire::array_shape(&unrealized, "positionUnrealizedPnL")?;
    let digits = wire::integer(&unrealized, "moneyDigits")?;
    ensure!((0..=12).contains(&digits), "invalid unrealized precision");
    let mut pnl = Decimal::ZERO;
    let mut positions = BTreeSet::new();
    for row in wire::array(&unrealized, "positionUnrealizedPnL") {
        ensure!(
            positions.insert(wire::integer(row, "positionId")?),
            "duplicate unrealized position"
        );
        pnl = plus(
            pnl,
            divide(
                number(row, "netUnrealizedPnL")?,
                Decimal::from(10u64.pow(digits as u32)),
            )?,
        )?;
    }
    ensure!(
        positions
            == wire::array(&before, "position")
                .iter()
                .map(|v| wire::integer(v, "positionId"))
                .collect::<Result<BTreeSet<_>>>()?,
        "unrealized PnL does not cover exact open-position set"
    );
    let equity = plus(balance, pnl)?;
    let exposures = exposure::existing(
        b,
        p,
        &before,
        &trader,
        &ledger.position_requests,
        &mut oldest,
        &unrealized,
    )
    .await?;
    let mut evidence_day = DayReference {
        day: day.clone(),
        balance: ledger.boundary_balance,
        equity: ledger.boundary_balance,
    };
    let mut equity_high = s.state().equity_high.max(equity);
    let mut external_resolved = BTreeSet::new();
    let mut stop_flags = (
        ledger.account_breached,
        ledger.daily_breached.contains(&day.start),
    );
    let last = s.state().last_reconciled_at.unwrap_or(p.history_from);
    let mut prior_volumes = std::collections::BTreeMap::<&str, Decimal>::new();
    for event in ledger.events.iter().filter(|e| e.occurred_at <= last) {
        let previous = prior_volumes
            .get(event.position_id.as_str())
            .copied()
            .unwrap_or_default();
        prior_volumes.insert(&event.position_id, plus(previous, event.volume_delta)?);
    }
    let risk_in_gap = prior_volumes.values().any(|v| *v > Decimal::ZERO)
        || ledger
            .events
            .iter()
            .any(|e| e.occurred_at > last && e.volume_delta > Decimal::ZERO);
    let gap_requires_evidence =
        (captured - last).num_seconds() > p.risk.execution.max_data_age_seconds && risk_in_gap;
    if gap_requires_evidence
        || p.firm_evidence_file.is_some()
        || p.risk.daily_reference != DailyReference::Balance
        || p.risk.drawdown_reference == DrawdownReference::TrailingEquity
    {
        let path = p
            .firm_evidence_file
            .as_ref()
            .ok_or_else(|| eyre!("firm daily equity/reference peak evidence required"))?;
        let evidence: FirmEvidence = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            evidence.account_id == p.account_id
                && evidence.environment == p.environment
                && evidence.policy_version == p.risk.version
                && evidence.day.day == day
                && adaptive_risk::decision::fresh(
                    Utc::now(),
                    evidence.observed_at,
                    p.risk.execution.max_data_age_seconds
                ),
            "stale/wrong firm reference evidence"
        );
        ensure!(
            evidence.continuous_equity_high >= equity_high
                && evidence.day.balance == ledger.boundary_balance,
            "inconsistent firm reference evidence"
        );
        ensure!(
            evidence.resolved_requests.iter().all(|id| s
                .state()
                .reservations
                .get(id)
                .is_none_or(|r| r.created_at < evidence.observed_at)),
            "absence evidence predates request reservation"
        );
        external_resolved = evidence.resolved_requests;
        stop_flags.0 |= evidence.account_stop_breached;
        stop_flags.1 |= evidence.daily_stop_breached;
        evidence_day = evidence.day;
        equity_high = evidence.continuous_equity_high;
    }
    let live_requests: BTreeSet<_> = exposures
        .iter()
        .filter_map(|e| e.request_id.clone())
        .collect();
    ensure!(
        live_requests
            .iter()
            .all(|id| ledger.request_setups.contains_key(id)),
        "unattributed live request/setup"
    );
    let completed: BTreeSet<_> = ledger
        .events
        .iter()
        .map(|e| e.setup_id.clone())
        .filter(|id| {
            !live_requests
                .iter()
                .any(|request| ledger.request_setups.get(request) == Some(id))
        })
        .collect();
    for setup in completed {
        let end = ledger
            .events
            .iter()
            .filter(|e| e.setup_id == setup)
            .map(|e| e.occurred_at)
            .max()
            .ok_or_else(|| eyre!("missing setup completion"))?;
        s.on_setup_closed(&setup, end, true)?;
    }
    let mut resolved = external_resolved;
    for order in &ledger.orders {
        if let Some(id) = order["clientOrderId"].as_str() {
            let terminal = [
                (2, "ORDER_STATUS_FILLED"),
                (3, "ORDER_STATUS_REJECTED"),
                (4, "ORDER_STATUS_EXPIRED"),
                (5, "ORDER_STATUS_CANCELLED"),
            ]
            .iter()
            .any(|(n, name)| wire::is_enum(order, "orderStatus", *n, name));
            if terminal && !live_requests.contains(id) {
                resolved.insert(id.to_owned());
            }
        }
    }
    // A mutation that never reached cTrader has no order history. Its reservation
    // remains blocked until an explicit, broker-verified absence reconciliation.
    let after = b.snapshot().await?;
    let after_trader = metadata::trader(b).await?;
    ensure!(
        before == after
            && trader == after_trader
            && b.shared.risk_revision() == revision
            && b.shared.risk_healthy(),
        "account changed during reconciliation; retry read-only reconciliation"
    );
    let sequence = u64::try_from(captured.timestamp_micros())?;
    let snapshot = AccountSnapshot {
        account_id: p.identity(),
        observed_at: captured,
        quote_at: oldest,
        balance,
        equity,
        connected: !b.shared.client.is_closed(),
        complete: true,
        sequence,
        day_reference: evidence_day,
        balance_high: ledger.high.max(balance),
        equity_high,
        exposures,
        resolved_requests: resolved,
        setups_reconciled: true,
    };
    s.reconcile(&snapshot, &adaptive_risk::StandardLossRule, Utc::now())?;
    s.latch_stops(stop_flags.0, stop_flags.1)?;
    Ok(snapshot)
}
