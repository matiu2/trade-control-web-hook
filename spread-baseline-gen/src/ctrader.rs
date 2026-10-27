//! Account-pinned calibration from genuine paired cTrader ticks through candle-cache.
use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::ensure};
use serde::{Deserialize, Serialize};

pub mod bake;
pub mod calibration;
pub mod checkpoint;
pub mod generate;
pub mod output;
pub mod quarantine;

/// Non-secret, broker-verified account identity. CLI and FIX credentials cannot substitute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub profile: String,
    pub environment: String,
    pub endpoint: String,
    pub account_id: i64,
    pub broker: String,
    pub login: i64,
}
impl Identity {
    pub fn validate_demo(&self) -> Result<()> {
        ensure!(
            !self.profile.is_empty()
                && self
                    .profile
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "invalid cTrader profile name"
        );
        ensure!(
            self.environment == "demo"
                && self.endpoint == ctrader_api::Environment::Demo.endpoint(),
            "spread calibration currently permits demo accounts only"
        );
        ensure!(
            self.account_id > 0
                && self.login > 0
                && !self.broker.trim().is_empty()
                && !self.broker.chars().any(char::is_control),
            "invalid verified cTrader account identity"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub identity: Identity,
    pub source: String,
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    pub requested_symbols: Vec<String>,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_policy: Option<quarantine::Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_audit: Option<quarantine::AuditEvidence>,
    /// Catalogue instruments intentionally outside the supported FX universe.
    #[serde(default)]
    pub excluded: Vec<crate::mt5::Failure>,
    pub instruments: Vec<InstrumentReport>,
    pub failures: Vec<crate::mt5::Failure>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct InstrumentReport {
    pub symbol_id: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quarantined_minutes: Vec<quarantine::Minute>,
    #[serde(flatten)]
    pub measurement: crate::mt5::InstrumentReport,
}

/// Only known instrument-lookup symbols supply schedule metadata; no guessed aliases.
pub fn asset(symbol: &str) -> Result<&'static instrument_lookup::Asset> {
    instrument_lookup::resolve(symbol)?.ok_or_else(|| {
        color_eyre::eyre::eyre!("no instrument-lookup schedule/pip metadata for cTrader {symbol}")
    })
}

#[cfg(test)]
fn test_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
