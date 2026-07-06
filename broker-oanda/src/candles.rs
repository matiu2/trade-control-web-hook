//! OANDA candle-history fetch for the trade-plan engine.
//!
//! Maps the engine's small [`Granularity`] set onto `oanda_client`'s own
//! `Granularity`, fetches the `(since, now]` window via `get_candles_range`
//! (price `MBA` → take MID), drops the still-forming bar (`complete == false`)
//! and any candle missing a MID block, and runs the rest through
//! [`filter_new_candles`] so only closed candles strictly after the watermark
//! come back, oldest first.

use chrono::{DateTime, Utc};
use oanda_client::OandaClient;
use oanda_client::candles::Granularity as OandaGranularity;
use trade_control_core::broker::{
    BidAskCandle, Candle, CandleError, Granularity, filter_new_candles,
};

/// Engine granularity → `oanda_client` granularity. Total — every engine
/// variant has a direct OANDA equivalent.
fn to_oanda(g: Granularity) -> OandaGranularity {
    match g {
        Granularity::M1 => OandaGranularity::OneMinute,
        Granularity::M5 => OandaGranularity::FiveMinutes,
        Granularity::M15 => OandaGranularity::FifteenMinutes,
        Granularity::H1 => OandaGranularity::OneHour,
        Granularity::H4 => OandaGranularity::FourHours,
        Granularity::D1 => OandaGranularity::OneDay,
    }
}

/// Fetch closed MID candles for `instrument` in `(since, now]`. See the
/// [`Broker::get_candles`](trade_control_core::broker::Broker::get_candles)
/// contract.
pub async fn get_candles(
    client: &OandaClient,
    instrument: &str,
    granularity: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<Candle>, CandleError> {
    if since >= now {
        return Err(CandleError::BadRange);
    }

    let resp = client
        .get_candles_range(
            instrument,
            since.fixed_offset(),
            now.fixed_offset(),
            to_oanda(granularity),
        )
        .await
        .map_err(|err| {
            tracing::error!("oanda get_candles_range({instrument}): {err:?}");
            CandleError::Transient
        })?;

    let candles = resp
        .candles
        .into_iter()
        .filter(|c| c.raw.complete) // drop the still-forming bar
        .filter_map(|c| {
            let mid = c.raw.mid.as_ref()?;
            Some(Candle {
                time: c.raw.time.with_timezone(&Utc),
                o: mid.o(),
                h: mid.h(),
                l: mid.l(),
                c: mid.c(),
            })
        })
        .collect();

    Ok(filter_new_candles(candles, since))
}

/// Fetch closed candles for `instrument` in `(since, now]` carrying **both**
/// mid and the bid/ask books OANDA already returns for `MBA`. Same windowing
/// contract as [`get_candles`] (closed-only, strictly-after-`since`,
/// ascending), but the mid-only discard is dropped so a caller can read the
/// real per-bar spread. See
/// [`Broker::get_bidask_candles`](trade_control_core::broker::Broker::get_bidask_candles).
///
/// A candle missing **either** the bid or ask block (OANDA can serve a partial
/// `MBA` in rare gaps) is dropped rather than mid-filled — the entry SL-floor
/// wants a genuine two-sided read, and a mid-filled zero-spread bar would
/// silently deflate the mean. The mid block is required too (same as
/// [`get_candles`]).
pub async fn get_bidask_candles(
    client: &OandaClient,
    instrument: &str,
    granularity: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<BidAskCandle>, CandleError> {
    if since >= now {
        return Err(CandleError::BadRange);
    }

    let resp = client
        .get_candles_range(
            instrument,
            since.fixed_offset(),
            now.fixed_offset(),
            to_oanda(granularity),
        )
        .await
        .map_err(|err| {
            tracing::error!("oanda get_candles_range({instrument}) [bid/ask]: {err:?}");
            CandleError::Transient
        })?;

    let mut candles: Vec<BidAskCandle> = resp
        .candles
        .into_iter()
        .filter(|c| c.raw.complete) // drop the still-forming bar
        .filter_map(|c| {
            // Require all three books — a partial MBA is dropped, not mid-filled,
            // so the spread read is genuine (see fn docs).
            let mid = c.raw.mid.as_ref()?;
            let bid = c.raw.bid.as_ref()?;
            let ask = c.raw.ask.as_ref()?;
            let time = c.raw.time.with_timezone(&Utc);
            if time <= since {
                return None; // strictly after the watermark
            }
            Some(BidAskCandle {
                time,
                o: mid.o(),
                h: mid.h(),
                l: mid.l(),
                c: mid.c(),
                bid_o: bid.o(),
                bid_h: bid.h(),
                bid_l: bid.l(),
                bid_c: bid.c(),
                ask_o: ask.o(),
                ask_h: ask.h(),
                ask_l: ask.l(),
                ask_c: ask.c(),
            })
        })
        .collect();
    candles.sort_by_key(|c| c.time);
    Ok(candles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granularity_maps_to_oanda() {
        assert_eq!(to_oanda(Granularity::M1), OandaGranularity::OneMinute);
        assert_eq!(to_oanda(Granularity::M5), OandaGranularity::FiveMinutes);
        assert_eq!(to_oanda(Granularity::M15), OandaGranularity::FifteenMinutes);
        assert_eq!(to_oanda(Granularity::H1), OandaGranularity::OneHour);
        assert_eq!(to_oanda(Granularity::H4), OandaGranularity::FourHours);
        assert_eq!(to_oanda(Granularity::D1), OandaGranularity::OneDay);
    }
}
