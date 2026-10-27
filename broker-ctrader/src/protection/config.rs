use super::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropAccount {
    pub enabled: bool,
    pub account_id: i64,
    pub environment: ctrader_api::Environment,
    pub state_directory: PathBuf,
    /// Verified flat-account enrollment instant, before its first protected execution.
    pub history_from: DateTime<Utc>,
    pub initial_balance: Decimal,
    pub rulebook_verified: bool,
    pub fees_verified: bool,
    pub fee_mode: FeeMode,
    pub commission_is_positive_cost: bool,
    pub risk: RiskPolicy,
    pub costs: BTreeMap<String, Costs>,
    /// Required for daily equity references and continuous trailing equity peaks.
    pub firm_evidence_file: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeMode {
    PerDeal,
    CloseDetailIncludesOpening,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Costs {
    pub opening_per_unit: Decimal,
    pub closing_per_unit: Decimal,
    /// Conservative remaining financing/other charge estimate in account currency.
    pub future_swap_per_unit: Decimal,
    pub fixed_per_order: Decimal,
    pub overlapping: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmEvidence {
    pub account_id: i64,
    pub environment: ctrader_api::Environment,
    pub day: adaptive_risk::DayReference,
    pub continuous_equity_high: Decimal,
    pub policy_version: String,
    /// Requests proven terminal/absent by the authoritative broker reconciliation source.
    #[serde(default)]
    pub resolved_requests: std::collections::BTreeSet<String>,
    pub observed_at: DateTime<Utc>,
    pub account_stop_breached: bool,
    pub daily_stop_breached: bool,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Accounts {
    accounts: BTreeMap<String, PropAccount>,
}
pub(crate) fn load(
    name: &str,
    account_id: i64,
    environment: ctrader_api::Environment,
) -> Result<Option<PropAccount>> {
    let path = std::env::var_os("PROP_RISK_ACCOUNTS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config/trade-control/prop-risk-accounts.toml")
        });
    let explicitly_configured = std::env::var_os("PROP_RISK_ACCOUNTS_FILE").is_some();
    let document = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicitly_configured => {
            super::binding::check(None, account_id, environment)?;
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let profile = parse(&document, name, account_id, environment)?;
    super::binding::check(profile.as_ref(), account_id, environment)?;
    Ok(profile)
}
pub(super) fn parse(
    document: &str,
    name: &str,
    account_id: i64,
    environment: ctrader_api::Environment,
) -> Result<Option<PropAccount>> {
    ensure!(document.len() <= 1_048_576, "prop risk profiles too large");
    let profiles: Accounts = toml::from_str(document)?;
    let mut identities = std::collections::BTreeSet::new();
    let mut directories = std::collections::BTreeSet::new();
    for profile in profiles.accounts.values().filter(|p| p.enabled) {
        ensure!(
            identities.insert(profile.identity()) && directories.insert(&profile.state_directory),
            "each prop account must have exactly one profile and durable state directory"
        );
    }
    if let Some(named) = profiles.accounts.get(name).filter(|p| p.enabled) {
        ensure!(
            named.account_id == account_id && named.environment == environment,
            "named prop profile identifies another physical account"
        );
    }
    Ok(profiles
        .accounts
        .into_values()
        .find(|p| p.enabled && p.account_id == account_id && p.environment == environment))
}
impl PropAccount {
    pub fn identity(&self) -> String {
        format!("ctrader:{:?}:{}", self.environment, self.account_id)
    }
    pub fn validate(&self) -> Result<()> {
        self.risk.validate()?;
        ensure!(
            self.account_id > 0
                && self.initial_balance > Decimal::ZERO
                && self.rulebook_verified
                && self.fees_verified,
            "verify purchased program rules and broker fee treatment before enabling prop risk"
        );
        ensure!(
            self.history_from.timestamp_millis() >= 0 && self.state_directory.is_absolute(),
            "invalid risk history start or state directory"
        );
        ensure!(
            self.costs.values().all(|c| [
                c.opening_per_unit,
                c.closing_per_unit,
                c.future_swap_per_unit,
                c.fixed_per_order
            ]
            .iter()
            .all(|n| *n >= Decimal::ZERO)),
            "negative cost estimate"
        );
        Ok(())
    }
    pub fn manager(&self) -> Result<RiskManager> {
        self.validate()?;
        RiskManager::new(
            adaptive_risk::store::FileStore::new(&self.state_directory),
            self.identity(),
            self.risk.clone(),
        )
    }
    /// Explicit offline enrollment, using verified initial/boundary references.
    /// This never connects to a broker or submits an order.
    pub fn enroll(&self, day: adaptive_risk::DayReference) -> Result<()> {
        self.validate()?;
        ensure!(
            day.day == self.risk.calendar.day_at(self.history_from)?,
            "enrollment day mismatch"
        );
        super::binding::check(Some(self), self.account_id, self.environment)?;
        let state = adaptive_risk::RiskState::enroll(
            self.identity(),
            self.initial_balance,
            day,
            self.risk.clone(),
        )?;
        adaptive_risk::store::FileStore::new(&self.state_directory).enroll(&state)
    }
}
