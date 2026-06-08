//! Pure-math geometry helpers for M / W (double-top / double-bottom)
//! reversal setups.
//!
//! The operator draws a 3-anchor PATH on the chart:
//!
//! - `A` — **runup start** (the base the first leg ran up/down from),
//! - `B` — **first point** (the first peak for an M, first trough for a
//!   W),
//! - `C` — **neckline** (the retracement level the price pulled back to
//!   between the two peaks).
//!
//! All prices here are **MID** prices, exactly as read off the chart.
//! The mid→bid/ask spread correction happens later, in the worker's
//! `mw_resolution` — these functions are spread-agnostic geometry only.
//!
//! Like [`crate::geometry`], everything is plain scalar `f64` math so
//! the module stays decoupled from the tv-mcp `Drawing` struct. Anchor
//! *order* is load-bearing for M/W (unlike the min/max H&S helpers), so
//! each anchor is a named parameter rather than a slice.

/// Neckline retracement depth as a fraction of the runup depth.
///
/// ```text
///   runup_depth   = |first_point − runup_start|   (A→B leg)
///   retrace_depth = |first_point − neckline|      (B→C pullback)
///   pct           = retrace_depth / runup_depth
/// ```
///
/// This is the gate input: tv-arm hard-errors at `>= 0.40` (or `> 0.50`
/// with `--allow-50-pct-m-trades`). Direction-agnostic — uses absolute
/// distances, so an M (down-retrace from a high) and a W (up-retrace
/// from a low) compute identically.
///
/// Returns [`f64::NAN`] when `runup_depth == 0` (degenerate A==B path).
pub fn neckline_retrace_pct(runup_start: f64, first_point: f64, neckline: f64) -> f64 {
    let runup_depth = (first_point - runup_start).abs();
    if runup_depth == 0.0 {
        return f64::NAN;
    }
    let retrace_depth = (first_point - neckline).abs();
    retrace_depth / runup_depth
}

/// Cancel level: the 1.3 extension of the neckline→first-point leg,
/// measured *past* the first point.
///
/// ```text
///   level = neckline + 1.3 × (first_point − neckline)
/// ```
///
/// For an M (short) `first_point > neckline`, so the level sits *above*
/// B; for a W (long) `first_point < neckline`, so it sits *below* B.
/// The sign falls out of the signed `(first_point − neckline)` term —
/// no direction parameter needed.
///
/// Price crossing this level cancels the pending stop and disarms
/// future entries (rule 5). It is *also* the two-peaks alignment
/// ceiling: the second peak must stay within 1.3 of the first, so this
/// same level enforces alignment implicitly.
pub fn cancel_level(first_point: f64, neckline: f64) -> f64 {
    neckline + 1.3 * (first_point - neckline)
}

/// Abort level: the neckline itself.
///
/// A candle closing back through here means the breakout failed (rule
/// 6). Named (rather than inlining `neckline`) so the intent reads
/// self-documenting and a future body/wick tweak has one home.
pub fn abort_level(neckline: f64) -> f64 {
    neckline
}

#[cfg(test)]
mod tests {
    use super::*;

    // Worked example — M (short):
    //   A = 1.1000 (runup start)
    //   B = 1.1200 (first peak)
    //   C = 1.1120 (neckline)
    //   runup_depth   = |1.1200 − 1.1000| = 0.0200
    //   retrace_depth = |1.1200 − 1.1120| = 0.0080
    //   pct           = 0.0080 / 0.0200 = 0.40
    //   cancel        = 1.1120 + 1.3 × (1.1200 − 1.1120) = 1.1224
    //   abort         = 1.1120
    const M_A: f64 = 1.1000;
    const M_B: f64 = 1.1200;
    const M_C: f64 = 1.1120;

    // Worked example — W (long), the vertical mirror:
    //   A = 1.1200, B = 1.1000, C = 1.1080
    //   pct    = |1.1000 − 1.1080| / |1.1000 − 1.1200| = 0.0080/0.0200 = 0.40
    //   cancel = 1.1080 + 1.3 × (1.1000 − 1.1080) = 1.1080 − 0.0104 = 1.0976
    const W_A: f64 = 1.1200;
    const W_B: f64 = 1.1000;
    const W_C: f64 = 1.1080;

    #[test]
    fn retrace_pct_m_worked_example() {
        let pct = neckline_retrace_pct(M_A, M_B, M_C);
        assert!((pct - 0.40).abs() < 1e-9, "pct = {pct}");
    }

    #[test]
    fn retrace_pct_w_mirrors_m() {
        let pct = neckline_retrace_pct(W_A, W_B, W_C);
        assert!((pct - 0.40).abs() < 1e-9, "pct = {pct}");
    }

    #[test]
    fn retrace_pct_boundaries() {
        // Build setups that land exactly on the gate boundaries the
        // pipeline checks: 0.399, 0.40, 0.499, 0.50, 0.501.
        // runup fixed at 0.0200; vary retrace depth.
        let runup_start = 1.0000;
        let first_point = 1.0200; // runup_depth = 0.0200
        let pct_for = |retrace: f64| {
            let neckline = first_point - retrace;
            neckline_retrace_pct(runup_start, first_point, neckline)
        };
        assert!((pct_for(0.00798) - 0.399).abs() < 1e-9);
        assert!((pct_for(0.00800) - 0.400).abs() < 1e-9);
        assert!((pct_for(0.00998) - 0.499).abs() < 1e-9);
        assert!((pct_for(0.01000) - 0.500).abs() < 1e-9);
        assert!((pct_for(0.01002) - 0.501).abs() < 1e-9);
    }

    #[test]
    fn retrace_pct_zero_runup_is_nan() {
        // Degenerate A == B path: no runup to measure against.
        assert!(neckline_retrace_pct(1.1000, 1.1000, 1.0950).is_nan());
    }

    #[test]
    fn cancel_level_m_worked_example() {
        let level = cancel_level(M_B, M_C);
        assert!((level - 1.1224).abs() < 1e-9, "cancel = {level}");
        // M is short: cancel sits *above* the first peak.
        assert!(level > M_B);
    }

    #[test]
    fn cancel_level_w_worked_example() {
        let level = cancel_level(W_B, W_C);
        assert!((level - 1.0976).abs() < 1e-9, "cancel = {level}");
        // W is long: cancel sits *below* the first trough.
        assert!(level < W_B);
    }

    #[test]
    fn cancel_level_is_1_3_extension() {
        // The cancel level is exactly 1.3× the neckline→B leg past the
        // neckline, i.e. 0.3 of the leg beyond B.
        let leg = M_B - M_C;
        assert!((cancel_level(M_B, M_C) - (M_B + 0.3 * leg)).abs() < 1e-9);
    }

    #[test]
    fn abort_level_is_neckline() {
        assert_eq!(abort_level(M_C), M_C);
        assert_eq!(abort_level(W_C), W_C);
    }
}
