use crate::*;
use chrono::{DateTime, Utc};
use color_eyre::Result;
use serde::Serialize;
#[derive(Clone, Debug, Serialize)]
pub struct MetricsSnapshot {
    pub risk_multiplier: Decimal,
    pub consecutive_losses: u32,
    pub account_drawdown_pct: Decimal,
    pub daily_loss_pct: Decimal,
    pub daily_budget_remaining: Decimal,
    pub total_reserved_exposure: Decimal,
    pub blocked_trades: u64,
    pub circuit_breaker_activations: u64,
    pub reduced_risk_seconds: i64,
}
pub fn snapshot(
    s: &RiskState,
    a: &AccountSnapshot,
    rule: &dyn LossRule,
    now: DateTime<Utc>,
) -> Result<MetricsSnapshot> {
    let f = rule.thresholds(&s.policy, s, a)?;
    let dd = div(
        mul(
            sub(f.drawdown_reference, a.equity)?.max(Decimal::ZERO),
            Decimal::from(100),
        )?,
        f.drawdown_reference,
    )?;
    let daily = div(
        mul(
            sub(f.daily_reference, a.equity)?.max(Decimal::ZERO),
            Decimal::from(100),
        )?,
        f.daily_reference,
    )?;
    let exposure = decision::reserved_risk(&s.policy, s, a)?;
    let multiplier = s
        .policy
        .streak_multiplier(s.consecutive_losses)
        .min(div(s.policy.drawdown_risk_pct(dd), s.policy.base_risk_pct)?);
    Ok(MetricsSnapshot {
        risk_multiplier: if s.daily_locked || s.account_locked {
            Decimal::ZERO
        } else {
            multiplier
        },
        consecutive_losses: s.consecutive_losses,
        account_drawdown_pct: dd,
        daily_loss_pct: daily,
        daily_budget_remaining: sub(sub(a.equity, f.internal_daily_floor)?, exposure)?
            .max(Decimal::ZERO),
        total_reserved_exposure: exposure,
        blocked_trades: s.metrics.blocked_trades,
        circuit_breaker_activations: s.metrics.circuit_breaker_activations,
        reduced_risk_seconds: s.metrics.reduced_risk_seconds.saturating_add(
            s.metrics
                .reduced_since
                .map_or(0, |at| (now - at).num_seconds().max(0)),
        ),
    })
}
