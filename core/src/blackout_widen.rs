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
/// **Legacy path.** Used only when there is no baked per-instrument
/// spread-hour p90 for the instrument (an un-sampled asset falling back to
/// the `is_ny_close_edge` gate). When a baked p90 *is* available the caller
/// uses [`spread_hour_widen_size`] instead, whose bounds are
/// instrument-relative — the flat 22–40 clamp here is wrong for the range
/// baked p90 spans (EUR/USD 5p to Gold 75p), which is exactly why the
/// per-instrument path exists.
///
/// `f64::clamp` panics on a NaN bound but our bounds are constants; a NaN
/// *input* yields NaN, which the caller treats as "unusable, skip the
/// widen" upstream (it never reaches here for a NaN spread — Cron 1 guards
/// on a finite `pip_size` and a successful quote first).
pub fn clamp_widen(live_spread_pips: f64) -> f64 {
    live_spread_pips.clamp(WIDEN_FLOOR_PIPS, WIDEN_CEIL_PIPS)
}

/// Ceiling headroom over the baked p90 for the per-instrument widen — see
/// [`spread_hour_widen_size`]. The widen may exceed the baked p90 (a live
/// spread worse than history) but never by more than this multiple, so a
/// freak live-spread print can't blow the stop out absurdly far and convert
/// a designed −1R into a −4R.
pub const WIDEN_BAKED_CEIL_MULT: f64 = 1.5;

/// Pips to widen an open stop by during a **learned spread hour**, given the
/// instrument's baked p90 for that hour and the current live spread.
///
/// # Design (documented so a later revisit can change it deliberately)
///
/// The **baked p90 is the primary widen** — the historical typical-high
/// spread for this instrument at this hour, which is *why* we widen
/// pre-emptively (we know the spike is coming and roughly how big). The
/// **live spread is a floor**: if the current spread is already worse than
/// history predicted (an unusually bad night), we widen by the live reading
/// instead so the stop still clears it. The **ceiling** is
/// `max(WIDEN_CEIL_PIPS, baked_p90 × WIDEN_BAKED_CEIL_MULT)` — per-instrument
/// (not the flat 40) so Gold's legitimate ~75p widen isn't capped at 40,
/// while a 200p freak live print is still bounded to 1.5× the baked p90.
///
/// So: `widen = clamp(max(baked_p90, live_spread), floor, ceil)` where
/// `floor = min(baked_p90, WIDEN_FLOOR_PIPS)` — the floor never forces a
/// small legitimate widen (EUR/USD 5p) up to the legacy 22p, but a
/// degenerate baked 0 still can't produce a zero widen.
///
/// Alternatives considered (if revisiting): (a) `max(baked_p90, live)` with
/// no ceiling — simplest, but unbounded on a freak live print; (b) baked p90
/// only, ignoring live — predictable but blind to a worse-than-history
/// night. We chose the floored+ceilinged blend for protection without
/// unboundedness. The restore path is unchanged either way (it restores the
/// remembered original stop verbatim, so an over-wide widen is given back as
/// soon as the live spread recovers).
///
/// Pure — no broker/KV. A NaN input yields NaN (the caller skips the widen
/// upstream on a non-finite spread / pip, as with [`clamp_widen`]).
pub fn spread_hour_widen_size(baked_p90_pips: f64, live_spread_pips: f64) -> f64 {
    let ceil = WIDEN_CEIL_PIPS.max(baked_p90_pips * WIDEN_BAKED_CEIL_MULT);
    // Floor never inflates a small legitimate widen, but guards a degenerate
    // baked 0 (shouldn't happen — an elevated hour bakes a positive p90 — but
    // keep the widen non-trivial if it ever did).
    let floor = baked_p90_pips.min(WIDEN_FLOOR_PIPS);
    baked_p90_pips.max(live_spread_pips).clamp(floor, ceil)
}

/// New stop-loss after widening `widen_pips` away from price, snapped onto the
/// `pip_size` grid.
///
/// SHORT → stop sits **above** entry → widening moves the SL **UP**
/// (`original + widen`). LONG → stop sits **below** entry → widening moves
/// the SL **DOWN** (`original − widen`). Pure & total — mirrors
/// `breach_detected`. Widening the wrong way would *tighten* the stop into
/// the spread and clip the position instantly; the direction matrix test is
/// the sign-bug guard.
///
/// # Why it snaps
///
/// `widen_pips` is a float blend of a baked p90 and a live measured spread
/// (see [`spread_hour_widen_size`] / [`clamp_widen`]), so the raw
/// `original ± widen_pips × pip_size` lands on no price grid at all. OANDA
/// rejects an over-precise price outright — `PRICE_PRECISION_EXCEEDED`, and the
/// amend simply does not happen. That is strictly worse than the same fault on
/// an entry: the position is already **open**, so a rejected amend leaves it
/// running with its un-widened stop straight through the spread spike this
/// widen exists to survive.
///
/// The snap is **away from price**, the same direction as the widen itself —
/// `RoundDir::Up` for a short, `Down` for a long. Snapping the other way would
/// walk part of the widen back, i.e. the sign bug above wearing a rounding hat.
///
/// # Why the grid is `pip_size` and not a tick
///
/// The caller (`blackout_apply`) reads `pip_size` off the originating
/// `EntryAttempt`, which carries **no** `tick_size` — adding one is a persisted
/// schema change, deliberately out of scope here. `pip_size` is a safe
/// **coarser-or-equal** grid: gold's pip and tick are both `0.01`, while
/// NAS100 pips at `1.0` against a `0.1` tick and FX pips at `0.0001` against
/// `0.00001`. A value on the coarser grid is always also on the finer one, so
/// the broker accepts it; the only cost is that an index widen is quantised to
/// whole points. Combined with the away-from-price direction, over-coarseness
/// can only ever widen *further* — never tighten. If `EntryAttempt` later
/// carries a tick, pass that instead for a tighter grid; nothing else changes.
///
/// A non-positive / non-finite `pip_size` is identity — see [`crate::rounding`].
pub fn widened_stop(direction: Direction, original_sl: f64, widen_pips: f64, pip_size: f64) -> f64 {
    let widen = widen_pips * pip_size;
    let (raw, dir) = match direction {
        // Up, away from price — and snap the same way.
        Direction::Short => (original_sl + widen, crate::rounding::RoundDir::Up),
        // Down, away from price — and snap the same way.
        Direction::Long => (original_sl - widen, crate::rounding::RoundDir::Down),
    };
    crate::rounding::round_to_tick(raw, pip_size, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE SECOND INSTANCE of the `PRICE_PRECISION_EXCEEDED` bug, on the AMEND
    /// path. `widen_pips` is a float blend of a baked p90 and a live measured
    /// spread (`spread_hour_widen_size` / `clamp_widen`), so `original ± pips ×
    /// tick` lands on no grid at all. This one is WORSE than the entry case:
    /// the position is already OPEN, so a rejected amend leaves it running with
    /// the un-widened stop through the very spread spike the widen exists for.
    #[test]
    fn a_fractional_widen_lands_on_the_tick_grid() {
        // Gold (tick 0.01) short, stop 4016.35, widen 34.7 "pips" of 0.01 =
        // 0.347 → 4016.697 raw: three decimals on a two-decimal grid. Must snap
        // UP (away from price for a short) to 4016.70, never the nearest.
        let short = widened_stop(Direction::Short, 4016.35, 34.7, 0.01);
        assert!(
            (short - 4016.70).abs() < 1e-9,
            "expected 4016.70 on gold's 0.01 grid, got {short}"
        );
        // Long, mirrored: 4016.35 − 0.347 = 4016.003 → snaps DOWN to 4016.00.
        let long = widened_stop(Direction::Long, 4016.35, 34.7, 0.01);
        assert!(
            (long - 4016.00).abs() < 1e-9,
            "expected 4016.00 on gold's 0.01 grid, got {long}"
        );
    }

    /// The snap must never walk the widen back. A widen that rounds TOWARD
    /// price is the sign bug wearing a rounding hat: it tightens the stop into
    /// the very spread spike the widen was applied to survive.
    ///
    /// Swept over fractional widens that land all across a tick cell, so a
    /// `Nearest` snap is caught (it tightens on roughly half of them) rather
    /// than passing on a lucky value.
    #[test]
    fn the_snap_never_walks_the_widen_back() {
        let (orig, tick) = (4016.35, 0.01);
        for step in 0..20 {
            let pips = 34.0 + f64::from(step) * 0.1; // 34.0 .. 35.9
            let raw = pips * tick;
            let short = widened_stop(Direction::Short, orig, pips, tick);
            assert!(
                short >= orig + raw - 1e-9,
                "short widen {pips} tightened: {short} < {}",
                orig + raw
            );
            let long = widened_stop(Direction::Long, orig, pips, tick);
            assert!(
                long <= orig - raw + 1e-9,
                "long widen {pips} tightened: {long} > {}",
                orig - raw
            );
        }
    }

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

    // --- per-instrument spread-hour widen size ---

    #[test]
    fn baked_p90_is_the_primary_widen_when_live_is_quiet() {
        // EUR/USD 21:00: baked p90 5p, live spread still tight (0.5p).
        // Widen by the baked prediction, NOT forced up to the legacy 22p
        // floor (the whole point of the per-instrument path).
        assert_eq!(spread_hour_widen_size(5.0, 0.5), 5.0);
        // Gold overnight: baked 75p, live quiet 40p → baked wins.
        assert_eq!(spread_hour_widen_size(75.0, 40.0), 75.0);
    }

    #[test]
    fn live_spread_floors_the_widen_when_worse_than_history() {
        // A worse-than-history night: live 8p exceeds the baked 5p → widen
        // by the live reading so the stop still clears the real spread.
        assert_eq!(spread_hour_widen_size(5.0, 8.0), 8.0);
    }

    #[test]
    fn ceiling_is_per_instrument_not_the_flat_forty() {
        // Gold's legitimate ~75p widen must NOT be capped at the flat 40 —
        // the ceiling is max(40, 75*1.5)=112.5, so 75 passes through.
        assert_eq!(spread_hour_widen_size(75.0, 40.0), 75.0);
        // A freak live print (300p) on Gold is bounded to 75*1.5 = 112.5.
        assert_eq!(spread_hour_widen_size(75.0, 300.0), 112.5);
    }

    #[test]
    fn small_baked_p90_freak_live_bounded_to_baked_ceiling_or_flat_forty() {
        // EUR/USD baked 5p: ceiling is max(40, 5*1.5=7.5) = 40 (the flat
        // ceiling wins for small p90), so a 200p freak print caps at 40.
        assert_eq!(spread_hour_widen_size(5.0, 200.0), WIDEN_CEIL_PIPS);
    }

    #[test]
    fn degenerate_zero_baked_falls_back_to_live_within_bounds() {
        // Shouldn't happen (an elevated hour bakes a positive p90) but keep
        // it total: baked 0, live 30 → 30 (floor is min(0,22)=0, ceil 40).
        assert_eq!(spread_hour_widen_size(0.0, 30.0), 30.0);
    }
}
