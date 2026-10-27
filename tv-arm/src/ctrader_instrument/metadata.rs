//! Validate the selected broker's native precision and volume bounds.
use super::*;
use crate::precision::CatalogPrecision;
use broker_ctrader::InstrumentMetadata;
use instrument_lookup::Asset;

pub(super) struct NativeMetadata {
    pub name: String,
    pub id: i64,
    pub digits: u32,
    pub pip_size: f64,
    pub min_units: f64,
    pub step_units: f64,
    pub max_units: f64,
}

impl From<&InstrumentMetadata> for NativeMetadata {
    fn from(value: &InstrumentMetadata) -> Self {
        Self {
            name: value.name.clone(),
            id: value.id,
            digits: value.digits,
            pip_size: value.pip_size,
            min_units: value.min_units,
            step_units: value.step_units,
            max_units: value.max_units,
        }
    }
}

pub(super) fn resolve(
    asset: &'static Asset,
    symbol: &str,
    native: &NativeMetadata,
) -> Result<ResolvedInstrument> {
    ensure!(
        native.name == symbol && native.id > 0,
        "cTrader symbol identity does not match {symbol}"
    );
    ensure!(native.digits <= 8, "unsupported cTrader precision");
    ensure!(
        native.pip_size.is_finite() && native.pip_size > 0.0,
        "invalid cTrader pip size"
    );
    ensure!(
        [native.min_units, native.step_units, native.max_units]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
            && native.max_units >= native.min_units
            && native.step_units <= native.max_units,
        "invalid cTrader native volume limits"
    );
    let decimal_places = u8::try_from(native.digits)?;
    Ok(ResolvedInstrument {
        asset,
        broker_symbol: native.name.clone(),
        precision: CatalogPrecision {
            tick_size: 10_f64.powi(-i32::from(decimal_places)),
            pip_size: native.pip_size,
            decimal_places,
            class: asset.class,
            // Native volume bounds are not a futures multiplier or a proof of
            // executable sizing. Replay reports price/R and no broker volume.
            contract_multiplier: None,
        },
    })
}
