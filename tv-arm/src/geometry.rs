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

/// Direction implied by the fib's *own* geometry, read from its ordered
/// anchors — the authoritative source of trade direction.
///
/// The operator draws the fib spanning **head → neckline**, clicking the
/// head (the fib's `0` reading) first. So `prices[0]` is the head and
/// `prices[1]` the neckline:
///
/// - head **above** neckline (`prices[0] > prices[1]`) → the pattern points
///   down → **short** (a classic H&S: head is the peak).
/// - head **below** neckline → **long** (inverse H&S: head is the trough).
///
/// This replaces reading direction off the `too-high`/`too-low` invalidation
/// label, which could be a stale line from a different trade and silently flip
/// the trade direction. Returns `None` when fewer than two prices are supplied
/// or the two anchors are equal (a degenerate flat fib carries no direction).
pub fn direction_from_fib(prices: &[f64]) -> Option<Direction> {
    let head = *prices.first()?;
    let neckline = *prices.get(1)?;
    if !head.is_finite() || !neckline.is_finite() {
        return None;
    }
    if head > neckline {
        Some(Direction::Short)
    } else if head < neckline {
        Some(Direction::Long)
    } else {
        None
    }
}

/// The inclusive price range spanned by the fib's two anchors,
/// `[min, max]` — the head↔neckline band. An invalidation horizontal for
/// *this* setup must sit inside this band (see [`price_within_fib_range`]);
/// a line outside it belongs to a different, larger pattern.
///
/// Returns `None` when fewer than two finite prices are supplied.
pub fn fib_range(prices: &[f64]) -> Option<(f64, f64)> {
    let a = *prices.first()?;
    let b = *prices.get(1)?;
    if !a.is_finite() || !b.is_finite() {
        return None;
    }
    Some((a.min(b), a.max(b)))
}

/// Is `price` inside the fib's head↔neckline band (inclusive)?
///
/// Used to reject an invalidation horizontal left over from a *different*
/// trade: a genuine `too-high`/`too-low` cap/floor for this setup lies
/// between the head and the neckline, so a line outside that band is stale.
/// Returns `false` when the fib range can't be computed or `price` is
/// non-finite.
pub fn price_within_fib_range(price: f64, fib_prices: &[f64]) -> bool {
    match fib_range(fib_prices) {
        Some((lo, hi)) => price.is_finite() && price >= lo && price <= hi,
        None => false,
    }
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

    #[test]
    fn direction_from_fib_head_above_neckline_is_short() {
        // H&S: operator clicks the head (peak, 0-reading) first at 1.20,
        // then the neckline at 1.10. Head above neckline → short.
        assert_eq!(direction_from_fib(&[1.20, 1.10]), Some(Direction::Short));
    }

    #[test]
    fn direction_from_fib_head_below_neckline_is_long() {
        // iH&S: head (trough, 0-reading) at 1.00, neckline at 1.10.
        // Head below neckline → long.
        assert_eq!(direction_from_fib(&[1.00, 1.10]), Some(Direction::Long));
    }

    #[test]
    fn direction_from_fib_flat_or_short_is_none() {
        assert_eq!(direction_from_fib(&[1.10, 1.10]), None);
        assert_eq!(direction_from_fib(&[1.10]), None);
        assert_eq!(direction_from_fib(&[]), None);
        assert_eq!(direction_from_fib(&[f64::NAN, 1.10]), None);
    }

    #[test]
    fn fib_range_is_min_max_order_independent() {
        assert_eq!(fib_range(&[1.20, 1.10]), Some((1.10, 1.20)));
        assert_eq!(fib_range(&[1.00, 1.10]), Some((1.00, 1.10)));
        assert_eq!(fib_range(&[1.10]), None);
        assert_eq!(fib_range(&[f64::NAN, 1.10]), None);
    }

    #[test]
    fn price_within_fib_range_inclusive() {
        // Fib spans 1.10..1.20. A too-high cap for this setup sits inside.
        assert!(price_within_fib_range(1.15, &[1.20, 1.10]));
        assert!(price_within_fib_range(1.10, &[1.20, 1.10])); // inclusive edge
        assert!(price_within_fib_range(1.20, &[1.20, 1.10])); // inclusive edge
        // A stale line from a different trade sits outside → rejected.
        assert!(!price_within_fib_range(1.25, &[1.20, 1.10]));
        assert!(!price_within_fib_range(1.05, &[1.20, 1.10]));
        // Degenerate inputs are rejected, not accepted.
        assert!(!price_within_fib_range(1.15, &[1.20]));
        assert!(!price_within_fib_range(f64::NAN, &[1.20, 1.10]));
    }
}
