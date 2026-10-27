use crate::{
    DailyReference, Decimal, DrawdownReference, Exposure, RiskPolicy, add,
    calendar::TradingDay,
    ledger::{Execution, SetupOutcome},
    pct, sub,
};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DayReference {
    pub day: TradingDay,
    /// Firm-defined reference captured at the boundary, never inferred from a late first tick.
    pub balance: Decimal,
    pub equity: Decimal,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub account_id: String,
    pub observed_at: DateTime<Utc>,
    pub quote_at: DateTime<Utc>,
    pub balance: Decimal,
    pub equity: Decimal,
    pub connected: bool,
    pub complete: bool,
    /// Monotonic adapter cursor. Includes fills, balance changes and order changes.
    pub sequence: u64,
    pub day_reference: DayReference,
    /// Trusted continuous broker high-water marks, including disconnection intervals.
    pub balance_high: Decimal,
    pub equity_high: Decimal,
    pub exposures: Vec<Exposure>,
    /// Positive proof that these requests are fully absent (including historical fills).
    pub resolved_requests: BTreeSet<String>,
    pub setups_reconciled: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub request_id: String,
    pub setup_id: String,
    pub risk: Decimal,
    pub volume: Decimal,
    pub created_at: DateTime<Utc>,
    /// Set before handing to transport. Cancellation/restart must reconcile this request.
    pub submitted: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskState {
    pub schema: u32,
    pub account_id: String,
    /// The full policy is pinned as well as its version; edits require explicit migration.
    pub policy: RiskPolicy,
    pub initial_balance: Decimal,
    pub balance_high: Decimal,
    pub equity_high: Decimal,
    pub day: DayReference,
    pub daily_locked: bool,
    pub account_locked: bool,
    pub consecutive_losses: u32,
    pub reservations: BTreeMap<String, Reservation>,
    pub attempted_requests: BTreeSet<String>,
    pub executions: BTreeMap<String, Execution>,
    pub outcomes: BTreeMap<String, SetupOutcome>,
    pub last_sequence: Option<u64>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
    pub needs_reconciliation: bool,
    pub invalidated_after: Option<DateTime<Utc>>,
    pub metrics: RiskMetrics,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RiskMetrics {
    pub blocked_trades: u64,
    pub circuit_breaker_activations: u64,
    pub reduced_risk_seconds: i64,
    pub reduced_since: Option<DateTime<Utc>>,
}
impl RiskState {
    /// Explicit enrollment. Caller supplies verified account history and boundary evidence.
    pub fn enroll(
        account_id: String,
        initial_balance: Decimal,
        day: DayReference,
        policy: RiskPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        ensure!(
            !account_id.is_empty()
                && initial_balance > Decimal::ZERO
                && day.balance > Decimal::ZERO
                && day.equity > Decimal::ZERO
                && day.day.start < day.day.end,
            "invalid enrollment"
        );
        Ok(Self {
            schema: 1,
            account_id,
            policy,
            initial_balance,
            balance_high: initial_balance.max(day.balance),
            equity_high: initial_balance.max(day.equity),
            day,
            daily_locked: false,
            account_locked: false,
            consecutive_losses: 0,
            reservations: BTreeMap::new(),
            attempted_requests: BTreeSet::new(),
            executions: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            last_sequence: None,
            last_reconciled_at: None,
            needs_reconciliation: true,
            invalidated_after: None,
            metrics: RiskMetrics::default(),
        })
    }
}
#[derive(Clone, Debug)]
pub struct Thresholds {
    pub risk_reference: Decimal,
    pub drawdown_reference: Decimal,
    pub internal_account_floor: Decimal,
    pub firm_account_floor: Decimal,
    pub daily_reference: Decimal,
    pub internal_daily_floor: Decimal,
    pub firm_daily_floor: Decimal,
}
/// Custom firm formulas must return actual monetary floors, including the configured
/// safety margin to the firm's floor. Version custom logic through RiskPolicy::version.
pub trait LossRule: Send + Sync {
    fn thresholds(
        &self,
        policy: &RiskPolicy,
        state: &RiskState,
        snapshot: &AccountSnapshot,
    ) -> Result<Thresholds>;
}
#[derive(Default)]
pub struct StandardLossRule;
impl LossRule for StandardLossRule {
    fn thresholds(&self, p: &RiskPolicy, s: &RiskState, a: &AccountSnapshot) -> Result<Thresholds> {
        let reference = match p.drawdown_reference {
            DrawdownReference::StaticInitialBalance => s.initial_balance,
            DrawdownReference::TrailingBalance => s.balance_high.max(a.balance_high).max(a.balance),
            DrawdownReference::TrailingEquity => s.equity_high.max(a.equity_high).max(a.equity),
        };
        let allowance = if p.trailing_allowance_tracks_reference {
            reference
        } else {
            s.initial_balance
        };
        let daily = match p.daily_reference {
            DailyReference::Balance => s.day.balance,
            DailyReference::Equity => s.day.equity,
            DailyReference::HigherOfBalanceAndEquity => s.day.balance.max(s.day.equity),
        };
        let firm_account = sub(reference, pct(allowance, p.max_account_loss_pct)?)?;
        let firm_daily = sub(daily, pct(daily, p.daily_loss_limit_pct)?)?;
        Ok(Thresholds {
            risk_reference: s.initial_balance,
            drawdown_reference: reference,
            internal_account_floor: sub(reference, pct(allowance, p.internal_account_stop_pct)?)?
                .max(add(firm_account, pct(allowance, p.safety_buffer_pct)?)?),
            firm_account_floor: firm_account,
            daily_reference: daily,
            internal_daily_floor: sub(daily, pct(daily, p.internal_daily_stop_pct)?)?
                .max(add(firm_daily, pct(daily, p.safety_buffer_pct)?)?),
            firm_daily_floor: firm_daily,
        })
    }
}
