//! Join fresh account-native metadata without consulting global ticker aliases.
use super::*;
use serde_json::json;

/// Six unmodified native objects from the selected account's metadata reads.
/// Read symbol light/full, assets, categories and classes in the current session.
pub struct NativeStockMetadata<'a> {
    pub light: &'a Value,
    pub full_symbol: &'a Value,
    pub base_asset: &'a Value,
    pub quote_asset: &'a Value,
    pub category: &'a Value,
    pub asset_class: &'a Value,
}

impl StockMetadata {
    pub fn from_native(native: NativeStockMetadata<'_>) -> Result<Self> {
        let light = native.light;
        let full = native.full_symbol;
        let id = crate::wire::integer(light, "symbolId")?;
        ensure!(
            id == crate::wire::integer(full, "symbolId")?
                && crate::wire::integer(light, "baseAssetId")?
                    == crate::wire::integer(native.base_asset, "assetId")?
                && crate::wire::integer(light, "quoteAssetId")?
                    == crate::wire::integer(native.quote_asset, "assetId")?
                && crate::wire::integer(light, "symbolCategoryId")?
                    == crate::wire::integer(native.category, "id")?
                && crate::wire::integer(native.category, "assetClassId")?
                    == crate::wire::integer(native.asset_class, "id")?,
            "native stock metadata joins disagree"
        );
        let current: Self = serde_json::from_value(json!({
            "name": light["symbolName"], "id": id, "canonical": null,
            "description": light["description"], "enabled": light["enabled"],
            "asset_class": native.asset_class, "category": native.category,
            "base_asset": native.base_asset, "quote_asset": native.quote_asset,
            "digits": full["digits"], "pip_position": full["pipPosition"],
            "min_volume": full["minVolume"], "step_volume": full["stepVolume"],
            "max_volume": full["maxVolume"], "lot_size": full["lotSize"],
            "measurement_units": full["measurementUnits"], "trading_mode": full["tradingMode"],
            "schedule_timezone": full["scheduleTimeZone"], "schedule": full["schedule"],
            "holidays": full.get("holiday").cloned().unwrap_or_else(|| json!([])),
        }))?;
        validate_record(&current)?;
        Ok(current)
    }
}
