//! Explicit native MT5 aliases for pip units and market-local schedules only.
//! Every native instrument still gets its own measured/account-scoped profile.
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use instrument_lookup::{Asset, AssetClass};

pub fn resolve(symbol: &str) -> Result<&'static Asset> {
    let canonical = match symbol {
        "DAX40" => "DE30",
        "SP500" => "SPX500",
        "XTIUSD" => "WTICOUSD",
        "XBRUSD" => "BCOUSD",
        name => name.strip_suffix("*t").unwrap_or(name),
    };
    let asset = instrument_lookup::resolve(canonical)?
        .ok_or_else(|| eyre!("no instrument-lookup pip/schedule metadata for {symbol}"))?;
    if symbol.ends_with("*t") {
        ensure!(
            asset.class == AssetClass::Forex,
            "unsupported non-FX MT5 *t variant {symbol}"
        );
    }
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_aliases_retain_the_underlying_schedule_and_units() {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        for (native, canonical) in [
            ("EURUSD*t", "EURUSD"),
            ("GBPUSD*t", "GBPUSD"),
            ("NZDUSD*t", "NZDUSD"),
            ("DAX40", "DE30"),
            ("SP500", "SPX500"),
            ("XTIUSD", "WTICOUSD"),
            ("XBRUSD", "BCOUSD"),
        ] {
            let got = resolve(native).unwrap();
            let expected = instrument_lookup::resolve(canonical).unwrap().unwrap();
            assert_eq!(got.id, expected.id);
            assert_eq!(got.pip_size, expected.pip_size);
            assert_eq!(got.spread_schedule_tz(), expected.spread_schedule_tz());
        }
        assert!(resolve("XAUUSD*t").is_err());
        assert!(resolve("UNKNOWN").is_err());
    }
}
