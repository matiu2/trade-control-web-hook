//! H1 candle fetchers for both venues, mapping each venue's candle type into
//! the analysis's plain [`Bar`](crate::compute::Bar).
//!
//! Kept thin: no ATR/gap logic here (that's [`compute`](crate::compute)); just
//! "hit the API, hand back a UTC-sorted `Vec<Bar>`". Errors bubble up as
//! `color_eyre` so the caller can flag the row unreviewed and carry on.

use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;

use crate::compute::Bar;

/// OANDA per-request cap is 5000 candles (~208 days of H1) — plenty for gap
/// statistics.
pub const OANDA_H1_COUNT: usize = 5000;

/// ~1 year of H1 for TradeNation (24 × 365 = 8760; API cap ~9999).
pub const TN_H1_COUNT: usize = 24 * 365;

/// Fetch OANDA H1 mid bars for `symbol` (practice env), complete candles only.
pub async fn oanda_bars(
    client: &oanda_client::OandaClient,
    symbol: &str,
) -> color_eyre::Result<Vec<Bar>> {
    use oanda_client::candles::Granularity;

    let resp = client
        .get_candles(symbol, OANDA_H1_COUNT, Granularity::OneHour)
        .await
        .map_err(|e| eyre!("oanda get_candles {symbol}: {e}"))?;

    let bars: Vec<Bar> = resp
        .candles
        .iter()
        .filter(|c| c.raw.complete)
        .filter_map(|c| {
            let m = c.raw.mid.as_ref()?;
            Some(Bar {
                t: c.raw.time.with_timezone(&Utc),
                o: m.open.to_f64(),
                h: m.high.to_f64(),
                l: m.low.to_f64(),
                c: m.close.to_f64(),
            })
        })
        .collect();
    Ok(bars)
}

/// Fetch TradeNation H1 mid bars for `tn_symbol` (~1 year), via a resolved
/// market id on the shared demo session.
pub async fn tn_bars(
    http: &reqwest::Client,
    session: &tradenation_api::Session,
    tn_symbol: &str,
) -> color_eyre::Result<Vec<Bar>> {
    use candle_model::Granularity;
    use tradenation_api::{PriceType, get_candles, resolve_market};

    let market = resolve_market(http, session, tn_symbol)
        .await
        .map_err(|e| eyre!("tn resolve_market {tn_symbol}: {e}"))?;
    let candles = get_candles(
        http,
        market.market_id,
        Granularity::OneHour,
        PriceType::Mid,
        TN_H1_COUNT,
    )
    .await
    .map_err(|e| eyre!("tn get_candles {tn_symbol}: {e}"))?;

    let bars: Vec<Bar> = candles
        .iter()
        .map(|c| {
            let t: DateTime<Utc> = c.timestamp.with_timezone(&Utc);
            Bar {
                t,
                o: c.open,
                h: c.high,
                l: c.low,
                c: c.close,
            }
        })
        .collect();
    Ok(bars)
}
