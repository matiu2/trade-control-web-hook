use crate::Decimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dataset {
    Replay,
    ForwardTest,
    Live,
    SyntheticProvisional,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TradeRecord {
    pub at: DateTime<Utc>,
    /// Net realised P&L / original nominal entry-to-stop risk, including realised
    /// fees/slippage in the numerator; unspent slippage reservations are excluded.
    pub net_r: Decimal,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Strategy {
    Fixed,
    StreakOnly,
    DrawdownOnly,
    Combined,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReplayReport {
    pub net_return_pct: Decimal,
    pub max_drawdown_pct: Decimal,
    pub longest_losing_streak: u32,
    pub skipped_trades: usize,
    pub average_risk_pct: Decimal,
    pub average_reserved_risk_pct: Decimal,
    pub daily_lockouts: u64,
    pub account_limit_breaches: u64,
    pub daily_firm_limit_breaches: u64,
    pub trades_to_target: Option<usize>,
    pub funded: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct StrategyReport {
    pub strategy: Strategy,
    pub dataset: Dataset,
    pub report: ReplayReport,
}
