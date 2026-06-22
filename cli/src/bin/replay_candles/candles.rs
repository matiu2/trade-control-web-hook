//! Pull a historical candle window from the chosen broker (via candle-cache)
//! and convert it into the engine's `BidAskCandle` type.
//!
//! candle-cache returns `candle_model::BidAskCandleData` (mid + bid + ask OHLC,
//! `DateTime<FixedOffset>`); the simulator fills against
//! `trade_control_core::broker::BidAskCandle` (the same books, `DateTime<Utc>`,
//! no volume) while the engine evaluates over its `.mid()` view. We pull bid/ask
//! once and hand mid to the engine, real books to the fill simulator — so the
//! replay's fills carry the broker's real per-bar spread (not a flat synthetic
//! half-spread). The conversion is a field map plus a timezone normalisation —
//! OANDA and TradeNation both stamp UTC-offset times, so `with_timezone(&Utc)`
//! is lossless. A data source that serves mid-only yields bid == ask == mid,
//! and the simulator degrades cleanly to exact-level mid fills.

use candle_cache::{CacheClient, CacheConfig};
use chrono::{DateTime, FixedOffset, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use oanda_client::{OandaClient, data_source::OandaDataSource};
use std::path::PathBuf;
use trade_control_core::broker::BidAskCandle as EngineCandle;
use tradenation_api::TradeNationClient;

use super::granularity::ReplayGranularity;
use super::source::CandleSource;

/// Pull `[from, to]` bid/ask candles for `symbol` at `granularity` from
/// `source`, in ascending time order, already converted to engine bid/ask
/// candles.
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
                .get_candles_range_bid_ask(symbol, from_fx, to_fx, gran)
                .await
                .wrap_err("pull OANDA bid/ask candles")?
        }
        CandleSource::TradeNation => {
            let client = CacheClient::new(config, tradenation_source()?).await?;
            client
                .get_candles_range_bid_ask(symbol, from_fx, to_fx, gran)
                .await
                .wrap_err("pull TradeNation bid/ask candles")?
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

/// Field-map a candle-cache bid/ask candle to the engine's bid/ask candle (drop
/// volume, normalise the timestamp to UTC).
fn to_engine_candle(cd: &candle_model::BidAskCandleData) -> EngineCandle {
    EngineCandle {
        time: to_utc(cd.timestamp),
        o: cd.open,
        h: cd.high,
        l: cd.low,
        c: cd.close,
        bid_o: cd.bid_open,
        bid_h: cd.bid_high,
        bid_l: cd.bid_low,
        bid_c: cd.bid_close,
        ask_o: cd.ask_open,
        ask_h: cd.ask_high,
        ask_l: cd.ask_low,
        ask_c: cd.ask_close,
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
        // +10:00 (Brisbane) stamped candle normalises to the same instant in UTC,
        // and all three books (mid/bid/ask) map through.
        let bne = FixedOffset::east_opt(10 * 3600).unwrap();
        let ts = bne.with_ymd_and_hms(2026, 6, 18, 21, 0, 0).unwrap();
        let cd = candle_model::BidAskCandleData {
            open: 1.0,
            high: 1.5,
            low: 0.9,
            close: 1.2,
            volume: 100.0,
            timestamp: ts,
            bid_open: 0.99,
            bid_high: 1.49,
            bid_low: 0.89,
            bid_close: 1.19,
            ask_open: 1.01,
            ask_high: 1.51,
            ask_low: 0.91,
            ask_close: 1.21,
        };

        let ec = to_engine_candle(&cd);
        assert_eq!(
            ec.time,
            Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap()
        );
        assert_eq!((ec.o, ec.h, ec.l, ec.c), (1.0, 1.5, 0.9, 1.2));
        assert_eq!(
            (ec.bid_o, ec.bid_h, ec.bid_l, ec.bid_c),
            (0.99, 1.49, 0.89, 1.19)
        );
        assert_eq!(
            (ec.ask_o, ec.ask_h, ec.ask_l, ec.ask_c),
            (1.01, 1.51, 0.91, 1.21)
        );
    }
}
