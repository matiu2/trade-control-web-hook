//! Read-only spread calibration has no order-sizing or lot-multiplier semantics.
use super::asset as catalog_asset;
use broker_ctrader::{
    InstrumentMetadata,
    market_data::{ChartData, InstrumentSummary},
};
use color_eyre::{Result, eyre::ensure};
use instrument_lookup::{Asset, AssetClass};

/// The default discovery remains FX-only. Palladium must be explicitly requested.
pub fn asset(symbol: &str, explicit: bool) -> Result<&'static Asset> {
    let asset = catalog_asset(symbol)?;
    ensure!(
        asset.class == AssetClass::Forex
            || (explicit && symbol == "XPDUSD" && asset.class == AssetClass::Commodity),
        "not a supported read-only cTrader calibration instrument"
    );
    Ok(asset)
}

/// Preserve FX leg validation; palladium only needs its exact quote identity.
pub async fn metadata(
    data: &ChartData,
    symbol: &str,
    explicit: bool,
) -> Result<InstrumentMetadata> {
    let asset = asset(symbol, explicit)?;
    let metadata = if asset.class == AssetClass::Forex {
        data.forex_instrument_metadata(symbol).await?
    } else {
        let expected_id = available_palladium(&data.catalogue().await?, symbol)?;
        let metadata = data.instrument_metadata(symbol).await?;
        ensure!(
            metadata.id == expected_id,
            "cTrader palladium symbol ID changed"
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

fn available_palladium(catalogue: &[InstrumentSummary], symbol: &str) -> Result<i64> {
    let matches = catalogue
        .iter()
        .filter(|row| row.name == symbol)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1 && matches[0].id > 0 && matches[0].enabled == Some(true),
        "cTrader palladium symbol is missing, disabled or ambiguous"
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
    fn palladium_is_explicit_and_does_not_broaden_default_fx_discovery() {
        crate::ctrader::test_logging();
        assert!(asset("EURUSD", false).is_ok());
        assert!(asset("XPDUSD", false).is_err());
        assert_eq!(asset("XPDUSD", true).unwrap().class, AssetClass::Commodity);
        for symbol in ["XAUUSD", "BTCUSD", "EURUSD*t", "NO_SUCH_SYMBOL"] {
            assert!(asset(symbol, true).is_err(), "accepted {symbol}");
        }
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
        assert_eq!(
            available_palladium(&[row.clone()], "XPDUSD").unwrap(),
            22346
        );
        assert!(available_palladium(&[], "XPDUSD").is_err());
        assert!(available_palladium(&[row.clone(), row.clone()], "XPDUSD").is_err());
        for enabled in [None, Some(false)] {
            row.enabled = enabled;
            assert!(available_palladium(&[row.clone()], "XPDUSD").is_err());
        }
    }
}
