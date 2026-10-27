use adaptive_risk::Decimal;
use chrono::{DateTime, Utc};
use clap::Args;
use std::path::PathBuf;
#[derive(Args, Default, Clone)]
#[group(id = "PropRiskOptions")]
pub struct Options {
    /// Enroll this cTrader account in persistent prop-firm risk protection.
    #[arg(long)]
    pub prop_firm: bool,
    /// Verified firm policy TOML; individual flags override it.
    #[arg(long)]
    pub risk_policy_file: Option<PathBuf>,
    #[arg(long = "risk-base-pct")]
    pub base_risk_pct: Option<Decimal>,
    #[arg(long)]
    pub max_account_loss_pct: Option<Decimal>,
    #[arg(long)]
    pub internal_account_stop_pct: Option<Decimal>,
    #[arg(long)]
    pub daily_loss_limit_pct: Option<Decimal>,
    #[arg(long)]
    pub internal_daily_stop_pct: Option<Decimal>,
    #[arg(long)]
    pub safety_buffer_pct: Option<Decimal>,
    /// Local firm day boundary, e.g. 17:00. Default: 17:00 America/New_York.
    #[arg(long)]
    pub daily_reset: Option<String>,
    #[arg(long)]
    pub risk_timezone: Option<String>,
    #[arg(long,value_parser=["static-initial-balance","trailing-balance","trailing-equity"])]
    pub drawdown_reference: Option<String>,
    #[arg(long,value_parser=["balance","equity","higher-of-balance-and-equity"])]
    pub daily_reference: Option<String>,
    #[arg(long)]
    pub initial_balance: Option<Decimal>,
    /// Confirm the purchased account is flat, unused, and its day-start references equal initial balance.
    #[arg(long)]
    pub new_flat_account: bool,
    #[arg(long)]
    pub day_start_balance: Option<Decimal>,
    #[arg(long)]
    pub day_start_equity: Option<Decimal>,
    #[arg(long)]
    pub history_from: Option<DateTime<Utc>>,
    #[arg(long)]
    pub risk_state_directory: Option<PathBuf>,
    #[arg(long)]
    pub firm_evidence_file: Option<PathBuf>,
    /// Native symbol and ACCOUNT-CURRENCY costs: SYMBOL:OPEN:CLOSE:FUTURE_SWAP[:FIXED]. Repeat per symbol.
    #[arg(long = "risk-cost")]
    pub costs: Vec<String>,
    /// Complete instrument cost map TOML for reusable program settings.
    #[arg(long)]
    pub risk_costs_file: Option<PathBuf>,
    /// Loss count:multiplier pairs, e.g. 0:1,1:0.75,2:0.5,3:0.25,4:0.1.
    #[arg(long)]
    pub streak_levels: Option<String>,
    /// Drawdown percentage:risk percentage pairs, e.g. 0:1,2.5:0.5,3.5:0.25,4.5:0.1,5:0.
    #[arg(long)]
    pub drawdown_levels: Option<String>,
    #[arg(long)]
    pub no_streak_backoff: bool,
    #[arg(long)]
    pub no_drawdown_backoff: bool,
    #[arg(long)]
    pub slippage_buffer_pct_of_risk: Option<Decimal>,
    #[arg(long)]
    pub max_data_age_seconds: Option<i64>,
    /// Attest that the selected purchased program's loss rules/reset semantics have been verified.
    #[arg(long)]
    pub rules_verified: bool,
    /// Attest that cost estimates and broker commission conventions have been verified.
    #[arg(long)]
    pub fees_verified: bool,
    /// Closing-detail commission already includes opening charges (default: per-deal).
    #[arg(long)]
    pub close_detail_includes_opening: bool,
    /// Broker commission values are signed P&L contributions (default: positive costs).
    #[arg(long)]
    pub commission_is_signed: bool,
}
impl Options {
    pub fn is_present(&self) -> bool {
        self.prop_firm
            || self.risk_policy_file.is_some()
            || self.base_risk_pct.is_some()
            || self.max_account_loss_pct.is_some()
            || self.internal_account_stop_pct.is_some()
            || self.daily_loss_limit_pct.is_some()
            || self.internal_daily_stop_pct.is_some()
            || self.safety_buffer_pct.is_some()
            || self.daily_reset.is_some()
            || self.risk_timezone.is_some()
            || self.drawdown_reference.is_some()
            || self.daily_reference.is_some()
            || self.initial_balance.is_some()
            || self.new_flat_account
            || self.day_start_balance.is_some()
            || self.day_start_equity.is_some()
            || self.history_from.is_some()
            || self.risk_state_directory.is_some()
            || self.firm_evidence_file.is_some()
            || !self.costs.is_empty()
            || self.risk_costs_file.is_some()
            || self.streak_levels.is_some()
            || self.drawdown_levels.is_some()
            || self.no_streak_backoff
            || self.no_drawdown_backoff
            || self.slippage_buffer_pct_of_risk.is_some()
            || self.max_data_age_seconds.is_some()
            || self.rules_verified
            || self.fees_verified
            || self.close_detail_includes_opening
            || self.commission_is_signed
    }
}
