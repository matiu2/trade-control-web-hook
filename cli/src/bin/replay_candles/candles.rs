//! Pull a historical candle window from the chosen broker (via candle-cache)
//! and convert it into the engine's mid-only `Candle` type.
//!
//! candle-cache returns `candle_model::CandleData` (mid OHLC + volume,
//! `DateTime<FixedOffset>`); the engine evaluates over
//! `trade_control_core::broker::Candle` (mid OHLC, `DateTime<Utc>`, no volume).
//! The conversion is a straight field map plus a timezone normalisation — OANDA
//! and TradeNation both stamp UTC-offset times, so `with_timezone(&Utc)` is
//! lossless.

use candle_cache::{CacheClient, CacheConfig};
use chrono::{DateTime, FixedOffset, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use oanda_client::{OandaClient, data_source::OandaDataSource};
use std::path::PathBuf;
use trade_control_core::broker::Candle as EngineCandle;
use tradenation_api::TradeNationClient;

use super::granularity::ReplayGranularity;
use super::source::CandleSource;

/// Pull `[from, to]` candles for `symbol` at `granularity` from `source`, in
/// ascending time order, already converted to engine candles.
pub async fn pull(
    source: CandleSource,
    symbol: &str,
    granularity: ReplayGranularity,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    cache_dir: Option<PathBuf>,
) -> Result<Vec<EngineCandle>> {
    let config = match cache_dir {
        Some(dir) => CacheConfig::default().with_cache_dir(dir),
        None => CacheConfig::default(),
    };
    let from_fx = from.fixed_offset();
    let to_fx = to.fixed_offset();
    let gran = granularity.candle_model();

    let candles = match source {
        CandleSource::Oanda => {
            let client = CacheClient::new(config, oanda_source()?).await?;
            client
                .get_candles_range(symbol, from_fx, to_fx, gran)
                .await
                .wrap_err("pull OANDA candles")?
        }
        CandleSource::TradeNation => {
            let client = CacheClient::new(config, tradenation_source()?).await?;
            client
                .get_candles_range(symbol, from_fx, to_fx, gran)
                .await
                .wrap_err("pull TradeNation candles")?
        }
    };

    Ok(candles.candles.iter().map(to_engine_candle).collect())
}

/// Build an OANDA data source from `OANDA_TOKEN` / `OANDA_ACCOUNT_ID`.
fn oanda_source() -> Result<OandaDataSource> {
    let token = std::env::var("OANDA_TOKEN")
        .map_err(|_| eyre!("OANDA_TOKEN not set (required for --source oanda)"))?;
    let account_id = std::env::var("OANDA_ACCOUNT_ID")
        .map_err(|_| eyre!("OANDA_ACCOUNT_ID not set (required for --source oanda)"))?;
    Ok(OandaDataSource::new(OandaClient::new(token), account_id))
}

/// Build a TradeNation client. `TN_ACCOUNT_TYPE=demo` (the default) bootstraps a
/// demo session with no creds; `live` needs `TN_USERNAME` + `TN_PASSWORD`.
fn tradenation_source() -> Result<TradeNationClient> {
    let kind = std::env::var("TN_ACCOUNT_TYPE").unwrap_or_else(|_| "demo".to_string());
    match kind.to_ascii_lowercase().as_str() {
        "live" => {
            let user = std::env::var("TN_USERNAME")
                .map_err(|_| eyre!("TN_USERNAME not set (required for TN_ACCOUNT_TYPE=live)"))?;
            let pass = std::env::var("TN_PASSWORD")
                .map_err(|_| eyre!("TN_PASSWORD not set (required for TN_ACCOUNT_TYPE=live)"))?;
            Ok(TradeNationClient::new(user, pass))
        }
        "demo" => Ok(TradeNationClient::new_demo()),
        other => Err(eyre!(
            "TN_ACCOUNT_TYPE={other:?} not understood; use `demo` or `live`"
        )),
    }
}

/// Field-map a candle-cache candle to the engine's mid candle (drop volume,
/// normalise the timestamp to UTC).
fn to_engine_candle(cd: &candle_model::CandleData) -> EngineCandle {
    EngineCandle {
        time: to_utc(cd.timestamp),
        o: cd.open,
        h: cd.high,
        l: cd.low,
        c: cd.close,
    }
}

fn to_utc(ts: DateTime<FixedOffset>) -> DateTime<Utc> {
    ts.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn converts_candle_fields_and_timezone() {
        // +10:00 (Brisbane) stamped candle normalises to the same instant in UTC.
        let bne = FixedOffset::east_opt(10 * 3600).unwrap();
        let ts = bne.with_ymd_and_hms(2026, 6, 18, 21, 0, 0).unwrap();
        let cd = candle_model::CandleData::new(1.0, 1.5, 0.9, 1.2, 100.0, ts).unwrap();

        let ec = to_engine_candle(&cd);
        assert_eq!(
            ec.time,
            Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap()
        );
        assert_eq!((ec.o, ec.h, ec.l, ec.c), (1.0, 1.5, 0.9, 1.2));
    }
}
