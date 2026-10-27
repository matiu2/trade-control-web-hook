//! Explicit, bounded exclusions for statistical calibration; runtime feeds stay strict.
use super::{Identity, Report};
use chrono::{DateTime, Duration, Utc};
use color_eyre::{Result, eyre::ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) mod audit;
pub(crate) mod collect;
mod persistence;
mod validation;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub max_minutes_per_instrument: u32,
}
impl Policy {
    pub fn new(max_minutes_per_instrument: u32) -> Result<Self> {
        ensure!(
            (1..=5).contains(&max_minutes_per_instrument),
            "crossed-minute quarantine limit must be between one and five"
        );
        Ok(Self {
            max_minutes_per_instrument,
        })
    }
    pub fn validate(&self) -> Result<()> {
        Self::new(self.max_minutes_per_instrument).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Minute {
    pub symbol: String,
    pub symbol_id: i64,
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    pub observed_at_utc: DateTime<Utc>,
    pub bid: f64,
    pub ask: f64,
    pub reason: Reason,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    CrossedHistoricalBook,
}

impl Minute {
    pub(crate) fn from_error(
        symbol: &str,
        symbol_id: i64,
        error: &ctrader_data_source::CrossedHistoricalQuote,
    ) -> Result<Self> {
        let observed_at_utc = DateTime::from_timestamp_millis(error.time_ms)
            .ok_or_else(|| color_eyre::eyre::eyre!("crossed quote has invalid timestamp"))?;
        let from_utc =
            DateTime::from_timestamp_millis(error.time_ms.div_euclid(60_000) * 60_000)
                .ok_or_else(|| color_eyre::eyre::eyre!("crossed minute timestamp overflow"))?;
        let to_utc = from_utc
            .checked_add_signed(Duration::minutes(1))
            .ok_or_else(|| color_eyre::eyre::eyre!("crossed minute end overflow"))?;
        let minute = Self {
            symbol: symbol.into(),
            symbol_id,
            from_utc,
            to_utc,
            observed_at_utc,
            bid: error.bid,
            ask: error.ask,
            reason: Reason::CrossedHistoricalBook,
        };
        minute.validate()?;
        Ok(minute)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.symbol.is_empty() && self.symbol_id > 0,
            "quarantine has invalid instrument identity"
        );
        ensure!(
            self.from_utc.timestamp().rem_euclid(60) == 0
                && self.from_utc.timestamp_subsec_nanos() == 0
                && self.to_utc - self.from_utc == Duration::minutes(1)
                && self.observed_at_utc >= self.from_utc
                && self.observed_at_utc < self.to_utc,
            "quarantine must identify one whole M1 candle containing the bad observation"
        );
        ensure!(
            self.bid.is_finite() && self.ask.is_finite() && self.bid > self.ask && self.ask > 0.0,
            "only a finite, positive crossed book is eligible for quarantine"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvidence {
    pub path: PathBuf,
    pub sha256: String,
    pub excluded_minutes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub version: u32,
    pub identity: Identity,
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    pub requested_symbols: Vec<String>,
    pub policy: Policy,
}
impl Binding {
    pub(crate) fn from_report(report: &Report) -> Result<Self> {
        let policy = report.quarantine_policy.clone().ok_or_else(|| {
            color_eyre::eyre::eyre!("quarantine policy was not explicitly enabled")
        })?;
        let binding = Self {
            version: 1,
            identity: report.identity.clone(),
            from_utc: report.from_utc,
            to_utc: report.to_utc,
            requested_symbols: report.requested_symbols.clone(),
            policy,
        };
        binding.validate()?;
        Ok(binding)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.identity.validate_demo()?;
        self.policy.validate()?;
        ensure!(
            self.version == 1
                && self.to_utc - self.from_utc >= Duration::days(90)
                && self.to_utc - self.from_utc <= Duration::days(366),
            "quarantine requires an explicit 90–366 day sampling window"
        );
        ensure!(
            self.from_utc.timestamp().rem_euclid(60) == 0
                && self.to_utc.timestamp().rem_euclid(60) == 0
                && self.from_utc.timestamp_subsec_nanos() == 0
                && self.to_utc.timestamp_subsec_nanos() == 0,
            "quarantine window must contain whole closed minutes"
        );
        let symbols = self
            .requested_symbols
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(
            !symbols.is_empty()
                && symbols.iter().all(|s| !s.is_empty())
                && symbols.len() == self.requested_symbols.len(),
            "quarantine audit has invalid instrument scope"
        );
        Ok(())
    }
}

pub fn validate_report(report: &Report, verify_audit: bool) -> Result<()> {
    validation::validate_report(report, verify_audit)
}

#[cfg(test)]
mod tests;
