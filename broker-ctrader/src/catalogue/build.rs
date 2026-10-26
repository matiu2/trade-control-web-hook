//! Preserve existing underlying clocks; construct only explicitly audited gaps.
use super::{facts::Record, *};
use color_eyre::eyre::ensure;
use instrument_lookup::{AssetClass, AssetSymbols};

pub(super) fn assets(records: &[Record]) -> Result<BTreeMap<String, Asset>> {
    records
        .iter()
        .map(|row| {
            let asset = match &row.canonical {
                Some(id) => known_asset(row, id)?,
                None => native_asset(row)?,
            };
            ensure!(
                asset.class == row.class
                    && asset.spread_schedule == row.calibration_schedule
                    && asset.spread_schedule_tz().is_some()
                    && asset.news_currencies.contains(&row.quote_asset)
                    && asset.futures.is_none(),
                "audited cTrader underlying class/clock/currency changed for {}",
                row.symbol
            );
            Ok((row.symbol.clone(), asset))
        })
        .collect()
}

fn known_asset(row: &Record, id: &str) -> Result<Asset> {
    instrument_lookup::by_id(id)?
        .cloned()
        .ok_or_else(|| eyre!("audited underlying {id:?} for {} is missing", row.symbol))
}

fn native_asset(row: &Record) -> Result<Asset> {
    let (id, news_currencies) = match row.class {
        AssetClass::Forex => {
            ensure!(
                row.source_class == "Forex"
                    && row.native_timezone == "America/New_York"
                    && row.calibration_schedule == "ny",
                "native FX requires verified currency legs and NY learning-clock evidence"
            );
            (
                row.symbol.clone(),
                vec![row.base_asset.clone(), row.quote_asset.clone()],
            )
        }
        AssetClass::Crypto => {
            ensure!(
                row.source_class == "Cryptocurrency" && row.calibration_schedule == "ny",
                "native crypto requires verified coin/quote identity and its existing learning-clock convention"
            );
            (
                row.symbol.clone(),
                vec![row.measurement_units.clone(), row.quote_asset.clone()],
            )
        }
        AssetClass::Index => {
            ensure!(
                row.symbol == "ITALY 40"
                    && row.source_class == "Indices"
                    && row.native_timezone == "Europe/Berlin"
                    && row.calibration_schedule == "frankfurt",
                "native index has no audited underlying and learning clock"
            );
            ("ITALY40".into(), vec![row.quote_asset.clone()])
        }
        _ => {
            return Err(eyre!(
                "no audited native-only underlying policy for {}",
                row.symbol
            ));
        }
    };
    Ok(Asset {
        id,
        class: row.class,
        display_name: row.description.clone(),
        description: row.description.clone(),
        news_currencies,
        tick_size: 10_f64.powi(-(row.digits as i32)),
        decimal_places: u8::try_from(row.digits)?,
        pip_size: 10_f64.powi(-row.pip_position),
        // This clock buckets measured spread observations. Native broker
        // trading sessions stay separate and are never invented from it.
        spread_schedule: row.calibration_schedule.clone(),
        futures: None,
        symbols: AssetSymbols {
            oanda: None,
            tradenation: None,
            tradingview: None,
            ibkr: None,
        },
    })
}
