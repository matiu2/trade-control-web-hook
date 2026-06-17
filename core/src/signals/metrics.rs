//! Per-candle derived quantities — the Pine "Common Calculations" block
//! (`candle-signals-v2.pine` lines ~193-210) ported verbatim.
//!
//! Every pattern test is phrased in terms of these, so computing them once per
//! candle keeps [`super::detect`] a direct transcription of the Pine booleans.

use crate::broker::Candle;

/// Derived geometry of a single candle. All fields are absolute MID prices
/// except [`close_position`] (a 0..=1 fraction) and the boolean flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandleMetrics {
    pub high: f64,
    pub low: f64,
    pub open: f64,
    pub close: f64,
    /// `high - low`.
    pub range: f64,
    /// `|close - open|`.
    pub body_size: f64,
    /// `max(open, close)`.
    pub body_top: f64,
    /// `min(open, close)`.
    pub body_bottom: f64,
    /// `high - body_top`.
    pub upper_wick: f64,
    /// `body_bottom - low`.
    pub lower_wick: f64,
    /// `high - range * 0.25` — the bottom of the top quartile.
    pub top_25: f64,
    /// `low + range * 0.25` — the top of the bottom quartile.
    pub bottom_25: f64,
    /// `low + range * 0.5`.
    pub midpoint: f64,
    /// `(close - low) / range`, or `None` for a zero-range candle.
    pub close_position: Option<f64>,
    /// `range > 0 && body_size > 0` — the Pine `valid_candle` flag. A doji
    /// (zero body) or a zero-range bar is not a valid signal candle.
    pub valid_candle: bool,
    /// `close > open`.
    pub is_bullish: bool,
    /// `close < open`.
    pub is_bearish: bool,
}

impl CandleMetrics {
    /// Compute the derived quantities for one candle.
    pub fn of(c: &Candle) -> Self {
        let range = c.h - c.l;
        let body_size = (c.c - c.o).abs();
        let body_top = c.o.max(c.c);
        let body_bottom = c.o.min(c.c);
        let close_position = if range > 0.0 {
            Some((c.c - c.l) / range)
        } else {
            None
        };
        Self {
            high: c.h,
            low: c.l,
            open: c.o,
            close: c.c,
            range,
            body_size,
            body_top,
            body_bottom,
            upper_wick: c.h - body_top,
            lower_wick: body_bottom - c.l,
            top_25: c.h - range * 0.25,
            bottom_25: c.l + range * 0.25,
            midpoint: c.l + range * 0.5,
            close_position,
            valid_candle: range > 0.0 && body_size > 0.0,
            is_bullish: c.c > c.o,
            is_bearish: c.c < c.o,
        }
    }

    /// `close_position >= 0.75` (Pine `close_in_top_25`). False for a zero-range
    /// candle (no defined close position).
    pub fn close_in_top_25(&self) -> bool {
        self.close_position.is_some_and(|p| p >= 0.75)
    }

    /// `close_position <= 0.25` (Pine `close_in_bottom_25`).
    pub fn close_in_bottom_25(&self) -> bool {
        self.close_position.is_some_and(|p| p <= 0.25)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn c(o: f64, h: f64, l: f64, cl: f64) -> Candle {
        Candle {
            time: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            o,
            h,
            l,
            c: cl,
        }
    }

    #[test]
    fn bullish_body_metrics() {
        // open 1.0, close 1.4, high 1.5, low 0.9.
        let m = CandleMetrics::of(&c(1.0, 1.5, 0.9, 1.4));
        assert!((m.range - 0.6).abs() < 1e-12);
        assert!((m.body_size - 0.4).abs() < 1e-12);
        assert!((m.body_top - 1.4).abs() < 1e-12);
        assert!((m.body_bottom - 1.0).abs() < 1e-12);
        assert!((m.upper_wick - 0.1).abs() < 1e-12);
        assert!((m.lower_wick - 0.1).abs() < 1e-12);
        assert!(m.is_bullish && !m.is_bearish);
        assert!(m.valid_candle);
    }

    #[test]
    fn quartile_bands_and_close_position() {
        // range 1.0 over [1.0, 2.0]: top_25 = 1.75, bottom_25 = 1.25, mid 1.5.
        let m = CandleMetrics::of(&c(1.1, 2.0, 1.0, 1.9));
        assert!((m.top_25 - 1.75).abs() < 1e-12);
        assert!((m.bottom_25 - 1.25).abs() < 1e-12);
        assert!((m.midpoint - 1.5).abs() < 1e-12);
        // close 1.9 → (1.9-1.0)/1.0 = 0.9 → in top 25.
        assert!((m.close_position.unwrap() - 0.9).abs() < 1e-12);
        assert!(m.close_in_top_25());
        assert!(!m.close_in_bottom_25());
    }

    #[test]
    fn doji_is_not_valid_and_zero_range_close_position_none() {
        // body_size 0 → not a valid signal candle even with range.
        let doji = CandleMetrics::of(&c(1.5, 2.0, 1.0, 1.5));
        assert!(!doji.valid_candle);
        // zero range → close_position None, neither top nor bottom 25.
        let flat = CandleMetrics::of(&c(1.0, 1.0, 1.0, 1.0));
        assert!(flat.close_position.is_none());
        assert!(!flat.close_in_top_25() && !flat.close_in_bottom_25());
        assert!(!flat.valid_candle);
    }
}
