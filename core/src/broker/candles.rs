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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub time: DateTime<Utc>,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
}

/// The candle timeframes the engine fetches. Deliberately a small closed set —
/// only the granularities trades are actually armed on — so every broker can
/// map it without an "unsupported" runtime branch leaking into the engine.
///
/// TradeNation natively serves minute / quarter (15m) / hour / day; M5 and H4
/// are aggregated upstream by `tradenation-api`. OANDA serves all six directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
