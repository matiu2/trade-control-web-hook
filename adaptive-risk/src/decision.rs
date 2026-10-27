use crate::*;
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposedTrade {
    pub instrument_id: String,
    pub overlapping: bool,
    pub request_id: String,
    pub strategy_id: String,
    pub setup_id: String,
    pub estimate: LossEstimate,
    pub volume: VolumeLimits,
    pub requested_volume: Decimal,
    pub requested_risk: Decimal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskLimitReason {
    BaseRisk,
    Streak,
    Drawdown,
    DailyBudget,
    AccountBudget,
    InvalidPolicy,
    Reconciliation,
    StaleData,
    Disconnected,
    InvalidExposure,
    InvalidTrade,
    BelowMinimum,
    DailyLockout,
    AccountLockout,
    DuplicateRequest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskDecision {
    pub approved: bool,
    pub allowed_risk_pct: Decimal,
    pub allowed_risk_amount: Decimal,
    pub max_volume: Decimal,
    pub limiting_policy: RiskLimitReason,
    pub reasons: Vec<RiskLimitReason>,
    pub reserved_exposure: Decimal,
    pub drawdown_pct: Decimal,
    pub daily_loss_pct: Decimal,
    pub daily_remaining: Decimal,
}
impl RiskDecision {
    pub fn blocked(reason: RiskLimitReason) -> Self {
        Self {
            approved: false,
            allowed_risk_pct: Decimal::ZERO,
            allowed_risk_amount: Decimal::ZERO,
            max_volume: Decimal::ZERO,
            limiting_policy: reason,
            reasons: vec![reason],
            reserved_exposure: Decimal::ZERO,
            drawdown_pct: Decimal::ZERO,
            daily_loss_pct: Decimal::ZERO,
            daily_remaining: Decimal::ZERO,
        }
    }
}
pub fn fresh(now: DateTime<Utc>, then: DateTime<Utc>, age: i64) -> bool {
    then <= now && now.signed_duration_since(then).num_milliseconds() <= age.saturating_mul(1000)
}
pub fn validate_snapshot(
    p: &RiskPolicy,
    s: &RiskState,
    a: &AccountSnapshot,
    now: DateTime<Utc>,
) -> Result<()> {
    ensure!(
        a.account_id == s.account_id && a.complete && a.setups_reconciled,
        "incomplete account reconciliation"
    );
    ensure!(
        s.invalidated_after.is_none_or(|time| a.observed_at > time),
        "snapshot predates execution/disconnection invalidation"
    );
    ensure!(
        a.balance > Decimal::ZERO
            && a.equity > Decimal::ZERO
            && a.balance_high >= a.balance
            && a.equity_high >= a.equity,
        "inconsistent account values"
    );
    ensure!(
        fresh(now, a.observed_at, p.execution.max_data_age_seconds)
            && fresh(now, a.quote_at, p.execution.max_data_age_seconds),
        "stale account/quote"
    );
    ensure!(
        p.calendar.day_at(now)? == a.day_reference.day
            && a.day_reference.balance > Decimal::ZERO
            && a.day_reference.equity > Decimal::ZERO,
        "missing day boundary evidence"
    );
    ensure!(
        s.last_sequence.is_none_or(|seq| a.sequence >= seq),
        "broker cursor regressed"
    );
    Ok(())
}
/// Reservations reflected in a broker snapshot are replaced by the sum of ALL matching
/// fragments, with the original reservation as a conservative floor until terminal proof.
pub fn reserved_risk(p: &RiskPolicy, s: &RiskState, a: &AccountSnapshot) -> Result<Decimal> {
    let mut ids = BTreeSet::new();
    let mut by_request = BTreeMap::<&str, Decimal>::new();
    let mut unlinked = Decimal::ZERO;
    for exposure in &a.exposures {
        ensure!(ids.insert(&exposure.broker_id), "duplicate broker exposure");
        let risk = exposure.risk(
            p.execution.slippage_buffer_pct_of_risk,
            p.execution.overlap_multiplier,
        )?;
        if let Some(id) = &exposure.request_id {
            ensure!(
                !a.resolved_requests.contains(id),
                "resolved request still has exposure"
            );
            let previous = by_request.get(id.as_str()).copied().unwrap_or_default();
            by_request.insert(id, add(previous, risk)?);
        } else {
            unlinked = add(unlinked, risk)?;
        }
    }
    for (id, reservation) in &s.reservations {
        ensure!(
            reservation.risk >= Decimal::ZERO && reservation.volume > Decimal::ZERO,
            "invalid persisted reservation"
        );
        let broker_risk = by_request.remove(id.as_str()).unwrap_or_default();
        unlinked = add(unlinked, broker_risk.max(reservation.risk))?;
    }
    by_request
        .values()
        .try_fold(unlinked, |total, risk| add(total, *risk))
}
pub fn evaluate_trade(
    p: &RiskPolicy,
    a: &AccountSnapshot,
    proposed: &ProposedTrade,
    s: &RiskState,
    rule: &dyn LossRule,
    now: DateTime<Utc>,
) -> RiskDecision {
    use RiskLimitReason::*;
    if p.validate().is_err() || p != &s.policy {
        return RiskDecision::blocked(InvalidPolicy);
    }
    if !a.connected {
        return RiskDecision::blocked(Disconnected);
    }
    if !fresh(now, a.observed_at, p.execution.max_data_age_seconds)
        || !fresh(now, a.quote_at, p.execution.max_data_age_seconds)
    {
        return RiskDecision::blocked(StaleData);
    }
    if s.needs_reconciliation
        || validate_snapshot(p, s, a, now).is_err()
        || s.day != a.day_reference
        || s.last_sequence != Some(a.sequence)
        || s.last_reconciled_at != Some(a.observed_at)
    {
        return RiskDecision::blocked(Reconciliation);
    }
    if s.account_locked {
        return RiskDecision::blocked(AccountLockout);
    }
    if s.daily_locked {
        return RiskDecision::blocked(DailyLockout);
    }
    if s.attempted_requests.contains(&proposed.request_id) {
        return RiskDecision::blocked(DuplicateRequest);
    }
    if s.outcomes.contains_key(&proposed.setup_id)
        || proposed.instrument_id.is_empty()
        || proposed.request_id.is_empty()
        || proposed.setup_id.is_empty()
        || proposed.strategy_id.is_empty()
        || proposed.requested_risk <= Decimal::ZERO
        || proposed.estimate.is_open_position
    {
        return RiskDecision::blocked(InvalidTrade);
    }
    match calculate(p, a, proposed, s, rule) {
        Ok(d) => d,
        Err(_) => RiskDecision::blocked(InvalidExposure),
    }
}
fn calculate(
    p: &RiskPolicy,
    a: &AccountSnapshot,
    t: &ProposedTrade,
    s: &RiskState,
    rule: &dyn LossRule,
) -> Result<RiskDecision> {
    use RiskLimitReason::*;
    let f = rule.thresholds(p, s, a)?;
    ensure!(
        f.risk_reference > Decimal::ZERO
            && f.drawdown_reference > Decimal::ZERO
            && f.daily_reference > Decimal::ZERO
            && f.internal_account_floor > f.firm_account_floor
            && f.internal_daily_floor > f.firm_daily_floor,
        "invalid loss rule floors"
    );
    let reserved = reserved_risk(p, s, a)?;
    let drawdown = div(
        mul(
            sub(f.drawdown_reference, a.equity)?.max(Decimal::ZERO),
            Decimal::from(100),
        )?,
        f.drawdown_reference,
    )?;
    let daily_loss = div(
        mul(
            sub(f.daily_reference, a.equity)?.max(Decimal::ZERO),
            Decimal::from(100),
        )?,
        f.daily_reference,
    )?;
    let base = pct(f.risk_reference, p.base_risk_pct)?.min(t.requested_risk);
    let limits = [
        (base, BaseRisk),
        (
            mul(
                pct(f.risk_reference, p.base_risk_pct)?,
                p.streak_multiplier(s.consecutive_losses),
            )?,
            Streak,
        ),
        (
            pct(f.risk_reference, p.drawdown_risk_pct(drawdown))?,
            Drawdown,
        ),
        (
            sub(sub(a.equity, f.internal_daily_floor)?, reserved)?.max(Decimal::ZERO),
            DailyBudget,
        ),
        (
            sub(sub(a.equity, f.internal_account_floor)?, reserved)?.max(Decimal::ZERO),
            AccountBudget,
        ),
    ];
    let (budget, reason) = limits
        .iter()
        .min_by_key(|(amount, _)| amount)
        .copied()
        .unwrap_or((Decimal::ZERO, InvalidPolicy));
    if budget <= Decimal::ZERO {
        return Ok(RiskDecision::blocked(reason));
    }
    let multiplier = if t.overlapping {
        p.execution.overlap_multiplier
    } else {
        Decimal::ONE
    };
    let sizing_budget = div(budget, multiplier)?;
    let volume = match t.volume.size(
        sizing_budget,
        &t.estimate,
        p.execution.slippage_buffer_pct_of_risk,
        t.requested_volume,
    ) {
        Ok(volume) => volume,
        Err(_) => return Ok(RiskDecision::blocked(BelowMinimum)),
    };
    let amount = mul(
        t.estimate
            .loss(volume, p.execution.slippage_buffer_pct_of_risk)?,
        multiplier,
    )?;
    Ok(RiskDecision {
        approved: true,
        allowed_risk_pct: div(mul(amount, Decimal::from(100))?, f.risk_reference)?,
        allowed_risk_amount: amount,
        max_volume: volume,
        limiting_policy: reason,
        reasons: limits
            .iter()
            .filter(|(n, _)| *n == budget)
            .map(|(_, r)| *r)
            .collect(),
        reserved_exposure: reserved,
        drawdown_pct: drawdown,
        daily_loss_pct: daily_loss,
        daily_remaining: limits[3].0,
    })
}
