use super::*;
use crate::*;
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure, eyre};
use std::collections::BTreeSet;
pub fn compare(
    policy: &RiskPolicy,
    trades: &[TradeRecord],
    dataset: Dataset,
    initial: Decimal,
    target_pct: Decimal,
) -> Result<Vec<StrategyReport>> {
    [
        Strategy::Fixed,
        Strategy::StreakOnly,
        Strategy::DrawdownOnly,
        Strategy::Combined,
    ]
    .into_iter()
    .map(|strategy| {
        Ok(StrategyReport {
            strategy,
            dataset,
            report: replay(policy, trades, initial, target_pct, strategy)?,
        })
    })
    .collect()
}
struct ReplayState {
    state: RiskState,
    equity: Decimal,
    high: Decimal,
    drawdown: Decimal,
    losses: u32,
    longest: u32,
    skipped: usize,
    total_risk: Decimal,
    total_reserved: Decimal,
    day_firm_breached: bool,
    taken: usize,
    daily_breaches: u64,
}
fn replay(
    p: &RiskPolicy,
    trades: &[TradeRecord],
    initial: Decimal,
    target: Decimal,
    strategy: Strategy,
) -> Result<ReplayReport> {
    p.validate()?;
    ensure!(
        initial > Decimal::ZERO && target > Decimal::ZERO && !trades.is_empty(),
        "invalid replay inputs"
    );
    ensure!(
        trades.windows(2).all(|w| w[0].at <= w[1].at),
        "replay trades must be chronological"
    );
    let first = trades.first().ok_or_else(|| eyre!("empty replay"))?;
    let day = DayReference {
        day: p.calendar.day_at(first.at)?,
        balance: initial,
        equity: initial,
    };
    let mut r = ReplayState {
        state: RiskState::enroll("simulation".into(), initial, day, p.clone())?,
        equity: initial,
        high: initial,
        drawdown: Decimal::ZERO,
        losses: 0,
        longest: 0,
        skipped: 0,
        total_risk: Decimal::ZERO,
        total_reserved: Decimal::ZERO,
        day_firm_breached: false,
        taken: 0,
        daily_breaches: 0,
    };
    let mut breaches = 0;
    let mut reached = None;
    for (i, trade) in trades.iter().enumerate() {
        let a = r.snapshot(p, trade, i as u64)?;
        let floors = StandardLossRule.thresholds(p, &r.state, &a)?;
        let risk = r.risk(p, &a, &floors, strategy, trade.at)?;
        if risk.nominal <= Decimal::ZERO {
            r.skipped += 1;
            continue;
        }
        r.taken += 1;
        r.total_risk = add(
            r.total_risk,
            div(mul(risk.nominal, Decimal::from(100))?, initial)?,
        )?;
        r.total_reserved = add(
            r.total_reserved,
            div(mul(risk.reserved, Decimal::from(100))?, initial)?,
        )?;
        r.equity = add(r.equity, mul(risk.nominal, trade.net_r)?)?;
        if matches!(strategy, Strategy::Combined) {
            r.latch(&floors);
        }
        r.high = r.high.max(r.equity);
        r.drawdown = r.drawdown.max(div(
            mul(sub(r.high, r.equity)?, Decimal::from(100))?,
            r.high,
        )?);
        r.state.balance_high = r.state.balance_high.max(r.equity);
        r.state.equity_high = r.state.equity_high.max(r.equity);
        if trade.net_r < Decimal::ZERO {
            r.losses = r.losses.saturating_add(1);
            r.longest = r.longest.max(r.losses);
            r.state.consecutive_losses = r.state.consecutive_losses.saturating_add(1);
        } else if trade.net_r > Decimal::ZERO {
            r.losses = 0;
            if p.streak_backoff.reset_on_win {
                r.state.consecutive_losses = 0;
            }
        } else if p.streak_backoff.reset_on_break_even {
            r.state.consecutive_losses = 0;
        }
        if r.equity <= floors.firm_daily_floor && !r.day_firm_breached {
            r.daily_breaches += 1;
            r.day_firm_breached = true;
        }
        if r.equity <= floors.firm_account_floor {
            breaches += 1;
            break;
        }
        if r.equity >= add(initial, pct(initial, target)?)? {
            reached = Some(r.taken);
            break;
        }
    }
    Ok(ReplayReport {
        net_return_pct: div(mul(sub(r.equity, initial)?, Decimal::from(100))?, initial)?,
        max_drawdown_pct: r.drawdown,
        longest_losing_streak: r.longest,
        skipped_trades: r.skipped,
        average_risk_pct: if r.taken == 0 {
            Decimal::ZERO
        } else {
            div(r.total_risk, Decimal::from(r.taken as u64))?
        },
        average_reserved_risk_pct: if r.taken == 0 {
            Decimal::ZERO
        } else {
            div(r.total_reserved, Decimal::from(r.taken as u64))?
        },
        daily_lockouts: r.state.metrics.circuit_breaker_activations,
        account_limit_breaches: breaches,
        daily_firm_limit_breaches: r.daily_breaches,
        trades_to_target: reached,
        funded: reached.is_some() && breaches == 0 && r.daily_breaches == 0,
    })
}
impl ReplayState {
    fn snapshot(
        &mut self,
        p: &RiskPolicy,
        t: &TradeRecord,
        sequence: u64,
    ) -> Result<AccountSnapshot> {
        let day = p.calendar.day_at(t.at)?;
        if self.state.day.day != day {
            self.state.day = DayReference {
                day,
                balance: self.equity,
                equity: self.equity,
            };
            self.state.daily_locked = false;
            self.day_firm_breached = false;
        }
        self.state.needs_reconciliation = false;
        self.state.last_sequence = Some(sequence);
        self.state.last_reconciled_at = Some(t.at);
        Ok(AccountSnapshot {
            account_id: "simulation".into(),
            observed_at: t.at,
            quote_at: t.at,
            balance: self.equity,
            equity: self.equity,
            connected: true,
            complete: true,
            sequence,
            day_reference: self.state.day.clone(),
            balance_high: self.state.balance_high,
            equity_high: self.state.equity_high,
            exposures: vec![],
            resolved_requests: BTreeSet::new(),
            setups_reconciled: true,
        })
    }
    fn latch(&mut self, f: &Thresholds) {
        if self.equity <= f.internal_daily_floor && !self.state.daily_locked {
            self.state.daily_locked = true;
            self.state.metrics.circuit_breaker_activations = self
                .state
                .metrics
                .circuit_breaker_activations
                .saturating_add(1);
        }
        if self.equity <= f.internal_account_floor {
            self.state.account_locked = true;
        }
    }
    fn risk(
        &mut self,
        p: &RiskPolicy,
        a: &AccountSnapshot,
        f: &Thresholds,
        mode: Strategy,
        at: DateTime<Utc>,
    ) -> Result<super::allocation::Allocation> {
        let t =
            super::allocation::proposed(self.state.initial_balance, p.base_risk_pct, a.sequence)?;
        let base = t.requested_risk;
        let dd = div(
            mul(
                sub(f.drawdown_reference, self.equity)?.max(Decimal::ZERO),
                Decimal::from(100),
            )?,
            f.drawdown_reference,
        )?;
        let budget = match mode {
            Strategy::Fixed => base,
            Strategy::StreakOnly => mul(base, p.streak_multiplier(self.state.consecutive_losses))?,
            Strategy::DrawdownOnly => {
                base.min(pct(self.state.initial_balance, p.drawdown_risk_pct(dd))?)
            }
            Strategy::Combined => {
                self.latch(f);
                let decision = evaluate_trade(p, a, &t, &self.state, &StandardLossRule, at);
                return super::allocation::at_volume(&t, decision.max_volume, p);
            }
        };
        super::allocation::size(&t, budget, p)
    }
}
