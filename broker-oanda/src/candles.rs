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
use trade_control_core::broker::{Candle, CandleError, Granularity, filter_new_candles};

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
