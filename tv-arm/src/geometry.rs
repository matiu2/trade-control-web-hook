//! Pure-math geometry helpers for reading prices off chart drawings.
//!
//! The fib helpers take an explicit `(head, neckline)` pair — resolved by
//! the caller from `Drawing::fib_head_neckline()`, which reads TradingView's
//! `reverse` flag to know which anchor is the `0`-reading (head). They
//! deliberately do **not** re-derive head/neckline from raw point order or
//! min/max: the raw order is unreliable (an operator who draws the fib
//! "neckline-first" still has the `0`-level at `points[1]`), and min/max
//! conflates the two whenever direction is what we're trying to establish.

use trade_control_conventions::Direction;

/// Return the take-profit price for a fib whose `(head, neckline)` have
/// already been resolved (via `Drawing::fib_head_neckline`).
///
/// TP is one full leg past the neckline — price reflected through it:
/// `TP = 2 × neckline − head`. Symmetric for long and short, so no
/// direction argument is needed. Returns `f64::NAN` if either input is
/// non-finite.
pub fn tp_price(head: f64, neckline: f64) -> f64 {
    if !head.is_finite() || !neckline.is_finite() {
        return f64::NAN;
    }
    2.0 * neckline - head
}

/// Return the pcl-exhausted veto price: 80% of the way from the **neckline**
/// toward the take-profit, for an already-resolved `(head, neckline)`.
///
/// On the operator's fib — drawn `head(0) → neckline(1)`, so the `2.0`
/// extension lands on TP by the reflection `TP = 2·neckline − head` — this
/// is exactly the **fib `1.8` level**. Beyond it the projected move is
/// essentially complete and the R:R for a fresh entry no longer justifies
/// opening the trade. The price sits *between* the neckline and the TP.
///
/// This anchors on the **neckline**, not the head↔neckline midpoint. It
/// deliberately matches the M/W `overshoot_level` (`mw_geometry.rs`), which
/// is already `neckline + 0.8·(TP − neckline)` (≡ its `180% of top→neckline`
/// leg) — so H&S and M/W now abort at the same fraction of the projected
/// move. (Was `midpoint + 0.8·(TP − midpoint)` ≈ fib `1.7`, ~one fib notch
/// shallower, until 2026-07-20.) Returns `f64::NAN` if either input is
/// non-finite.
pub fn pcl_exhausted_price(head: f64, neckline: f64) -> f64 {
    if !head.is_finite() || !neckline.is_finite() {
        return f64::NAN;
    }
    let tp = 2.0 * neckline - head;
    neckline + 0.8 * (tp - neckline)
}

/// Direction implied by an already-resolved fib `(head, neckline)` — the
/// authoritative source of trade direction.
///
/// The head is the fib's `0`-reading (resolved via the `reverse` flag, not
/// point order); the neckline is its `1`-level. So:
///
/// - head **above** neckline → the pattern points down → **short** (classic
///   H&S: the head is the peak).
/// - head **below** neckline → **long** (inverse H&S: the head is the trough).
///
/// This replaces reading direction off the `too-high`/`too-low` invalidation
/// label (a stale line from a different trade could flip it) *and* the earlier
/// point-order reading (unreliable — see the module docs; AUD/CAD 2026-07 had
/// its head at `points[1]`, so point-order armed a long instead of the correct
/// short). Returns `None` when the two levels are equal (a degenerate flat fib
/// carries no direction).
pub fn direction_from_head_neckline(head: f64, neckline: f64) -> Option<Direction> {
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

/// Is `price` inside the fib's head↔neckline band (inclusive)?
///
/// Used to reject an invalidation horizontal left over from a *different*
/// trade: a genuine `too-high`/`too-low` cap/floor for this setup lies
/// between the head and the neckline, so a line outside that band is stale.
/// `head`/`neckline` come from `Drawing::fib_head_neckline`. Returns `false`
/// when any input is non-finite.
pub fn price_within_fib_range(price: f64, head: f64, neckline: f64) -> bool {
    if !price.is_finite() || !head.is_finite() || !neckline.is_finite() {
        return false;
    }
    let (lo, hi) = (head.min(neckline), head.max(neckline));
    price >= lo && price <= hi
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

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values come from the Python tv_arm_hs.py
    // implementation on a worked example.
    //
    // Setup: bearish H&S, head at 1.20, neckline at 1.10.
    // Expected: TP = 2 × 1.10 − 1.20 = 1.00.
    // pcl-exhausted = neckline + 0.8 × (TP − neckline)   (fib 1.8)
    //               = 1.10 + 0.8 × (1.00 − 1.10)
    //               = 1.10 − 0.08
    //               = 1.02.
    #[test]
    fn tp_short() {
        // head = 1.20, neckline = 1.10.
        assert!((tp_price(1.20, 1.10) - 1.00).abs() < 1e-9);
    }

    #[test]
    fn tp_long() {
        // Mirror: head = 1.00, neckline = 1.10, TP = 1.20.
        assert!((tp_price(1.00, 1.10) - 1.20).abs() < 1e-9);
    }

    #[test]
    fn pcl_short() {
        // fib 1.8 = neckline + 0.8 × (TP − neckline) = 1.10 + 0.8 × (1.00 − 1.10) = 1.02.
        assert!((pcl_exhausted_price(1.20, 1.10) - 1.02).abs() < 1e-9);
    }

    #[test]
    fn pcl_long_mirrors_short() {
        // Mirror: head = 1.00, neckline = 1.10, TP = 1.20, pcl = 1.18.
        assert!((pcl_exhausted_price(1.00, 1.10) - 1.18).abs() < 1e-9);
    }

    #[test]
    fn pcl_equals_neckline_plus_80pct_of_neckline_to_tp() {
        // The pcl-exhausted level anchors on the NECKLINE (fib 1.8), matching
        // the M/W overshoot level — not the head↔neckline midpoint.
        let (head, neckline) = (8848.0996, 8813.5);
        let tp = 2.0 * neckline - head;
        let expected = neckline + 0.8 * (tp - neckline); // 8785.82
        assert!((pcl_exhausted_price(head, neckline) - expected).abs() < 1e-6);
        // And it is deeper (closer to TP) than the old midpoint formula.
        let old_midpoint = (head + neckline) / 2.0;
        let old = old_midpoint + 0.8 * (tp - old_midpoint); // 8789.28
        assert!(pcl_exhausted_price(head, neckline) < old);
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
    fn tp_and_pcl_nonfinite_input_is_nan() {
        assert!(tp_price(f64::NAN, 1.10).is_nan());
        assert!(pcl_exhausted_price(1.20, f64::NAN).is_nan());
    }

    #[test]
    fn direction_head_above_neckline_is_short() {
        // H&S: head (0-reading) at 1.20 above neckline 1.10 → short.
        assert_eq!(
            direction_from_head_neckline(1.20, 1.10),
            Some(Direction::Short)
        );
    }

    #[test]
    fn direction_head_below_neckline_is_long() {
        // iH&S: head at 1.00 below neckline 1.10 → long.
        assert_eq!(
            direction_from_head_neckline(1.00, 1.10),
            Some(Direction::Long)
        );
    }

    #[test]
    fn direction_flat_or_nonfinite_is_none() {
        assert_eq!(direction_from_head_neckline(1.10, 1.10), None);
        assert_eq!(direction_from_head_neckline(f64::NAN, 1.10), None);
    }

    #[test]
    fn price_within_fib_range_inclusive() {
        // Fib head 1.20, neckline 1.10. A too-high cap for this setup sits
        // inside. Order of head/neckline doesn't matter (min/max internally).
        assert!(price_within_fib_range(1.15, 1.20, 1.10));
        assert!(price_within_fib_range(1.10, 1.20, 1.10)); // inclusive edge
        assert!(price_within_fib_range(1.20, 1.20, 1.10)); // inclusive edge
        assert!(price_within_fib_range(1.15, 1.10, 1.20)); // reversed args
        // A stale line from a different trade sits outside → rejected.
        assert!(!price_within_fib_range(1.25, 1.20, 1.10));
        assert!(!price_within_fib_range(1.05, 1.20, 1.10));
        // Non-finite inputs are rejected, not accepted.
        assert!(!price_within_fib_range(f64::NAN, 1.20, 1.10));
        assert!(!price_within_fib_range(1.15, f64::NAN, 1.10));
    }
}
