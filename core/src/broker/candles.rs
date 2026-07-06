//! Candle history types for the [`Broker`](super::Broker) trait.
//!
//! The request/response webhook only ever needed a *live quote*
//! ([`Quote`](super::Quote)) — TradingView evaluated every price/time
//! condition and POSTed the already-fired action. The server-side trade-plan
//! engine inverts that: it polls broker candles on a cron tick and evaluates
//! the conditions itself. That needs a windowed candle-history read, which is
//! what this module's [`Candle`] / [`Granularity`] / [`Broker::get_candles`]
//! (see [`super`]) surface adds.
//!
//! All candles are **MID** prices (same basis as the M/W geometry and the
//! Pine `Shell`); the mid→bid/ask correction stays downstream in the entry
//! resolver. Times are **UTC**; each broker impl converts from its own
//! timezone at the adapter boundary.

use chrono::{DateTime, Utc};

/// One closed OHLC candle, mid prices, UTC-stamped.
///
/// `time` is the candle's **open** time (the convention both OANDA and
/// TradeNation report), so a freshly-closed H1 bar opened at 14:00 carries
/// `time = 14:00` even though it closed at 15:00. The engine's watermark is
/// compared against this `time`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Candle {
    pub time: DateTime<Utc>,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
}

/// One closed OHLC candle carrying **both** mid and the broker's bid/ask
/// books, UTC-stamped. The engine never sees this — it runs on [`Candle`]
/// (mid). It exists for the **fill simulator**, which must reproduce the
/// broker's real spread: a buy fills on the ask, a sell on the bid, and the
/// spread varies bar to bar (wide at session opens / news). The synthetic
/// `mid ± half_spread` shift the simulator used before is replaced by these
/// real per-bar books.
///
/// `time` is the **open** time, same convention as [`Candle`]. `.mid()` drops
/// the books and returns the plain mid candle so a mid-only consumer (the
/// engine, the detector) can reuse a bid/ask series without a separate pull.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BidAskCandle {
    pub time: DateTime<Utc>,
    // Mid OHLC (what the engine evaluates on).
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    // Bid book (a sell fills here).
    pub bid_o: f64,
    pub bid_h: f64,
    pub bid_l: f64,
    pub bid_c: f64,
    // Ask book (a buy fills here).
    pub ask_o: f64,
    pub ask_h: f64,
    pub ask_l: f64,
    pub ask_c: f64,
}

impl BidAskCandle {
    /// The mid-only view of this candle — for mid consumers (engine, detector)
    /// that share a bid/ask series.
    pub fn mid(&self) -> Candle {
        Candle {
            time: self.time,
            o: self.o,
            h: self.h,
            l: self.l,
            c: self.c,
        }
    }

    /// This bar's close spread, `ask_c − bid_c` (raw price). The per-bar sample
    /// the entry SL-spread floor averages.
    pub fn close_spread(&self) -> f64 {
        self.ask_c - self.bid_c
    }
}

/// The mean close spread (`ask_c − bid_c`) over the **last `window`** candles of
/// an ascending `candles` slice, and how many bars fed the mean — the single
/// "trailing window → spread" reduction shared by the live worker
/// (`run_enter`'s `windowed_entry_spread`, over broker-fetched candles) and the
/// offline replay (its `Fire`-builder, over the recorded series). Both fetch the
/// candles through the **same** provider ([`super::Broker::get_bidask_candles`])
/// and then call THIS to reduce them, so they can't size the floor off different
/// statistics.
///
/// `window` is the trade's [`spread_window`](crate::intent::Intent::spread_window)
/// (already defaulted). Takes the most recent `window` bars (the tail, since the
/// slice is ascending), maps each to its [`BidAskCandle::close_spread`], and
/// means them via [`crate::intent::mean_spread`] (which drops degenerate
/// samples). `None` when `candles` is empty or every recent bar is degenerate —
/// the caller then fails open to its single-sample path.
pub fn trailing_spread_mean(candles: &[BidAskCandle], window: u32) -> Option<(f64, usize)> {
    if candles.is_empty() {
        return None;
    }
    let window = window.max(1) as usize;
    let start = candles.len().saturating_sub(window);
    let spreads: Vec<f64> = candles[start..]
        .iter()
        .map(BidAskCandle::close_spread)
        .collect();
    let mean = crate::intent::mean_spread(&spreads)?;
    Some((mean, spreads.len()))
}

/// The candle timeframes the engine fetches. Deliberately a small closed set —
/// only the granularities trades are actually armed on — so every broker can
/// map it without an "unsupported" runtime branch leaking into the engine.
///
/// TradeNation natively serves minute / quarter (15m) / hour / day; M5 and H4
/// are aggregated upstream by `tradenation-api`. OANDA serves all six directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    M1,
    M5,
    M15,
    H1,
    H4,
    D1,
}

impl Granularity {
    /// The bar length in seconds — used by the engine to reason about how many
    /// candles a cron gap can span and to size count-back fetches.
    pub fn seconds(self) -> i64 {
        match self {
            Granularity::M1 => 60,
            Granularity::M5 => 5 * 60,
            Granularity::M15 => 15 * 60,
            Granularity::H1 => 60 * 60,
            Granularity::H4 => 4 * 60 * 60,
            Granularity::D1 => 24 * 60 * 60,
        }
    }
}

/// Failure modes for [`Broker::get_candles`](super::Broker::get_candles).
/// Mirrors the [`LookupError`](super::LookupError) playbook: `Transient` means
/// "the read failed, skip this tick and try the next" — never a signal to busy
/// -retry inside one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandleError {
    /// Network / 5xx / decode / other transient broker failure.
    Transient,
    /// The requested window is degenerate (`since >= now`) or the broker
    /// rejected the range. Distinct from `Transient` so the engine logs it as
    /// a logic bug rather than a flaky feed.
    BadRange,
}

impl core::fmt::Display for CandleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transient => f.write_str("broker candle fetch failed (transient)"),
            Self::BadRange => f.write_str("candle fetch range is degenerate or rejected"),
        }
    }
}

impl std::error::Error for CandleError {}

/// Keep only candles strictly newer than the watermark, in ascending time
/// order. The watermark is the open-time of the last candle the engine already
/// processed; `> watermark` (strict) guarantees a candle is never re-processed
/// across cron ticks.
///
/// Pure and broker-free so the watermark contract is unit-tested without a
/// live feed. Broker impls call this after dropping any still-forming bar, so
/// the result is "new, closed candles since the watermark, oldest first".
pub fn filter_new_candles(candles: Vec<Candle>, watermark: DateTime<Utc>) -> Vec<Candle> {
    let mut fresh: Vec<Candle> = candles.into_iter().filter(|c| c.time > watermark).collect();
    fresh.sort_by_key(|c| c.time);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn candle(time: &str, c: f64) -> Candle {
        Candle {
            time: ts(time),
            o: c,
            h: c,
            l: c,
            c,
        }
    }

    /// A bid/ask candle whose close spread is exactly `spread` (bid_c = mid,
    /// ask_c = mid + spread). Only the close books matter for `close_spread`.
    fn ba(time: &str, mid: f64, spread: f64) -> BidAskCandle {
        BidAskCandle {
            time: ts(time),
            o: mid,
            h: mid,
            l: mid,
            c: mid,
            bid_o: mid,
            bid_h: mid,
            bid_l: mid,
            bid_c: mid,
            ask_o: mid + spread,
            ask_h: mid + spread,
            ask_l: mid + spread,
            ask_c: mid + spread,
        }
    }

    #[test]
    fn trailing_mean_averages_last_window_bars() {
        // Five bars; window 3 → mean of the LAST three spreads (0.0002, 0.0002,
        // 0.0020) = 0.0008. The two earlier calm bars are outside the window.
        let bars = vec![
            ba("2026-07-05T18:00:00Z", 1.10, 0.0001),
            ba("2026-07-05T19:00:00Z", 1.10, 0.0001),
            ba("2026-07-05T20:00:00Z", 1.10, 0.0002),
            ba("2026-07-05T21:00:00Z", 1.10, 0.0002),
            ba("2026-07-05T22:00:00Z", 1.10, 0.0020),
        ];
        let (mean, n) = trailing_spread_mean(&bars, 3).expect("some");
        assert_eq!(n, 3);
        assert!((mean - 0.0008).abs() < 1e-9, "{mean}");
    }

    #[test]
    fn trailing_mean_window_larger_than_series_uses_all() {
        let bars = vec![
            ba("2026-07-05T20:00:00Z", 1.10, 0.0002),
            ba("2026-07-05T21:00:00Z", 1.10, 0.0004),
        ];
        let (mean, n) = trailing_spread_mean(&bars, 10).expect("some");
        assert_eq!(n, 2);
        assert!((mean - 0.0003).abs() < 1e-9, "{mean}");
    }

    #[test]
    fn trailing_mean_empty_is_none() {
        assert_eq!(trailing_spread_mean(&[], 5), None);
    }

    #[test]
    fn trailing_mean_damps_a_spiky_entry_bar() {
        // The bug this fixes: the ENTRY bar (last) is spiky (20 pips) while the
        // prior four are calm (1.5). Single-sample would floor off 0.0020; the
        // windowed mean over 5 is (4×0.00015 + 0.0020)/5 = 0.00052.
        let bars = vec![
            ba("2026-07-05T18:00:00Z", 1.10, 0.00015),
            ba("2026-07-05T19:00:00Z", 1.10, 0.00015),
            ba("2026-07-05T20:00:00Z", 1.10, 0.00015),
            ba("2026-07-05T21:00:00Z", 1.10, 0.00015),
            ba("2026-07-05T22:00:00Z", 1.10, 0.0020),
        ];
        let (mean, _) = trailing_spread_mean(&bars, 5).expect("some");
        assert!((mean - 0.00052).abs() < 1e-9, "{mean}");
    }

    #[test]
    fn seconds_per_granularity() {
        assert_eq!(Granularity::M1.seconds(), 60);
        assert_eq!(Granularity::M15.seconds(), 900);
        assert_eq!(Granularity::H1.seconds(), 3600);
        assert_eq!(Granularity::H4.seconds(), 14400);
        assert_eq!(Granularity::D1.seconds(), 86400);
    }

    #[test]
    fn filter_drops_at_and_before_watermark() {
        let wm = ts("2026-06-16T12:00:00Z");
        let got = filter_new_candles(
            vec![
                candle("2026-06-16T11:00:00Z", 1.0), // before → dropped
                candle("2026-06-16T12:00:00Z", 2.0), // == watermark → dropped (strict)
                candle("2026-06-16T13:00:00Z", 3.0), // after → kept
            ],
            wm,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].c, 3.0);
    }

    #[test]
    fn filter_sorts_ascending() {
        let wm = ts("2026-06-16T00:00:00Z");
        let got = filter_new_candles(
            vec![
                candle("2026-06-16T13:00:00Z", 3.0),
                candle("2026-06-16T11:00:00Z", 1.0),
                candle("2026-06-16T12:00:00Z", 2.0),
            ],
            wm,
        );
        let times: Vec<f64> = got.iter().map(|c| c.c).collect();
        assert_eq!(times, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn filter_empty_is_empty() {
        let got = filter_new_candles(vec![], ts("2026-06-16T00:00:00Z"));
        assert!(got.is_empty());
    }
}
