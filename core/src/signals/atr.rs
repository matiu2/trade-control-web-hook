//! Wilder ATR with the timeframe-dependent length from the Pine
//! `f_get_atr_length()` helper (`candle-signals-v2.pine` lines ~174-191).
//!
//! Pine's `ta.atr(len)` is the Wilder RMA of True Range. The first ATR value is
//! the simple mean of the first `len` true ranges; thereafter
//! `atr = (atr_prev * (len-1) + tr) / len`. True Range is
//! `max(high-low, |high-prevClose|, |low-prevClose|)`, with the first bar's TR
//! falling back to `high-low` (no prior close).

use crate::broker::{Candle, Granularity};

/// The ATR length for a timeframe, matching `f_get_atr_length()`. Keyed off the
/// bar length in minutes — the Pine uses `timeframe.in_seconds() / 60` and the
/// same cut-offs.
pub fn atr_length_for(granularity: Granularity) -> usize {
    let tf_mins = granularity.seconds() / 60;
    match tf_mins {
        m if m <= 15 => 96,    // 15m: 24*4 (one day)
        m if m <= 60 => 24,    // 1h: 24 (one day)
        m if m <= 240 => 36,   // 4h: 6*6 (6 days, 6 candles/day)
        m if m <= 1440 => 24,  // 1d: 6*4 (6 days, 4 weeks)
        m if m <= 10080 => 52, // 1w: 52 (one year)
        _ => 24,               // 1M: 24 (two years)
    }
}

/// True range of `cur` given the previous candle's close. The first bar (no
/// prior close) uses `high - low`.
fn true_range(cur: &Candle, prev_close: Option<f64>) -> f64 {
    let hl = cur.h - cur.l;
    match prev_close {
        None => hl,
        Some(pc) => hl.max((cur.h - pc).abs()).max((cur.l - pc).abs()),
    }
}

/// Wilder ATR over `candles` (ascending) at `length`, returning the ATR value
/// **as of the last candle** — the value Pine's `atr_value` holds on the bar
/// being evaluated. Returns `None` if there are fewer than `length` candles (the
/// warmup region, where Pine's `ta.atr` is `na`).
///
/// `length` must be ≥ 1; a zero length yields `None`.
pub fn wilder_atr(candles: &[Candle], length: usize) -> Option<f64> {
    if length == 0 || candles.len() < length {
        return None;
    }
    // True ranges; the first has no prior close.
    let mut trs = Vec::with_capacity(candles.len());
    let mut prev_close = None;
    for c in candles {
        trs.push(true_range(c, prev_close));
        prev_close = Some(c.c);
    }
    // Seed: simple mean of the first `length` TRs (Wilder's RMA seed, matching
    // Pine's ta.rma warmup).
    let seed: f64 = trs[..length].iter().sum::<f64>() / length as f64;
    let mut atr = seed;
    let len_f = length as f64;
    for tr in &trs[length..] {
        atr = (atr * (len_f - 1.0) + tr) / len_f;
    }
    Some(atr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn c(h: f64, l: f64, cl: f64) -> Candle {
        Candle {
            time: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            o: cl,
            h,
            l,
            c: cl,
        }
    }

    #[test]
    fn lengths_match_pine_cutoffs() {
        assert_eq!(atr_length_for(Granularity::M5), 96);
        assert_eq!(atr_length_for(Granularity::M15), 96);
        assert_eq!(atr_length_for(Granularity::H1), 24);
        assert_eq!(atr_length_for(Granularity::H4), 36);
        assert_eq!(atr_length_for(Granularity::D1), 24);
        assert_eq!(atr_length_for(Granularity::M1), 96);
    }

    #[test]
    fn warmup_region_is_none() {
        let candles = [c(1.1, 1.0, 1.05), c(1.2, 1.1, 1.15)];
        assert!(wilder_atr(&candles, 3).is_none());
        assert!(wilder_atr(&candles, 0).is_none());
    }

    #[test]
    fn constant_true_range_atr_equals_that_range() {
        // Every bar has the same TR (high-low = 0.1, no gaps), so the RMA of a
        // constant is that constant regardless of length / extra bars.
        let candles: Vec<Candle> = (0..10).map(|_| c(1.10, 1.00, 1.05)).collect();
        let atr = wilder_atr(&candles, 3).unwrap();
        assert!((atr - 0.10).abs() < 1e-9, "got {atr}");
    }

    #[test]
    fn seed_is_simple_mean_of_first_length_trs() {
        // length = 3, exactly 3 candles → ATR is the mean of the 3 TRs.
        // Bar0 TR = high-low = 0.10. Bar1 prevClose 1.05: TR = max(0.10,
        // |1.20-1.05|, |1.10-1.05|) = 0.15. Bar2 prevClose 1.15: TR =
        // max(0.10, |1.30-1.15|, |1.20-1.15|) = 0.15. Mean = (0.10+0.15+0.15)/3.
        let candles = [
            c(1.10, 1.00, 1.05),
            c(1.20, 1.10, 1.15),
            c(1.30, 1.20, 1.25),
        ];
        let atr = wilder_atr(&candles, 3).unwrap();
        assert!((atr - (0.10 + 0.15 + 0.15) / 3.0).abs() < 1e-9, "got {atr}");
    }
}
