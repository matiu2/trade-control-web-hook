//! Broker fetchers → normalized [`Bar`]s for the pure computation.
//!
//! Two paths, both producing the same `Vec<Bar>` in timestamp order:
//! - OANDA via `oanda_client::OandaClient::get_candles` (`price=MBA`).
//! - TradeNation via the `broker-tradenation-adapter`'s `get_bidask_candles`.
//!
//! Both filter to bars with finite, positive mid and finite spread ≥ 0, then
//! reduce to `(utc_hour, spread_frac, mid_close)`. Networking + auth live here;
//! [`crate::compute`] stays pure.

use chrono::Timelike;
use color_eyre::eyre::{Result, eyre};

use crate::compute::{Bar, MinuteBar};

/// How many H1 candles to pull per instrument. ~2000 ≈ 83 weekday-days
/// (~4 wall-months) — enough for stable per-UTC-hour p90 buckets, validated
/// on the OANDA scratch fetch.
pub const CANDLE_COUNT: usize = 2000;

/// Reduce one bar's mid/bid/ask closes to a [`Bar`], or `None` if degenerate
/// (non-positive or non-finite mid, non-finite spread). Shared by both paths.
fn bar_from_closes(utc_hour: u8, mid_close: f64, bid_close: f64, ask_close: f64) -> Option<Bar> {
    if !(mid_close.is_finite() && mid_close > 0.0) {
        return None;
    }
    let spread = ask_close - bid_close;
    if !spread.is_finite() || spread < 0.0 {
        return None;
    }
    Some(Bar {
        utc_hour,
        spread_frac: spread / mid_close,
        mid_close,
    })
}

/// Fetch + normalize OANDA H1 bid/ask candles for one instrument.
pub async fn fetch_oanda(client: &oanda_client::OandaClient, instrument: &str) -> Result<Vec<Bar>> {
    use oanda_client::candles::Granularity;
    let resp = client
        .get_candles(instrument, CANDLE_COUNT, Granularity::OneHour)
        .await
        .map_err(|e| eyre!("oanda get_candles({instrument}): {e}"))?;

    let mut bars = Vec::with_capacity(resp.candles.len());
    for c in &resp.candles {
        let (Some(bid), Some(ask), Some(mid)) = (&c.raw.bid, &c.raw.ask, &c.raw.mid) else {
            continue;
        };
        let hour = c.raw.time.with_timezone(&chrono::Utc).hour() as u8;
        if let Some(bar) = bar_from_closes(hour, mid.c(), bid.c(), ask.c()) {
            bars.push(bar);
        }
    }
    Ok(bars)
}

/// Normalize a slice of core `BidAskCandle`s (TradeNation path) to [`Bar`]s.
/// Separated from the fetch so it's unit-testable without a TN session.
pub fn bars_from_bidask(candles: &[trade_control_core::broker::BidAskCandle]) -> Vec<Bar> {
    candles
        .iter()
        .filter_map(|c| {
            let hour = c.time.hour() as u8;
            bar_from_closes(hour, c.c, c.bid_c, c.ask_c)
        })
        .collect()
}

// ---- minute-level path (bleed-resistant mask) ----

/// Reduce one minute's mid/bid/ask closes to a [`MinuteBar`], keyed by its UTC
/// minute-of-day. `None` for a degenerate bar (bad mid / inverted spread).
fn minute_bar_from_closes(
    utc_minute_of_day: u16,
    mid_close: f64,
    bid_close: f64,
    ask_close: f64,
) -> Option<MinuteBar> {
    if !(mid_close.is_finite() && mid_close > 0.0) {
        return None;
    }
    let spread = ask_close - bid_close;
    if !spread.is_finite() || spread < 0.0 {
        return None;
    }
    Some(MinuteBar {
        utc_minute_of_day,
        spread_frac: spread / mid_close,
        mid_close,
    })
}

/// Fetch OANDA M1 bid/ask candles for the last `days`, paging **forward** from
/// `since` in 5000-bar chunks (OANDA's per-request ceiling), normalized to
/// [`MinuteBar`]s in timestamp order.
pub async fn fetch_oanda_minutes(
    client: &oanda_client::OandaClient,
    instrument: &str,
    days: i64,
) -> Result<Vec<MinuteBar>> {
    use chrono::{Duration, Timelike, Utc};
    use oanda_client::candles::Granularity;

    let now = Utc::now();
    let since = now - Duration::days(days);
    let mut cursor = since.fixed_offset();
    let per = 5000usize;
    let mut out: Vec<MinuteBar> = Vec::new();
    let mut last_seen: Option<chrono::DateTime<Utc>> = None;
    loop {
        let resp = client
            .get_candles_from(instrument, cursor, per, Granularity::OneMinute)
            .await
            .map_err(|e| eyre!("oanda get_candles_from({instrument}): {e}"))?;
        if resp.candles.is_empty() {
            break;
        }
        let mut latest = cursor.with_timezone(&Utc);
        for c in &resp.candles {
            let (Some(bid), Some(ask), Some(mid)) = (&c.raw.bid, &c.raw.ask, &c.raw.mid) else {
                continue;
            };
            let t = c.raw.time.with_timezone(&Utc);
            latest = latest.max(t);
            if t >= now {
                continue;
            }
            // Skip anything at/older than the last bar we already recorded
            // (chunks overlap by design).
            if last_seen.is_some_and(|ls| t <= ls) {
                continue;
            }
            let mod_ = (t.hour() * 60 + t.minute()) as u16;
            if let Some(mb) = minute_bar_from_closes(mod_, mid.c(), bid.c(), ask.c()) {
                out.push(mb);
                last_seen = Some(t);
            }
        }
        if latest >= now || resp.candles.len() < per {
            break;
        }
        let next = latest + Duration::minutes(1);
        if next.fixed_offset() <= cursor {
            break; // no forward progress
        }
        cursor = next.fixed_offset();
    }
    Ok(out)
}

/// Normalize core `BidAskCandle`s (TradeNation M1, from the paged adapter) to
/// [`MinuteBar`]s. Unit-testable without a TN session.
pub fn minutes_from_bidask(candles: &[trade_control_core::broker::BidAskCandle]) -> Vec<MinuteBar> {
    candles
        .iter()
        .filter_map(|c| {
            let mod_ = (c.time.hour() * 60 + c.time.minute()) as u16;
            minute_bar_from_closes(mod_, c.c, c.bid_c, c.ask_c)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_from_closes_computes_spread_frac() {
        let b = bar_from_closes(21, 1.0000, 0.9998, 1.0002).expect("valid bar");
        assert_eq!(b.utc_hour, 21);
        assert!((b.spread_frac - 0.0004).abs() < 1e-12);
        assert_eq!(b.mid_close, 1.0000);
    }

    #[test]
    fn bar_from_closes_rejects_bad_mid() {
        assert!(bar_from_closes(0, 0.0, 0.9998, 1.0002).is_none());
        assert!(bar_from_closes(0, f64::NAN, 0.9998, 1.0002).is_none());
    }

    #[test]
    fn bar_from_closes_rejects_inverted_spread() {
        assert!(bar_from_closes(0, 1.0, 1.0002, 0.9998).is_none());
    }
}
