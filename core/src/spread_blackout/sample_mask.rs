//! Drop bid/ask samples whose **close** lands in a spread hour.
//!
//! The SL-spread floor averages `ask_c − bid_c` over the last few plan bars. A
//! bar's close spread is the spread *at the close instant*, so a bar that closes
//! inside a masked spread hour contributes the liquidity-trough spike — on the
//! 17:00-New-York D1 grid that is every single bar. Those hours are already owned
//! by the `SpreadHour` hold (the order is pulled before the spike and re-placed
//! after it), so sizing the stop off them widens it for a window the order never
//! rests through. This filter removes them before the shared mean.

use crate::broker::BidAskCandle;

/// The candles whose close instant (`time + bar_seconds`) falls **outside**
/// `instrument`'s spread hours (per [`super::is_spread_hour`], lead included),
/// in their original order.
///
/// Keyed on the close, not the open: an H1 bar opening at 16:00 NY closes on
/// the 17:00 print and is dropped; the bar opening at 17:00 closes at 18:00 and
/// is kept. An instrument with no baked mask falls through to `is_spread_hour`'s
/// legacy NY-close-edge rule, so this is never a no-op by accident.
pub fn closes_outside_spread_hours(
    instrument: &str,
    candles: &[BidAskCandle],
    bar_seconds: i64,
) -> Vec<BidAskCandle> {
    candles
        .iter()
        .filter(|c| {
            let close_at = c.time + chrono::Duration::seconds(bar_seconds);
            !super::is_spread_hour(instrument, close_at)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid test timestamp")
            .with_timezone(&Utc)
    }

    /// A bar opening at `open` with a flat 2p book — the spread is irrelevant
    /// to the filter, only the time is.
    fn bar(open: &str) -> BidAskCandle {
        BidAskCandle {
            time: at(open),
            o: 1.1,
            h: 1.1,
            l: 1.1,
            c: 1.1,
            bid_o: 1.1,
            bid_h: 1.1,
            bid_l: 1.1,
            bid_c: 1.0999,
            ask_o: 1.1,
            ask_h: 1.1,
            ask_l: 1.1,
            ask_c: 1.1001,
        }
    }

    const H1: i64 = 3600;
    const D1: i64 = 86_400;

    /// EUR_USD's baked mask is exactly local hour 17 (New York). On 2026-03-12
    /// (EDT, DST began 03-08) that is 21:00Z. The H1 bar that CLOSES at 21:00Z
    /// is the spike sample and goes; the bar that OPENS at 21:00Z closes at
    /// 22:00Z and stays.
    /// A filter keyed on the open would keep the first and drop the second.
    #[test]
    fn keyed_on_the_close_not_the_open() {
        let bars = [
            bar("2026-03-12T19:00:00Z"), // closes 20:00Z = 16:00 NY, lead not reached
            bar("2026-03-12T20:00:00Z"), // closes 21:00Z = 17:00 NY → masked
            bar("2026-03-12T21:00:00Z"), // closes 22:00Z = 18:00 NY
        ];
        let kept = closes_outside_spread_hours("EUR_USD", &bars, H1);
        let kept_opens: Vec<_> = kept.iter().map(|c| c.time).collect();
        assert_eq!(
            kept_opens,
            vec![at("2026-03-12T19:00:00Z"), at("2026-03-12T21:00:00Z")],
            "only the bar closing ON the 17:00 NY print should be dropped",
        );
    }

    /// Every D1 bar on the 17:00-New-York grid closes on the spike, so the
    /// whole window goes — the caller then falls back to the live quote.
    #[test]
    fn a_daily_window_on_the_ny_grid_is_entirely_masked() {
        let bars = [
            bar("2026-03-09T21:00:00Z"),
            bar("2026-03-10T21:00:00Z"),
            bar("2026-03-11T21:00:00Z"),
        ];
        assert!(closes_outside_spread_hours("EUR_USD", &bars, D1).is_empty());
    }

    /// The 30-minute lead applies: a bar closing at 16:45 NY is inside the lead
    /// window of the 17:00 hour and is dropped too.
    #[test]
    fn the_lead_window_before_the_hour_is_masked_too() {
        let bars = [bar("2026-03-12T20:30:00Z")]; // M15 closing 20:45Z = 16:45 NY
        assert!(closes_outside_spread_hours("EUR_USD", &bars, 900).is_empty());
    }

    #[test]
    fn empty_in_empty_out() {
        assert!(closes_outside_spread_hours("EUR_USD", &[], H1).is_empty());
    }

    /// A quiet stretch passes through untouched, in order.
    #[test]
    fn unmasked_bars_pass_through_in_order() {
        let bars = [
            bar("2026-03-12T08:00:00Z"),
            bar("2026-03-12T09:00:00Z"),
            bar("2026-03-12T10:00:00Z"),
        ];
        let kept = closes_outside_spread_hours("EUR_USD", &bars, H1);
        assert_eq!(kept.len(), 3);
        assert!(kept.windows(2).all(|w| w[0].time < w[1].time));
    }
}
