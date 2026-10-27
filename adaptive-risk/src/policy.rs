use crate::{Decimal, calendar::TradingCalendar};
use color_eyre::eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskPolicy {
    pub version: String,
    pub base_risk_pct: Decimal,
    pub max_account_loss_pct: Decimal,
    pub internal_account_stop_pct: Decimal,
    pub daily_loss_limit_pct: Decimal,
    pub internal_daily_stop_pct: Decimal,
    pub safety_buffer_pct: Decimal,
    pub drawdown_reference: DrawdownReference,
    pub daily_reference: DailyReference,
    /// Fixed initial-balance allowance for trailing limits; opt in to reference-relative allowance.
    pub trailing_allowance_tracks_reference: bool,
    pub calendar: TradingCalendar,
    pub streak_backoff: StreakBackoff,
    pub drawdown_backoff: DrawdownBackoff,
    pub execution: ExecutionPolicy,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawdownReference {
    StaticInitialBalance,
    TrailingBalance,
    TrailingEquity,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DailyReference {
    Balance,
    Equity,
    HigherOfBalanceAndEquity,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreakBackoff {
    pub enabled: bool,
    pub reset_on_win: bool,
    pub reset_on_break_even: bool,
    pub levels: Vec<(u32, Decimal)>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DrawdownBackoff {
    pub enabled: bool,
    pub levels: Vec<(Decimal, Decimal)>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub slippage_buffer_pct_of_risk: Decimal,
    pub max_data_age_seconds: i64,
    /// Conservative surcharge for correlated/overlapping exposure. No netting credit.
    pub overlap_multiplier: Decimal,
}
impl Default for RiskPolicy {
    fn default() -> Self {
        Self {
            version: "1".into(),
            base_risk_pct: Decimal::ONE,
            max_account_loss_pct: Decimal::from(6),
            internal_account_stop_pct: Decimal::from(5),
            daily_loss_limit_pct: Decimal::from(3),
            internal_daily_stop_pct: Decimal::from(2),
            safety_buffer_pct: Decimal::new(25, 2),
            drawdown_reference: DrawdownReference::StaticInitialBalance,
            daily_reference: DailyReference::Balance,
            trailing_allowance_tracks_reference: false,
            calendar: TradingCalendar::default(),
            streak_backoff: StreakBackoff::default(),
            drawdown_backoff: DrawdownBackoff::default(),
            execution: ExecutionPolicy::default(),
        }
    }
}
impl Default for StreakBackoff {
    fn default() -> Self {
        Self {
            enabled: true,
            reset_on_win: true,
            reset_on_break_even: false,
            levels: vec![
                (0, Decimal::ONE),
                (1, Decimal::new(75, 2)),
                (2, Decimal::new(50, 2)),
                (3, Decimal::new(25, 2)),
                (4, Decimal::new(10, 2)),
            ],
        }
    }
}
impl Default for DrawdownBackoff {
    fn default() -> Self {
        Self {
            enabled: true,
            levels: vec![
                (Decimal::ZERO, Decimal::ONE),
                (Decimal::new(25, 1), Decimal::new(50, 2)),
                (Decimal::new(35, 1), Decimal::new(25, 2)),
                (Decimal::new(45, 1), Decimal::new(10, 2)),
                (Decimal::from(5), Decimal::ZERO),
            ],
        }
    }
}
impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            slippage_buffer_pct_of_risk: Decimal::from(20),
            max_data_age_seconds: 5,
            overlap_multiplier: Decimal::ONE,
        }
    }
}
impl RiskPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.version.is_empty(), "policy version required");
        ensure!(
            self.execution.max_data_age_seconds > 0,
            "data age must be positive"
        );
        ensure!(
            self.execution.slippage_buffer_pct_of_risk >= Decimal::ZERO
                && self.execution.slippage_buffer_pct_of_risk <= Decimal::from(1000),
            "invalid slippage buffer"
        );
        ensure!(
            self.execution.overlap_multiplier >= Decimal::ONE
                && self.execution.overlap_multiplier <= Decimal::from(100),
            "overlap multiplier must be 1..100"
        );
        ensure!(
            self.base_risk_pct > Decimal::ZERO && self.base_risk_pct <= Decimal::from(100),
            "invalid base risk"
        );
        ensure!(
            self.safety_buffer_pct >= Decimal::ZERO,
            "negative safety buffer"
        );
        for (internal, firm) in [
            (self.internal_daily_stop_pct, self.daily_loss_limit_pct),
            (self.internal_account_stop_pct, self.max_account_loss_pct),
        ] {
            ensure!(
                internal > Decimal::ZERO
                    && firm <= Decimal::from(100)
                    && internal
                        .checked_add(self.safety_buffer_pct)
                        .is_some_and(|sum| sum < firm),
                "internal stop plus buffer must be below firm limit"
            );
        }
        let s = &self.streak_backoff.levels;
        ensure!(
            s.first()
                .is_some_and(|(n, m)| *n == 0 && *m <= Decimal::ONE),
            "streak schedule must start at zero"
        );
        ensure!(
            s.iter()
                .all(|(_, m)| *m >= Decimal::ZERO && *m <= Decimal::ONE)
                && s.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 >= w[1].1),
            "streak schedule must tighten"
        );
        let d = &self.drawdown_backoff.levels;
        ensure!(
            d.first().is_some_and(|(n, _)| n.is_zero()),
            "drawdown schedule must start at zero"
        );
        ensure!(
            d.iter().all(|(n, m)| *n >= Decimal::ZERO
                && *n <= Decimal::from(100)
                && *m >= Decimal::ZERO
                && *m <= self.base_risk_pct)
                && d.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 >= w[1].1),
            "drawdown schedule must tighten"
        );
        self.calendar.validate()
    }
    pub fn streak_multiplier(&self, losses: u32) -> Decimal {
        if !self.streak_backoff.enabled {
            return Decimal::ONE;
        }
        self.streak_backoff
            .levels
            .iter()
            .rev()
            .find(|(n, _)| losses >= *n)
            .map_or(Decimal::ZERO, |(_, m)| *m)
    }
    pub fn drawdown_risk_pct(&self, drawdown: Decimal) -> Decimal {
        if !self.drawdown_backoff.enabled {
            return self.base_risk_pct;
        }
        self.drawdown_backoff
            .levels
            .iter()
            .rev()
            .find(|(n, _)| drawdown >= *n)
            .map_or(Decimal::ZERO, |(_, m)| *m)
    }
}
