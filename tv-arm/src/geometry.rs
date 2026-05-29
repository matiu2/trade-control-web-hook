//! Pure-math geometry helpers for reading prices off chart drawings.
//!
//! Drawings get represented to these functions as plain `&[f64]`
//! price slices — keeps this module decoupled from the
//! tv-mcp-flavoured `Drawing` struct that lives next door.

use trade_control_conventions::Direction;

/// Return the take-profit price implied by a fib retracement.
///
/// Convention: the operator draws the fib spanning the
/// head→neckline (or shoulder→neckline) move. TP is one full leg
/// further past the neckline (price reflected through the
/// neckline).
///
/// ```text
///   neckline = endpoint nearest the candle range
///              (highest for long, lowest for short)
///   head     = the other endpoint
///   TP       = 2 × neckline − head
/// ```
///
/// Returns `f64::NAN` when fewer than two prices are supplied.
pub fn tp_price_from_fib(prices: &[f64], direction: Direction) -> f64 {
    let (head, neckline) = match anchors(prices, direction) {
        Some(pair) => pair,
        None => return f64::NAN,
    };
    2.0 * neckline - head
}

/// Return the pcl-exhausted veto price: 80% of the way from the
/// fib's midpoint toward the take-profit.
///
/// Beyond this level the projected move is essentially complete and
/// the R:R for a fresh entry no longer justifies opening the trade.
/// The price sits *between* the neckline and the TP — below the
/// neckline for a short trade, above for a long.
///
/// Returns `f64::NAN` when fewer than two prices are supplied.
pub fn pcl_exhausted_price_from_fib(prices: &[f64], direction: Direction) -> f64 {
    let (head, neckline) = match anchors(prices, direction) {
        Some(pair) => pair,
        None => return f64::NAN,
    };
    let midpoint = (head + neckline) / 2.0;
    let tp = 2.0 * neckline - head;
    midpoint + 0.8 * (tp - midpoint)
}

/// Single horizontal-line price (the only point's price).
///
/// Returns `f64::NAN` when the slice is empty.
pub fn horizontal_price(prices: &[f64]) -> f64 {
    prices.first().copied().unwrap_or(f64::NAN)
}

/// Mean price across all anchor points — used for trendlines where
/// the operator-drawn line is at a roughly-constant level.
///
/// Returns `f64::NAN` when the slice is empty.
pub fn line_mean_price(prices: &[f64]) -> f64 {
    if prices.is_empty() {
        return f64::NAN;
    }
    let sum: f64 = prices.iter().copied().sum();
    sum / prices.len() as f64
}

fn anchors(prices: &[f64], direction: Direction) -> Option<(f64, f64)> {
    if prices.len() < 2 {
        return None;
    }
    let min = prices.iter().copied().fold(f64::INFINITY, f64::min);
    let max = prices.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(match direction {
        Direction::Long => (min, max),  // head = min, neckline = max
        Direction::Short => (max, min), // head = max, neckline = min
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values come from the Python tv_arm_hs.py
    // implementation on a worked example.
    //
    // Setup: bearish H&S, head at 1.20, neckline at 1.10.
    // Expected: TP = 2 × 1.10 − 1.20 = 1.00.
    // pcl-exhausted = midpoint + 0.8 × (TP − midpoint)
    //               = 1.15 + 0.8 × (1.00 − 1.15)
    //               = 1.15 − 0.12
    //               = 1.03.
    #[test]
    fn tp_short() {
        let tp = tp_price_from_fib(&[1.20, 1.10], Direction::Short);
        assert!((tp - 1.00).abs() < 1e-9, "tp = {tp}");
    }

    #[test]
    fn tp_long() {
        // Mirror: head = 1.00, neckline = 1.10, TP = 1.20.
        let tp = tp_price_from_fib(&[1.00, 1.10], Direction::Long);
        assert!((tp - 1.20).abs() < 1e-9, "tp = {tp}");
    }

    #[test]
    fn pcl_short() {
        let pcl = pcl_exhausted_price_from_fib(&[1.20, 1.10], Direction::Short);
        assert!((pcl - 1.03).abs() < 1e-9, "pcl = {pcl}");
    }

    #[test]
    fn pcl_long_mirrors_short() {
        // Mirror: head = 1.00, neckline = 1.10, pcl = 1.17.
        let pcl = pcl_exhausted_price_from_fib(&[1.00, 1.10], Direction::Long);
        assert!((pcl - 1.17).abs() < 1e-9, "pcl = {pcl}");
    }

    #[test]
    fn tp_unordered_input() {
        // The function takes min/max so input order doesn't matter.
        let a = tp_price_from_fib(&[1.10, 1.20], Direction::Short);
        let b = tp_price_from_fib(&[1.20, 1.10], Direction::Short);
        assert_eq!(a, b);
    }

    #[test]
    fn horizontal_uses_first_price() {
        assert_eq!(horizontal_price(&[2.5]), 2.5);
    }

    #[test]
    fn horizontal_empty_is_nan() {
        assert!(horizontal_price(&[]).is_nan());
    }

    #[test]
    fn line_mean_two_points() {
        assert!((line_mean_price(&[1.0, 2.0]) - 1.5).abs() < 1e-12);
    }

    #[test]
    fn line_mean_empty_is_nan() {
        assert!(line_mean_price(&[]).is_nan());
    }

    #[test]
    fn tp_insufficient_input_is_nan() {
        assert!(tp_price_from_fib(&[], Direction::Long).is_nan());
        assert!(tp_price_from_fib(&[1.0], Direction::Long).is_nan());
    }

    #[test]
    fn pcl_insufficient_input_is_nan() {
        assert!(pcl_exhausted_price_from_fib(&[], Direction::Long).is_nan());
        assert!(pcl_exhausted_price_from_fib(&[1.0], Direction::Long).is_nan());
    }
}
