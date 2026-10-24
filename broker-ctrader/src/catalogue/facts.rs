//! Sanitized native identity/session evidence; no authorization material.
use super::*;
use instrument_lookup::AssetClass;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Snapshot {
    pub source_profile: String,
    pub source_account_id: i64,
    pub source_environment: String,
    pub verified_at: String,
    pub records: Vec<Record>,
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Record {
    pub symbol: String,
    pub canonical: Option<String>,
    pub class: AssetClass,
    pub source_class: String,
    pub source_id: i64,
    pub description: String,
    pub base_asset: String,
    pub base_display: String,
    pub quote_asset: String,
    pub digits: u32,
    pub pip_position: i32,
    pub min_volume: i64,
    pub step_volume: i64,
    pub max_volume: i64,
    pub measurement_units: String,
    pub native_timezone: String,
    pub calibration_schedule: String,
}

pub(super) fn snapshot() -> Result<Snapshot> {
    Ok(serde_json::from_str(include_str!("nonstocks.json"))?)
}

pub(super) fn validate(snapshot: &Snapshot) -> Result<()> {
    color_eyre::eyre::ensure!(
        !snapshot.source_profile.trim().is_empty()
            && snapshot.source_account_id > 0
            && snapshot.source_environment == "demo"
            && chrono::NaiveDate::parse_from_str(&snapshot.verified_at, "%Y-%m-%d").is_ok(),
        "invalid cTrader catalogue evidence provenance"
    );
    color_eyre::eyre::ensure!(
        !snapshot.records.is_empty(),
        "empty native cTrader catalogue"
    );
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for row in &snapshot.records {
        validate_record(row)?;
        color_eyre::eyre::ensure!(
            names.insert(row.symbol.to_ascii_uppercase()) && ids.insert(row.source_id),
            "duplicate or ambiguous native cTrader catalogue identity"
        );
    }
    Ok(())
}

fn validate_record(row: &Record) -> Result<()> {
    color_eyre::eyre::ensure!(
        !row.symbol.is_empty()
            && row.symbol.is_ascii()
            && !row.symbol.chars().any(char::is_control)
            && row.source_id > 0
            && row.digits <= 8
            && (0..=8).contains(&row.pip_position)
            && row.min_volume > 0
            && row.step_volume > 0
            && row.max_volume >= row.min_volume
            && row.max_volume >= row.step_volume
            && !row.native_timezone.trim().is_empty()
            && !row.calibration_schedule.trim().is_empty()
            && !row.base_display.trim().is_empty()
            && !row.description.trim().is_empty()
            && crate::metadata::currency(&row.quote_asset),
        "incomplete cTrader native identity/precision/session evidence for {}",
        row.symbol
    );
    let matches_class = match row.source_class.as_str() {
        "Forex" => {
            row.class == AssetClass::Forex
                && crate::metadata::currency(&row.base_asset)
                && row.symbol == format!("{}{}", row.base_asset, row.quote_asset)
        }
        "Indices" => row.class == AssetClass::Index,
        "Metals" => matches!(row.class, AssetClass::Gold | AssetClass::Commodity),
        "Energies" => row.class == AssetClass::Commodity,
        "Cryptocurrency" => {
            row.class == AssetClass::Crypto
                && matches!(row.measurement_units.as_str(), "BTC" | "ETH")
                && row.base_display == row.measurement_units
                && row.symbol == format!("{}{}", row.measurement_units, row.quote_asset)
        }
        _ => false,
    };
    color_eyre::eyre::ensure!(
        matches_class,
        "unverified native cTrader asset class/currency identity for {}",
        row.symbol
    );
    Ok(())
}
