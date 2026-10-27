//! Account-scoped stock metadata for read-only history/calibration.
//! Native pip/lot facts are evidence, never permission or a sizing model.
use super::*;
use color_eyre::eyre::ensure;
use instrument_lookup::{AssetClass, AssetSymbols};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

mod native;
pub use native::NativeStockMetadata;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockAssetIdentity {
    #[serde(alias = "assetId")]
    pub id: i64,
    pub name: String,
    #[serde(alias = "displayName")]
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockClass {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockCategory {
    pub id: i64,
    pub name: String,
    #[serde(alias = "assetClassId")]
    pub asset_class_id: i64,
}

/// Seconds since Sunday 00:00 in the separately declared schedule timezone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockWeeklyInterval {
    #[serde(alias = "startSecond")]
    pub start_second: u32,
    #[serde(alias = "endSecond")]
    pub end_second: u32,
}

/// Native evidence; neither enabled nor trading_mode authorizes stock orders.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockMetadata {
    pub name: String,
    pub id: i64,
    pub canonical: Option<String>,
    pub description: String,
    pub enabled: bool,
    pub asset_class: StockClass,
    pub category: StockCategory,
    pub base_asset: StockAssetIdentity,
    pub quote_asset: StockAssetIdentity,
    pub digits: u32,
    pub pip_position: i32,
    pub min_volume: i64,
    pub step_volume: i64,
    pub max_volume: i64,
    pub lot_size: i64,
    pub measurement_units: String,
    pub trading_mode: i32,
    pub schedule_timezone: String,
    pub schedule: Vec<StockWeeklyInterval>,
    pub holidays: Vec<Value>,
}

#[derive(Deserialize)]
struct Snapshot {
    source_profile: String,
    source_account_id: i64,
    source_environment: String,
    source_broker_name: String,
    source_trader_login: i64,
    source_endpoint: String,
    verified_at: String,
    learning_clock: String,
    records: Vec<StockMetadata>,
}

struct Catalogue {
    snapshot: Snapshot,
    assets: BTreeMap<String, Asset>,
}

static CATALOGUE: LazyLock<Result<Catalogue>> = LazyLock::new(|| {
    let snapshot: Snapshot = serde_json::from_str(include_str!("stocks.json"))?;
    validate_snapshot(&snapshot)?;
    let assets = snapshot.records.iter().map(asset).collect::<Result<_>>()?;
    Ok(Catalogue { snapshot, assets })
});

fn catalogue() -> Result<&'static Catalogue> {
    CATALOGUE.as_ref().map_err(|error| eyre!("{error:#}"))
}

/// Exact native names from the audited ctrader-demo stock/ETF snapshot.
pub fn stock_symbols() -> Result<Vec<&'static str>> {
    Ok(catalogue()?.assets.keys().map(String::as_str).collect())
}

/// Audited metadata only. Before history/replay use, call verify_stock_metadata
/// with freshly read account and symbol evidence. This never permits orders.
pub fn stock_asset(symbol: &str) -> Result<&'static Asset> {
    catalogue()?
        .assets
        .get(symbol)
        .ok_or_else(|| eyre!("no audited native cTrader stock {symbol:?}"))
}

pub fn stock_metadata(symbol: &str) -> Result<&'static StockMetadata> {
    catalogue()?
        .snapshot
        .records
        .iter()
        .find(|row| row.name == symbol)
        .ok_or_else(|| eyre!("no audited native cTrader stock {symbol:?}"))
}

/// Compare fresh native evidence against this specific demo broker account.
/// A matching global ticker or another broker's symbol ID is insufficient.
/// Trading mode is verified, but even fully enabled stocks remain read-only.
pub fn verify_stock_metadata(
    profile: &str,
    identity: &crate::market_data::AccountIdentity,
    current: &StockMetadata,
) -> Result<&'static Asset> {
    let expected = &catalogue()?.snapshot;
    ensure!(
        profile == expected.source_profile
            && identity.account_id == expected.source_account_id
            && identity.environment == ctrader_api::Environment::Demo
            && identity.endpoint == expected.source_endpoint
            && identity.broker_name == expected.source_broker_name
            && identity.trader_login == expected.source_trader_login,
        "stock metadata belongs to another cTrader broker/account; audit that account separately"
    );
    ensure!(
        current == stock_metadata(&current.name)?,
        "native cTrader stock identity/precision/session evidence changed for {:?}; re-audit before reading history",
        current.name
    );
    stock_asset(&current.name)
}

fn asset(row: &StockMetadata) -> Result<(String, Asset)> {
    let asset = Asset {
        id: row.name.clone(),
        class: AssetClass::Stock,
        display_name: row.description.clone(),
        description: row.description.clone(),
        news_currencies: vec![row.quote_asset.name.clone()],
        tick_size: 10_f64.powi(-(row.digits as i32)),
        decimal_places: u8::try_from(row.digits)?,
        pip_size: 10_f64.powi(-row.pip_position),
        spread_schedule: "utc".into(),
        futures: None,
        symbols: AssetSymbols {
            oanda: None,
            tradenation: None,
            tradingview: None,
            ibkr: None,
        },
    };
    ensure!(
        asset.spread_schedule_tz().as_deref() == Some("UTC"),
        "missing explicit UTC learning clock"
    );
    Ok((row.name.clone(), asset))
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    ensure!(
        snapshot.source_profile == "ctrader-demo"
            && snapshot.source_account_id == 49247794
            && snapshot.source_environment == "demo"
            && snapshot.source_broker_name == "Spotware"
            && snapshot.source_trader_login == 5944473
            && snapshot.source_endpoint == ctrader_api::Environment::Demo.endpoint()
            && chrono::DateTime::parse_from_rfc3339(&snapshot.verified_at).is_ok()
            && snapshot.learning_clock == "utc"
            && snapshot.records.len() == 350,
        "invalid audited cTrader stock provenance"
    );
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for row in &snapshot.records {
        validate_record(row)?;
        ensure!(
            names.insert(&row.name) && ids.insert(row.id),
            "duplicate native stock identity"
        );
    }
    Ok(())
}

fn validate_record(row: &StockMetadata) -> Result<()> {
    ensure!(
        !row.name.is_empty()
            && row.name.is_ascii()
            && !row.name.chars().any(char::is_control)
            && !row.description.trim().is_empty()
            && row.id > 0
            && row.canonical.is_none()
            && row.enabled
            && row.asset_class.id > 0
            && row.category.id > 0
            && row.category.asset_class_id == row.asset_class.id
            && matches!(
                row.asset_class.name.as_str(),
                "Asia/Pacific Shares" | "EU Shares" | "US Shares" | "iShares ETFs"
            )
            && !row.category.name.is_empty()
            && row.base_asset.id > 0
            && !row.base_asset.name.is_empty()
            && !row.base_asset.display_name.is_empty()
            && row.quote_asset.id > 0
            && crate::metadata::currency(&row.quote_asset.name)
            && row.quote_asset.name == row.quote_asset.display_name
            && (1..=3).contains(&row.digits)
            && (0..=2).contains(&row.pip_position)
            && row.min_volume > 0
            && row.step_volume > 0
            && row.max_volume >= row.min_volume
            && row.lot_size == 100
            && row.measurement_units == "Shares"
            && matches!(row.trading_mode, 0 | 3)
            && row.schedule_timezone == "UTC"
            && !row.schedule.is_empty()
            && row.holidays.is_empty(),
        "incomplete audited native stock evidence for {:?}",
        row.name
    );
    ensure!(
        row.schedule
            .iter()
            .all(|s| s.start_second < s.end_second && s.end_second <= 604800)
            && row
                .schedule
                .windows(2)
                .all(|s| s[0].end_second <= s[1].start_second),
        "invalid native weekly stock sessions for {:?}",
        row.name
    );
    Ok(())
}
