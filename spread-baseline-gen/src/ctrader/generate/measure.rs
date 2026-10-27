use crate::ctrader::InstrumentReport;
use broker_ctrader::market_data::ChartData;
use candle_model::Granularity;
use chrono::{DateTime, Duration, Utc};
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
#[cfg(test)]
fn asset_for_fx(symbol: &str) -> Result<&'static instrument_lookup::Asset> {
    super::super::calibration::asset(symbol, false)
}

pub(super) async fn measure(
    data: &ChartData,
    feed: &candle_cache::ctrader::CachedFeed,
    symbol: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    window_hours: u32,
) -> Result<InstrumentReport> {
    let asset = super::super::calibration::asset(symbol, true)?;
    let metadata = super::super::calibration::metadata(data, symbol, true).await?;
    ensure!(
        metadata.name == symbol
            && metadata.id > 0
            && metadata.pip_size.is_finite()
            && metadata.pip_size > 0.0,
        "invalid broker symbol identity/pip metadata for {symbol}"
    );
    if metadata.pip_size != asset.pip_size {
        tracing::info!(
            symbol,
            broker_pip_size = metadata.pip_size,
            catalogue_pip_size = asset.pip_size,
            "cTrader spread calibration uses the account's broker-native pip convention"
        );
    }
    let timezone = super::super::calibration::timezone(asset)?;
    let mut candles = Vec::new();
    let mut cursor = from;
    while cursor < to {
        let end = (cursor + Duration::hours(i64::from(window_hours))).min(to);
        let chunk = feed
            .range_bid_ask(
                symbol,
                cursor.fixed_offset(),
                end.fixed_offset(),
                Granularity::OneMinute,
            )
            .await
            .wrap_err_with(|| format!("paired cTrader {symbol} [{cursor},{end})"))?;
        candles.extend(chunk.candles);
        cursor = end;
        tracing::info!(symbol,through=%cursor,minutes=candles.len(),"paired calibration window cached");
    }
    let measurement = crate::mt5::instrument_report(
        symbol,
        metadata.pip_size,
        &asset.spread_schedule,
        timezone,
        &candles,
    )
    .wrap_err("cTrader paired spread calculation")?;
    Ok(InstrumentReport {
        symbol_id: metadata.id,
        measurement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fx_metadata_is_recognized_but_metals_unknown_and_guessed_suffixes_are_rejected() {
        crate::ctrader::test_logging();
        assert_eq!(asset_for_fx("EURUSD").unwrap().pip_size, 0.0001);
        assert_eq!(asset_for_fx("USDJPY").unwrap().pip_size, 0.01);
        assert!(asset_for_fx("XAUUSD").is_err());
        assert!(asset_for_fx("EURUSD*t").is_err());
        assert!(asset_for_fx("NO_SUCH_SYMBOL").is_err());
    }
}
