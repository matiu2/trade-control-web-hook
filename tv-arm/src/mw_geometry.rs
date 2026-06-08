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
//!
//! ## Direction is geometry, not a label
//!
//! The TradingView path tool has **no text property**, so M/W can't be
//! labeled `m`/`w` like the H&S drawings. Direction is read off the
//! first leg's sign instead (see [`mw_direction_from_anchors`]): the
//! runup runs A→B, so `A` above `B` is a down-then-up pullback → a W
//! (double-bottom, long); `A` below `B` is an up-then-down pullback → an
//! M (double-top, short).

use color_eyre::eyre::{Result, eyre};
use trade_control_conventions::Direction;

/// Direction implied by the first leg (A→B) of an M/W path.
///
/// The runup starts at `A` and runs to the first peak/trough `B`:
///
/// - `A` **above** `B` (price fell A→B) → the first leg is a *trough*
///   leg → **W** (double-bottom) → [`Direction::Long`].
/// - `A` **below** `B` (price rose A→B) → the first leg is a *peak*
///   leg → **M** (double-top) → [`Direction::Short`].
///
/// `neckline` (C) is unused for the direction itself — the structure
/// gate ([`check_mw_structure`]) is what validates C sits sensibly
/// between B and the runup. A degenerate `A == B` first leg has no sign
/// and returns `None`.
pub fn mw_direction_from_anchors(runup_start: f64, first_point: f64) -> Option<Direction> {
    if runup_start > first_point {
        Some(Direction::Long) // W: fell into the first trough
    } else if runup_start < first_point {
        Some(Direction::Short) // M: rose into the first peak
    } else {
        None
    }
}

/// Structure gate: the runup leg (A→B) must be **longer by price** than
/// the retrace leg (B→C). A retrace at least as deep as the runup isn't
/// an M/W reversal — it's noise (or the anchors are mis-ordered).
///
/// Hard-errors with all three anchors and both leg lengths so a
/// fat-fingered path is obvious in the operator's terminal. (The
/// stricter `< 0.40` / `<= 0.50` retrace-% ceiling is enforced
/// separately by the pipeline via [`neckline_retrace_pct`]; this gate is
/// just the coarse "is this even an M/W shape" sanity check.)
pub fn check_mw_structure(runup_start: f64, first_point: f64, neckline: f64) -> Result<()> {
    let runup_leg = (first_point - runup_start).abs();
    let retrace_leg = (first_point - neckline).abs();
    if runup_leg > retrace_leg {
        return Ok(());
    }
    Err(eyre!(
        "M/W path structure invalid: runup leg (A→B) must be longer than the retrace leg (B→C).\n  \
         A (runup start) = {runup_start}\n  B (first point)  = {first_point}\n  \
         C (neckline)    = {neckline}\n  runup leg |A→B| = {runup_leg}\n  \
         retrace leg |B→C| = {retrace_leg}"
    ))
}

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

    #[test]
    fn direction_m_is_short() {
        // A below B (price rose into the first peak) → M / short.
        assert_eq!(mw_direction_from_anchors(M_A, M_B), Some(Direction::Short));
    }

    #[test]
    fn direction_w_is_long() {
        // A above B (price fell into the first trough) → W / long.
        assert_eq!(mw_direction_from_anchors(W_A, W_B), Some(Direction::Long));
    }

    #[test]
    fn direction_flat_first_leg_is_none() {
        assert_eq!(mw_direction_from_anchors(1.1000, 1.1000), None);
    }

    #[test]
    fn structure_accepts_worked_examples() {
        // Both worked examples: runup 0.0200 > retrace 0.0080.
        check_mw_structure(M_A, M_B, M_C).expect("M structure ok");
        check_mw_structure(W_A, W_B, W_C).expect("W structure ok");
    }

    #[test]
    fn structure_rejects_retrace_deeper_than_runup() {
        // runup |A→B| = 0.0080, retrace |B→C| = 0.0200 → retrace deeper.
        let err = check_mw_structure(1.1120, 1.1200, 1.1000).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("runup leg"), "msg = {msg}");
        assert!(msg.contains("1.112"), "msg = {msg}"); // A printed
        assert!(msg.contains("0.008"), "msg = {msg}"); // runup leg printed
    }

    #[test]
    fn structure_rejects_equal_legs() {
        // Equal legs is not "longer" — reject.
        let err = check_mw_structure(1.1000, 1.1200, 1.1000);
        assert!(err.is_err());
    }
}
