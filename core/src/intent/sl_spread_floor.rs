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
/// whose `sl_distance` is below `SL_MIN_SPREAD_MULTIPLE × spread` is rejected.
pub const SL_MIN_SPREAD_MULTIPLE: f64 = 10.0;

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
}
