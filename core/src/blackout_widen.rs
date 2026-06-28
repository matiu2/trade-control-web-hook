//! Pure widen math for System 2 (spread-blackout stop widening).
//! KV-free, broker-free — the unit-testable seam, mirroring
//! `crate::cron::sweep::breach_detected`.
//!
//! Lives in `core` so the live worker (`src/cron/blackout_apply.rs`) and
//! the offline replay (`engine::simulator`) compute the same widened stop
//! and can't drift — see `[[strategy_changes_in_both_replayer_and_worker]]`.
//!
//! Two independent quantities, each its own pure fn:
//!
//! * [`widened_stop`] — *which way* and *to what level* (the sign-bug seam:
//!   SHORT moves the stop UP, LONG moves it DOWN; the wrong direction
//!   tightens into the spread and clips the position instantly).
//! * [`clamp_widen`] — *how far*, bounded by the 22–40-pip clamp.
//!
//! There is deliberately **no** `restored_stop` helper: restore is
//! `amend_stop(account, id, remembered.original_stop)` — the remembered
//! original verbatim, never `current − widen`. "No math" is the safety
//! property (a partial widen / missed tick / double-fire can't drift the
//! restored level). See `blackout_watch::restore_remembered_stops`.

use crate::intent::Direction;

/// Empirical floor: ~the observed EUR/NZD blowout (~22 pips). Never widen
/// by less — guards a moment where the sampled spread reads tight at
/// fire-time (a brief snap-back between samples) and would otherwise leave
/// the stop sitting inside the next spread flare.
pub const WIDEN_FLOOR_PIPS: f64 = 22.0;

/// Hard ceiling: a freak spread print (a sampled spread momentarily reading
/// 200+ pips) must not blow the stop out absurdly far and convert a designed
/// −1R into a −4R if price genuinely runs. 40 caps the worst case.
pub const WIDEN_CEIL_PIPS: f64 = 40.0;

/// Pips to widen by: the live spread, floored at [`WIDEN_FLOOR_PIPS`] and
/// clamped at [`WIDEN_CEIL_PIPS`]. Pure — no broker/KV. Cron 1 has already
/// sampled `ask − bid` to know it's in the window; we widen by THAT,
/// bounded.
///
/// `f64::clamp` panics on a NaN bound but our bounds are constants; a NaN
/// *input* yields NaN, which the caller treats as "unusable, skip the
/// widen" upstream (it never reaches here for a NaN spread — Cron 1 guards
/// on a finite `pip_size` and a successful quote first).
pub fn clamp_widen(live_spread_pips: f64) -> f64 {
    live_spread_pips.clamp(WIDEN_FLOOR_PIPS, WIDEN_CEIL_PIPS)
}

/// New stop-loss after widening `widen_pips` away from price.
///
/// SHORT → stop sits **above** entry → widening moves the SL **UP**
/// (`original + widen`). LONG → stop sits **below** entry → widening moves
/// the SL **DOWN** (`original − widen`). Pure & total — mirrors
/// `breach_detected`. Widening the wrong way would *tighten* the stop into
/// the spread and clip the position instantly; the direction matrix test is
/// the sign-bug guard.
pub fn widened_stop(direction: Direction, original_sl: f64, widen_pips: f64, pip_size: f64) -> f64 {
    let widen = widen_pips * pip_size;
    match direction {
        Direction::Short => original_sl + widen, // up, away from price
        Direction::Long => original_sl - widen,  // down, away from price
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_widens_up_long_widens_down() {
        // SHORT: new SL strictly greater than original (moved UP, away).
        let short = widened_stop(Direction::Short, 1.8000, 30.0, 0.0001);
        assert!(short > 1.8000, "short widen must move the stop UP");
        assert!((short - (1.8000 + 30.0 * 0.0001)).abs() < 1e-12);

        // LONG: new SL strictly less than original (moved DOWN, away).
        let long = widened_stop(Direction::Long, 1.8000, 30.0, 0.0001);
        assert!(long < 1.8000, "long widen must move the stop DOWN");
        assert!((long - (1.8000 - 30.0 * 0.0001)).abs() < 1e-12);
    }

    /// The wrong-direction sign guard, stated as an invariant: a short's
    /// new SL is strictly greater than original and a long's strictly less,
    /// for any positive widen. If someone flips the match arms this fails.
    #[test]
    fn direction_sign_invariant() {
        for &(orig, widen, pip) in &[(1.5, 22.0, 0.0001), (100.0, 40.0, 1.0)] {
            assert!(widened_stop(Direction::Short, orig, widen, pip) > orig);
            assert!(widened_stop(Direction::Long, orig, widen, pip) < orig);
        }
    }

    #[test]
    fn pip_size_scales_the_absolute_move() {
        // FX 0.0001: 22 pips = 0.0022 absolute.
        let fx = widened_stop(Direction::Short, 1.8000, 22.0, 0.0001);
        assert!((fx - 1.8022).abs() < 1e-12);
        // Index 1.0: 22 "pips" = 22.0 absolute points.
        let index = widened_stop(Direction::Short, 18000.0, 22.0, 1.0);
        assert!((index - 18022.0).abs() < 1e-9);
        // 3-dp JPY cross 0.01: 22 pips = 0.22 absolute.
        let jpy = widened_stop(Direction::Long, 162.000, 22.0, 0.01);
        assert!((jpy - 161.780).abs() < 1e-9);
    }

    #[test]
    fn clamp_below_floor_returns_floor() {
        assert_eq!(clamp_widen(5.0), WIDEN_FLOOR_PIPS);
        assert_eq!(clamp_widen(0.0), WIDEN_FLOOR_PIPS);
    }

    #[test]
    fn clamp_in_band_is_identity() {
        assert_eq!(clamp_widen(30.0), 30.0);
    }

    #[test]
    fn clamp_above_ceiling_returns_ceiling() {
        assert_eq!(clamp_widen(200.0), WIDEN_CEIL_PIPS);
    }

    #[test]
    fn clamp_exact_boundaries_pass_through() {
        assert_eq!(clamp_widen(WIDEN_FLOOR_PIPS), WIDEN_FLOOR_PIPS);
        assert_eq!(clamp_widen(WIDEN_CEIL_PIPS), WIDEN_CEIL_PIPS);
    }
}
