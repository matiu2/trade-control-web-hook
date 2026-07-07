//! Hard server-side floor: an entry's stop-loss distance must be at least
//! [`SL_MIN_SPREAD_MULTIPLE`]× the live bid-ask spread.
//!
//! # Why
//!
//! A stop placed only a pip or two beyond the spread is dominated by
//! transaction cost: the spread alone can stop the trade out before any real
//! adverse move, and the reward:risk maths is meaningless when the "risk" is
//! mostly the cost of crossing the book. Requiring `sl_distance ≥ 10 × spread`
//! keeps the stop a genuine market level rather than spread noise.
//!
//! # Where this is enforced
//!
//! The decision is **pure** (a function of two price-unit distances) so the
//! same rule runs in every place that has both numbers:
//!
//! - **Worker entry gate** (`run_enter`, real-time hard limit) — `sl_distance`
//!   from the resolved geometry, spread from a live `get_quote`.
//! - **tv-arm arm time** and **`trade-control build-trade`** — caught before the
//!   intent is ever signed, using the live broker spread read.
//!
//! Like [`super::MIN_R_FLOOR`] this is a **fixed constant**: it cannot be
//! weakened per-intent. Bump the constant here and every consumer follows.

/// The minimum ratio of stop-loss distance to live bid-ask spread. An entry
/// whose `sl_distance` is below `SL_MIN_SPREAD_MULTIPLE × spread` is rejected
/// — unless the SL can be *widened* to satisfy the floor and the trade still
/// clears its R-floor (see [`widen_sl_to_spread_floor`]).
pub const SL_MIN_SPREAD_MULTIPLE: f64 = 10.0;

/// The multiple a too-tight stop is **widened to** when salvaging an entry.
/// Equal to [`SL_MIN_SPREAD_MULTIPLE`]: a salvaged stop is moved to exactly the
/// floor, no further. This lands on the `<` boundary of
/// [`sl_spread_floor_violation`] — `sl_distance == 10 × spread` is **not** a
/// violation (the check is strict `<`), so a stop widened to precisely the
/// floor passes. The widened distance is computed with the same `10.0 × spread`
/// multiplication as the check, so the two are bit-identical and there is no
/// floating-point boundary risk.
pub const SL_WIDEN_SPREAD_MULTIPLE: f64 = SL_MIN_SPREAD_MULTIPLE;

/// Default number of trailing candles the entry SL-spread floor averages the
/// bid-ask spread over, when a trade doesn't specify its own
/// [`super::Intent::spread_window`].
///
/// # Why a window at all
///
/// The floor multiplies a spread by [`SL_MIN_SPREAD_MULTIPLE`]. Sizing that off
/// a **single** sample — one live quote (worker) or one bar's `ask − bid`
/// (replay) — lets a spiky entry candle (high volatility → a momentarily wide
/// spread) blow the floor out and widen the stop far past what the instrument's
/// normal spread warrants. Averaging over the last N candles dilutes a lone
/// spike while still tracking a genuinely-elevated spread regime. `5` is short
/// enough to stay "recent" yet long enough to damp a single bar.
pub const DEFAULT_SPREAD_WINDOW: u32 = 5;

/// Arithmetic mean of the finite, strictly-positive spreads in `spreads`, or
/// `None` when none qualify.
///
/// Each element is a raw-price `ask − bid` for one candle. Non-finite (NaN /
/// ∞), zero, and negative (crossed-book) samples are **dropped** before the
/// mean — the same "a degenerate spread is unjudgeable" discipline
/// [`sl_spread_floor_violation`] applies to a single spread, lifted to the
/// window. An all-degenerate (or empty) window yields `None`, which the callers
/// treat as "no usable spread read" and fall through to their fail-open path
/// (worker: the live `get_quote`; replay: no floor). Averaging only the good
/// samples means a single NaN bar can't poison an otherwise-clean window.
pub fn mean_spread(spreads: &[f64]) -> Option<f64> {
    let (sum, n) = spreads
        .iter()
        .filter(|s| s.is_finite() && **s > 0.0)
        .fold((0.0, 0u32), |(sum, n), s| (sum + s, n + 1));
    if n == 0 {
        return None;
    }
    Some(sum / f64::from(n))
}

/// Does this entry violate the SL-vs-spread floor?
///
/// Both arguments are in **price units** (the same units the broker quotes in),
/// so callers never have to agree on a pip size — `sl_distance` is
/// `|entry_price − stop_loss|` and `spread_price` is `ask − bid`.
///
/// Returns `true` ⇒ **REJECT** when `sl_distance < SL_MIN_SPREAD_MULTIPLE ×
/// spread_price`.
///
/// A non-finite or non-positive `spread_price` (market closed, stale/crossed
/// feed) yields `false` — this rule can't judge a degenerate spread, and
/// fabricating a rejection here would conflate "stop too tight" with "no usable
/// quote". The live gates handle a missing/degenerate quote on their own (the
/// worker fails open on a quote error; tv-arm hard-errors on a non-positive
/// spread before this is ever called). A non-finite `sl_distance` is also
/// treated as no-violation for the same reason — upstream resolve already
/// rejects degenerate geometry.
pub fn sl_spread_floor_violation(sl_distance: f64, spread_price: f64) -> bool {
    if !spread_price.is_finite() || spread_price <= 0.0 || !sl_distance.is_finite() {
        return false;
    }
    sl_distance < SL_MIN_SPREAD_MULTIPLE * spread_price
}

/// Outcome of trying to salvage a too-tight stop by widening it to
/// [`SL_WIDEN_SPREAD_MULTIPLE`]× the spread. See [`widen_sl_to_spread_floor`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlWiden {
    /// The stop already cleared the floor (or the spread was degenerate /
    /// unjudgeable) — leave the geometry exactly as-is. The caller proceeds
    /// with the original `stop_loss`; **no widen happened**.
    Unchanged,
    /// The stop was too tight but widening it to `SL_WIDEN_SPREAD_MULTIPLE ×
    /// spread` keeps the trade at or above its R-floor. The caller should
    /// replace the stop with [`Self::Widened::new_stop_loss`] and place the
    /// trade. `new_sl_distance` and `new_r` are carried for the log line.
    Widened {
        new_stop_loss: f64,
        new_sl_distance: f64,
        new_r: f64,
    },
    /// The stop was too tight and widening it to the floor would drop the
    /// trade below its R-floor — there is no legal stop. The caller rejects
    /// the entry (same outcome as before the widen feature existed). The
    /// fields drive the operator-facing reject message.
    Reject {
        /// The stop **price** the widen would have moved to (`entry ±
        /// widened_sl_distance`, on the stop's existing side) — surfaced so the
        /// reject message can show the concrete level, not just the distance.
        widened_stop_loss: f64,
        widened_sl_distance: f64,
        r_at_widen: f64,
        min_r: f64,
    },
}

/// Salvage an entry whose stop-loss sits too close to the spread by *widening*
/// the stop — never tightening it — to [`SL_WIDEN_SPREAD_MULTIPLE`]× the live
/// spread, then re-checking the trade still clears its R-floor.
///
/// # Arguments (all in **price units**)
///
/// - `entry_price` — the resolved entry / trigger reference price.
/// - `stop_loss` — the resolved stop-loss price.
/// - `take_profit` — the resolved take-profit price.
/// - `spread_price` — live `ask − bid`.
/// - `min_r` — the trade's effective R-floor (its `min_r` override, or the
///   default [`super::MIN_R_FLOOR`]). The widened trade must still clear this.
///
/// # Behaviour
///
/// - If the stop already clears the floor, or the spread is non-finite /
///   non-positive (market closed, crossed feed), or the geometry is
///   degenerate, returns [`SlWiden::Unchanged`] — this matches the
///   fail-open discipline of [`sl_spread_floor_violation`] (a degenerate
///   spread is not a violation, so there is nothing to widen).
/// - Otherwise the stop is moved to `entry ± SL_WIDEN_SPREAD_MULTIPLE ×
///   spread` (away from entry, on the stop's existing side). If the resulting
///   R `= tp_distance / new_sl_distance` is `>= min_r`, returns
///   [`SlWiden::Widened`]; if not, [`SlWiden::Reject`].
///
/// The widen is **direction-agnostic**: the stop's side relative to entry is
/// preserved (above for a short, below for a long). It deliberately does not
/// clamp the widened stop to any pattern-invalidation level — pushing the stop
/// past `too-high` / `too-low` is acceptable (the continuous entry-level vetos
/// abort the trade independently if price actually reaches invalidation).
pub fn widen_sl_to_spread_floor(
    entry_price: f64,
    stop_loss: f64,
    take_profit: f64,
    spread_price: f64,
    min_r: f64,
) -> SlWiden {
    let sl_distance = (entry_price - stop_loss).abs();
    // Nothing to do when the floor isn't violated, or when the spread is
    // unjudgeable / the geometry degenerate (same guards as the violation
    // check — never fabricate a widen from a bad quote).
    if !sl_spread_floor_violation(sl_distance, spread_price) {
        return SlWiden::Unchanged;
    }
    if !entry_price.is_finite() || !take_profit.is_finite() || !min_r.is_finite() {
        return SlWiden::Unchanged;
    }

    let widened_distance = SL_WIDEN_SPREAD_MULTIPLE * spread_price;
    // Preserve the stop's side: a short has SL above entry, a long below.
    let new_stop_loss = if stop_loss >= entry_price {
        entry_price + widened_distance
    } else {
        entry_price - widened_distance
    };
    let tp_distance = (take_profit - entry_price).abs();
    let new_r = if widened_distance > 0.0 {
        tp_distance / widened_distance
    } else {
        f64::NAN
    };

    if new_r.is_finite() && new_r >= min_r {
        SlWiden::Widened {
            new_stop_loss,
            new_sl_distance: widened_distance,
            new_r,
        }
    } else {
        SlWiden::Reject {
            widened_stop_loss: new_stop_loss,
            widened_sl_distance: widened_distance,
            r_at_widen: new_r,
            min_r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_floor_is_violation() {
        // spread 0.0001 → floor 0.0010. SL distance 0.0009 < floor → reject.
        assert!(sl_spread_floor_violation(0.0009, 0.0001));
    }

    #[test]
    fn at_floor_is_allowed() {
        // Exactly 10× is permissive (strict `<`), mirroring the spread-blackout
        // boundary convention.
        assert!(!sl_spread_floor_violation(0.0010, 0.0001));
    }

    #[test]
    fn above_floor_is_allowed() {
        // A normal ~22-pip stop against a 1.5-pip spread.
        assert!(!sl_spread_floor_violation(0.0022, 0.00015));
    }

    #[test]
    fn degenerate_spread_is_not_a_violation() {
        assert!(!sl_spread_floor_violation(0.0001, 0.0)); // closed market
        assert!(!sl_spread_floor_violation(0.0001, -0.0002)); // crossed feed
        assert!(!sl_spread_floor_violation(0.0001, f64::NAN));
    }

    #[test]
    fn nonfinite_sl_distance_is_not_a_violation() {
        assert!(!sl_spread_floor_violation(f64::NAN, 0.0001));
        assert!(!sl_spread_floor_violation(f64::INFINITY, 0.0001));
    }

    // ---- widen_sl_to_spread_floor ------------------------------------------

    #[test]
    fn already_clears_floor_is_unchanged() {
        // SL distance 0.0022 against a 0.00015 spread → floor 0.0015, clears.
        let out = widen_sl_to_spread_floor(1.1000, 1.0978, 1.1044, 0.00015, 1.0);
        assert_eq!(out, SlWiden::Unchanged);
    }

    #[test]
    fn degenerate_spread_is_unchanged() {
        // Closed / crossed / NaN spread → unjudgeable, never widen.
        assert_eq!(
            widen_sl_to_spread_floor(1.10, 1.099, 1.11, 0.0, 1.0),
            SlWiden::Unchanged
        );
        assert_eq!(
            widen_sl_to_spread_floor(1.10, 1.099, 1.11, -0.0002, 1.0),
            SlWiden::Unchanged
        );
        assert_eq!(
            widen_sl_to_spread_floor(1.10, 1.099, 1.11, f64::NAN, 1.0),
            SlWiden::Unchanged
        );
    }

    #[test]
    fn too_tight_long_widens_and_stays_legal() {
        // Long: entry 1.1000, SL 1.0995 (5 pips), spread 0.0001 → floor 0.0010,
        // violated. Widen to 10× = 0.0010 below entry → 1.0990. TP 1.1050 is
        // 0.0050 above entry → R = 0.0050 / 0.0010 = 5.0 ≥ 1.0 → widened.
        let out = widen_sl_to_spread_floor(1.1000, 1.0995, 1.1050, 0.0001, 1.0);
        match out {
            SlWiden::Widened {
                new_stop_loss,
                new_sl_distance,
                new_r,
            } => {
                assert!((new_stop_loss - 1.0990).abs() < 1e-9, "{new_stop_loss}");
                assert!((new_sl_distance - 0.0010).abs() < 1e-9, "{new_sl_distance}");
                assert!(new_r > 4.0, "{new_r}");
            }
            other => panic!("expected Widened, got {other:?}"),
        }
    }

    #[test]
    fn too_tight_short_widens_above_entry() {
        // Short: entry 1.1000, SL 1.1005 (5 pips above), spread 0.0001.
        // Widen to 0.0010 ABOVE entry → 1.1010. TP 1.0950 (0.0050 below) →
        // R = 5.0 ≥ 1.0 → widened, stop stays on the short side.
        let out = widen_sl_to_spread_floor(1.1000, 1.1005, 1.0950, 0.0001, 1.0);
        match out {
            SlWiden::Widened { new_stop_loss, .. } => {
                assert!(
                    new_stop_loss > 1.1000,
                    "stop must stay above entry: {new_stop_loss}"
                );
                assert!((new_stop_loss - 1.1010).abs() < 1e-9, "{new_stop_loss}");
            }
            other => panic!("expected Widened, got {other:?}"),
        }
    }

    #[test]
    fn widening_below_r_floor_is_rejected() {
        // Entry 1.1000, SL 1.0999 (1 pip), spread 0.0001. Widen to 0.0010.
        // TP only 1.1008 → tp_distance 0.0008. R = 0.0008 / 0.0010 = 0.8 < 1.0
        // → reject (no legal stop).
        let out = widen_sl_to_spread_floor(1.1000, 1.0999, 1.1008, 0.0001, 1.0);
        match out {
            SlWiden::Reject {
                widened_stop_loss,
                widened_sl_distance,
                r_at_widen,
                min_r,
            } => {
                assert!(r_at_widen < 1.0, "{r_at_widen}");
                assert!((min_r - 1.0).abs() < 1e-9);
                assert!((widened_sl_distance - 0.0010).abs() < 1e-9);
                // Long (SL below entry) → widened stop = entry − 10×spread.
                assert!(
                    (widened_stop_loss - (1.1000 - 0.0010)).abs() < 1e-9,
                    "widened stop level: {widened_stop_loss}"
                );
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn honours_min_r_override_above_one() {
        // Same widen geometry as `too_tight_long_widens_and_stays_legal`
        // (R ≈ 4.5) but with a min_r override of 5.0 → now rejected.
        let out = widen_sl_to_spread_floor(1.1000, 1.0995, 1.1050, 0.0001, 5.0);
        assert!(matches!(out, SlWiden::Reject { .. }), "{out:?}");
    }

    #[test]
    fn verdict_is_invariant_to_pip_size() {
        // The floor is a pure ratio of two raw-price distances, so it must NOT
        // depend on any instrument's `pip_size`. Neither the violation check nor
        // the widen helper takes a `pip_size` argument — this test documents
        // that property by computing the same geometry as if it came from two
        // instruments with wildly different pip scales (an FX-like 0.0001 pip
        // and a metal-like 0.00001 pip) and asserting identical verdicts.
        //
        // XAGUSD-style: tick 0.00001, spread ~10 ticks = 0.0001.
        // WHEATUSD-style: tick 0.001, spread ~10 ticks = 0.010.
        // Both have an SL distance of exactly 8× their spread → both violate.
        let xag_spread = 0.0001;
        let wheat_spread = 0.010;
        assert!(sl_spread_floor_violation(8.0 * xag_spread, xag_spread));
        assert!(sl_spread_floor_violation(8.0 * wheat_spread, wheat_spread));

        // And the widen helper resolves the same SlWiden *shape* for matching
        // ratios regardless of absolute price scale: both have a 5× SL and a
        // 30× TP, so both widen to 10× and clear R = 30/10 = 3.0.
        let xag = widen_sl_to_spread_floor(
            1.0000,
            1.0000 - 5.0 * xag_spread,
            1.0000 + 30.0 * xag_spread,
            xag_spread,
            1.0,
        );
        let wheat = widen_sl_to_spread_floor(
            5.0000,
            5.0000 - 5.0 * wheat_spread,
            5.0000 + 30.0 * wheat_spread,
            wheat_spread,
            1.0,
        );
        assert!(matches!(xag, SlWiden::Widened { .. }), "{xag:?}");
        assert!(matches!(wheat, SlWiden::Widened { .. }), "{wheat:?}");
    }

    // ---- mean_spread --------------------------------------------------------

    #[test]
    fn mean_of_clean_window() {
        let m = mean_spread(&[0.0001, 0.0002, 0.0003]).expect("some");
        assert!((m - 0.0002).abs() < 1e-12, "{m}");
    }

    #[test]
    fn mean_skips_degenerate_samples() {
        // NaN, zero and a crossed-book negative are all dropped; the mean is of
        // the two good samples only (0.0001 and 0.0003 → 0.0002).
        let m = mean_spread(&[0.0001, f64::NAN, 0.0, -0.0005, 0.0003]).expect("some");
        assert!((m - 0.0002).abs() < 1e-12, "{m}");
    }

    #[test]
    fn mean_of_empty_or_all_degenerate_is_none() {
        assert_eq!(mean_spread(&[]), None);
        assert_eq!(mean_spread(&[0.0, -1.0, f64::NAN, f64::INFINITY]), None);
    }

    #[test]
    fn mean_damps_a_single_spike() {
        // The exact problem: four normal ~1.5-pip bars and one 20-pip spike.
        // The single-sample floor would size off the 0.0020 spike; the mean is
        // (4×0.00015 + 0.0020)/5 = 0.00052 — a quarter of the spike, so the
        // floor sizes off ~5 pips not ~20.
        let m = mean_spread(&[0.00015, 0.00015, 0.00015, 0.00015, 0.0020]).expect("some");
        assert!(
            m < 0.0020 / 3.0,
            "spike must be diluted well below itself, got {m}"
        );
        assert!(m > 0.00015, "but still above the calm baseline, got {m}");
    }

    #[test]
    fn default_spread_window_is_five() {
        assert_eq!(DEFAULT_SPREAD_WINDOW, 5);
    }

    #[test]
    fn wheat_short_salvaged() {
        // From the operator's wheat replay (2026-06-23): SHORT stop @ 5.9538,
        // SL 5.9882 (distance 0.0344), TP 5.7657. Rejected as sl-below-10x
        // because wheat's spread is wide. With a representative 0.005 spread the
        // 10× floor is 0.05 > 0.0344 → violated. Widen to 10× = 0.05 above
        // entry → 6.0038; TP distance = 5.9538 − 5.7657 = 0.1881 →
        // R = 0.1881 / 0.05 ≈ 3.76 ≥ 1.0 → entry salvaged.
        let out = widen_sl_to_spread_floor(5.9538, 5.9882, 5.7657, 0.005, 1.0);
        match out {
            SlWiden::Widened {
                new_stop_loss,
                new_r,
                ..
            } => {
                assert!(new_stop_loss > 5.9538, "short stop stays above entry");
                assert!(new_r > 3.0, "{new_r}");
            }
            other => panic!("expected wheat short to be salvaged, got {other:?}"),
        }
    }
}
