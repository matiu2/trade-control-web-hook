//! Read-only spread calibration has no order-sizing or lot-multiplier semantics.
use super::asset as catalog_asset;
use broker_ctrader::{
    InstrumentMetadata,
    market_data::{ChartData, InstrumentSummary},
};
use color_eyre::{Result, eyre::ensure};
use instrument_lookup::{Asset, AssetClass};

/// Keep legacy FX discovery; explicit selection uses the audited native non-stock catalogue.
pub fn asset(symbol: &str, explicit: bool) -> Result<&'static Asset> {
    let asset = if explicit {
        if broker_ctrader::catalogue::stock_symbols()?.contains(&symbol) {
            broker_ctrader::catalogue::stock_asset(symbol)?
        } else {
            broker_ctrader::catalogue::replay_asset(symbol)?
        }
    } else {
        catalog_asset(symbol)?
    };
    validate_asset(asset, explicit)?;
    Ok(asset)
}

/// Broad discovery is the non-stock rollout; explicit symbols remain deliberate.
pub fn discovery_asset(symbol: &str, all_instruments: bool) -> Result<&'static Asset> {
    ensure!(
        !broker_ctrader::catalogue::stock_symbols()?.contains(&symbol),
        "stock calibration requires explicit --stocks selection"
    );
    let legacy = instrument_lookup::resolve(symbol)?;
    ensure!(
        !all_instruments || legacy.is_none_or(|asset| asset.class != AssetClass::Stock),
        "stock calibration is excluded from the current non-stock rollout"
    );
    asset(symbol, all_instruments)
}

fn validate_asset(asset: &Asset, explicit: bool) -> Result<()> {
    ensure!(
        asset.class == AssetClass::Forex || explicit,
        "default cTrader discovery is FX-only; use --only or --all-instruments for other classes"
    );
    timezone(asset)?;
    Ok(())
}

/// A schedule is required by the runtime; never invent a market timezone.
pub fn timezone(asset: &Asset) -> Result<chrono_tz::Tz> {
    let timezone = asset.spread_schedule_tz().ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "{} has no resolvable spread schedule timezone; native schedule metadata is required before calibration",
            asset.id
        )
    })?;
    timezone.parse().map_err(|error| {
        color_eyre::eyre::eyre!("invalid spread schedule timezone for {}: {error}", asset.id)
    })
}

/// Preserve FX currency-leg validation; every other class needs exact enabled quote identity.
pub async fn metadata(
    data: &ChartData,
    symbol: &str,
    explicit: bool,
) -> Result<InstrumentMetadata> {
    if broker_ctrader::catalogue::stock_symbols()?.contains(&symbol) {
        ensure!(explicit, "stock calibration requires explicit selection");
        return data.verified_stock_metadata(symbol).await;
    }
    let asset = asset(symbol, explicit)?;
    let metadata = if asset.class == AssetClass::Forex {
        data.forex_instrument_metadata(symbol).await?
    } else {
        let expected_id = available_symbol(&data.catalogue().await?, symbol)?;
        let metadata = data.instrument_metadata(symbol).await?;
        ensure!(
            metadata.id == expected_id,
            "cTrader native symbol ID changed"
        );
        metadata
    };
    verify(
        symbol,
        &metadata.name,
        metadata.id,
        metadata.digits,
        metadata.pip_size,
    )?;
    Ok(metadata)
}

fn available_symbol(catalogue: &[InstrumentSummary], symbol: &str) -> Result<i64> {
    let matches = catalogue
        .iter()
        .filter(|row| row.name == symbol)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1 && matches[0].id > 0 && matches[0].enabled == Some(true),
        "cTrader native symbol is missing, disabled or ambiguous"
    );
    Ok(matches[0].id)
}

fn verify(symbol: &str, name: &str, id: i64, digits: u32, pip_size: f64) -> Result<()> {
    ensure!(
        name == symbol && id > 0 && digits <= 8 && pip_size.is_finite() && pip_size > 0.0,
        "invalid cTrader calibration symbol identity or native price precision"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalogued_class_can_be_explicitly_calibrated_with_a_known_schedule() {
        crate::ctrader::test_logging();
        let mut sample = catalog_asset("EURUSD").unwrap().clone();
        for class in [
            AssetClass::Forex,
            AssetClass::Index,
            AssetClass::Gold,
            AssetClass::Bond,
            AssetClass::Commodity,
            AssetClass::Crypto,
            AssetClass::Stock,
        ] {
            sample.class = class;
            validate_asset(&sample, true).unwrap();
            assert_eq!(
                validate_asset(&sample, false).is_ok(),
                class == AssetClass::Forex
            );
        }
        assert_eq!(asset("XAUEUR", true).unwrap().class, AssetClass::Gold);
        assert!(asset("XAUEUR", false).is_err());
        assert!(asset("EURUSD*t", true).is_err());
        assert!(asset("NO_SUCH_SYMBOL", true).is_err());
    }

    #[test]
    fn exact_stocks_use_native_metadata_and_utc_without_global_aliases() {
        crate::ctrader::test_logging();
        for name in broker_ctrader::catalogue::stock_symbols().unwrap() {
            let stock = asset(name, true).unwrap();
            assert_eq!(stock.id, name);
            assert_eq!(stock.class, AssetClass::Stock);
            assert_eq!(timezone(stock).unwrap(), chrono_tz::UTC);
            assert!(asset(name, false).is_err());
            assert!(discovery_asset(name, true).is_err());
        }
        let aia = asset("AIA", true).unwrap();
        assert_eq!(aia.news_currencies, ["HKD"]);
        assert!(aia.description.contains("AIA GROUP"));
    }

    #[test]
    fn absent_or_unknown_schedule_is_not_guessed_from_the_asset_class() {
        crate::ctrader::test_logging();
        let mut sample = catalog_asset("EURUSD").unwrap().clone();
        for schedule in ["none", "not-a-real-schedule"] {
            sample.spread_schedule = schedule.into();
            let error = validate_asset(&sample, true).unwrap_err().to_string();
            assert!(error.contains("schedule"), "{error}");
        }
    }

    #[test]
    fn broad_discovery_explicitly_excludes_stocks_for_this_rollout() {
        crate::ctrader::test_logging();
        let stock = instrument_lookup::all()
            .unwrap()
            .iter()
            .find(|asset| asset.class == AssetClass::Stock)
            .unwrap();
        let error = discovery_asset(&stock.id, true).unwrap_err().to_string();
        assert!(error.contains("stock calibration"), "{error}");
        assert!(discovery_asset("XAUEUR", true).is_ok());
        assert!(discovery_asset("XAUEUR", false).is_err());
    }

    #[test]
    fn broad_discovery_covers_every_audited_native_non_stock_without_forex_guesses() {
        crate::ctrader::test_logging();
        let symbols = broker_ctrader::catalogue::non_stock_symbols().unwrap();
        assert_eq!(symbols.len(), 93);
        for symbol in symbols {
            let asset = discovery_asset(symbol, true).unwrap();
            assert_ne!(asset.class, AssetClass::Stock);
            timezone(asset).unwrap();
        }
        assert_eq!(asset("NOKJPY", true).unwrap().pip_size, 0.0001);
        assert!(asset("NOKJPY", false).is_err());
        assert!(asset("AMP", true).is_ok());
        assert!(asset("APA", true).is_ok());
    }

    #[test]
    fn native_symbol_identity_and_precision_are_mandatory() {
        crate::ctrader::test_logging();
        verify("XPDUSD", "XPDUSD", 22346, 3, 0.01).unwrap();
        assert!(verify("XPDUSD", "XPD_USD", 22346, 3, 0.01).is_err());
        assert!(verify("XPDUSD", "XPDUSD", 0, 3, 0.01).is_err());
        assert!(verify("XPDUSD", "XPDUSD", 22346, 9, 0.01).is_err());
        for pip in [0.0, -0.01, f64::NAN, f64::INFINITY] {
            assert!(verify("XPDUSD", "XPDUSD", 22346, 3, pip).is_err());
        }
    }

    #[test]
    fn palladium_requires_one_explicitly_enabled_broker_symbol() {
        crate::ctrader::test_logging();
        let mut row = InstrumentSummary {
            id: 22346,
            name: "XPDUSD".into(),
            description: None,
            enabled: Some(true),
        };
        assert_eq!(available_symbol(&[row.clone()], "XPDUSD").unwrap(), 22346);
        assert!(available_symbol(&[], "XPDUSD").is_err());
        assert!(available_symbol(&[row.clone(), row.clone()], "XPDUSD").is_err());
        for enabled in [None, Some(false)] {
            row.enabled = enabled;
            assert!(available_symbol(&[row.clone()], "XPDUSD").is_err());
        }
    }
}
