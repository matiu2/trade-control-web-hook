//! *"What should this stop-loss be, right now?"* — the one place that decides.
//!
//! Pure: no broker, no store, no clock. That is what lets the same function
//! answer two different questions by varying only its input (see
//! [`SpreadInputs`]), and what makes it mutation-testable — flip a sign or drop
//! a clamp and a test must go red.
//!
//! # Why one function
//!
//! Before this, two unrelated implementations both widened stops:
//!
//! | | where | unit | source | bounds |
//! |---|---|---|---|---|
//! | System A | `intent::sl_spread_floor` | price | 5-bar trailing mean | none |
//! | System B | `blackout_widen` | pips | baked hourly p90 | 22–40 clamp |
//!
//! Different units, different sampling, different constants, no shared notion of
//! what the stop *should* be — so they drifted, and neither could see what the
//! other had done. Widen and shrink are not two features either; they are one
//! question differing by a sign. Splitting them first is how the drift happened,
//! so they are deliberately reunited here.
//!
//! # The floor is a `max` of three terms
//!
//! ```text
//! sl_distance = max(
//!     SL_MIN_SPREAD_MULTIPLE × last_candle_spread,    // reactive
//!     SL_MIN_SPREAD_MULTIPLE × expected_hour_spread,  // FORWARD-LOOKING
//!     desired_sl_distance,                            // never tighter than drawn
//! )
//! ```
//!
//! The middle term is the point of the whole design. Today's floor reads a 5-bar
//! trailing mean, so it can only react *after* a spread has widened — at which
//! point the reaction happens at a bad price. The expected term sizes the stop
//! for a spike **before it arrives**, which is the protection the 30-minute
//! `SPREAD_HOUR_LEAD_MINUTES` clock proxy was providing, recovered structurally
//! and continuously rather than as a step function around flagged hours.
//!
//! ⚠️ **Which hour?** A stop resting at 20:55 can fill at 21:05, inside the
//! spike. So [`SpreadInputs::expected`] takes the `max` over the current *and*
//! next hour — sizing off only the current hour re-opens the exact gap the lead
//! existed to close, and does so silently.
//!
//! Degrades cleanly: an instrument with no baked hourly row contributes `0.0`,
//! and the `max` falls through to exactly today's reactive behaviour.
//!
//! # Same function, two questions
//!
//! - Feed it the **measured** spread → *"what should this stop be?"* (widen /
//!   shrink / hold).
//! - Feed it the **expected** spread → *"what would this trade be worth at the
//!   spread that's coming?"* — the synthetic pre-check that parks a trade which
//!   can't carry the cost, replacing the per-instrument-hour boolean gate with a
//!   per-trade test.
//!
//! One code path, so replay and live cannot diverge and there is no second
//! implementation to keep in sync.

use crate::intent::SL_MIN_SPREAD_MULTIPLE;

/// The spread readings a stop is sized against. All in **price units** (`ask −
/// bid`), so no caller needs to agree on a pip size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpreadInputs {
    /// The spread we can actually measure now — a live quote, or the trailing
    /// windowed mean the entry floor uses. Reactive: it only moves after the
    /// market does.
    pub last_candle: f64,
    /// The baked p90 spread forecast for the hour we are in. `0.0` when the
    /// instrument has no baked row, which makes the term vanish from the `max`.
    pub expected_this_hour: f64,
    /// The baked p90 forecast for the **next** hour. Load-bearing: a resting
    /// order placed at 20:55 can fill at 21:05, so the stop must already clear
    /// the hour it might fill in, not merely the one it was placed in.
    pub expected_next_hour: f64,
}

impl SpreadInputs {
    /// A spread reading with no forecast available — exactly today's behaviour.
    pub fn measured_only(last_candle: f64) -> Self {
        Self {
            last_candle,
            expected_this_hour: 0.0,
            expected_next_hour: 0.0,
        }
    }

    /// The forward-looking spread to size against: the worse of this hour and
    /// next. See the struct docs on why `next` is included.
    ///
    /// Non-finite samples are dropped rather than propagated — a `NaN` forecast
    /// must not poison the `max` into `NaN` and take the whole floor with it.
    pub fn expected(&self) -> f64 {
        [self.expected_this_hour, self.expected_next_hour]
            .into_iter()
            .filter(|s| s.is_finite() && *s > 0.0)
            .fold(0.0, f64::max)
    }

    /// The largest usable spread across every reading, forward-looking included.
    fn worst(&self) -> f64 {
        let measured = if self.last_candle.is_finite() && self.last_candle > 0.0 {
            self.last_candle
        } else {
            0.0
        };
        measured.max(self.expected())
    }
}

/// What should happen to the stop, and what the trade is worth if it does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlTarget {
    /// The stop **distance** the trade should carry, in price units. Always
    /// `>= 0`, and never tighter than the trade's own drawn distance.
    pub desired_sl_distance: f64,
    /// Reward:risk at `desired_sl_distance` — `tp_distance / desired_sl_distance`.
    pub r: f64,
    /// What this is relative to where the stop sits now.
    pub action: SlAction,
}

/// The direction of travel for the stop, relative to its current distance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlAction {
    /// The floor demands more room than the stop currently has: move it away
    /// from price. Safe on any order state — widening never realises a loss.
    Widen,
    /// The stop is wider than it needs to be and can be brought back toward the
    /// trade's original drawn distance.
    ///
    /// ⚠️ **Never act on this for a LIVE position unless the trade is in
    /// profit** — see [`in_profit`]. Tightening a losing position's stop
    /// converts an unrealised loss into a realised one at a level nobody chose.
    /// The check is deliberately *not* folded in here: this module is pure and
    /// has no price feed, so the caller supplies the verdict.
    Shrink,
    /// The stop is already where it should be (within a relative epsilon).
    Hold,
    /// The floor forces a stop so wide the trade no longer clears its R-floor.
    /// The caller must **not** place it — but under the stored-order design it
    /// parks the trade rather than discarding it, and re-checks next candle.
    BelowMinR,
}

/// Prices/distances within this fraction of each other count as equal, so float
/// round-tripping doesn't produce a pointless amend or a phantom `Widen`.
const RELATIVE_EPSILON: f64 = 1e-9;

/// Is the trade far enough in profit that tightening its stop is safe?
///
/// **"In profit" means beyond entry by more than the current spread** — green
/// net of the round-trip, not merely green on mid. A position that is one tick
/// above entry on mid is still *underwater* once the spread is crossed, so
/// shrinking its stop there would lock in a loss.
///
/// Direction-aware: a long profits upward, a short downward. `spread` is the
/// live `ask − bid`; a degenerate (non-finite / non-positive) spread yields
/// `false` — unjudgeable means "don't tighten", the conservative answer.
pub fn in_profit(direction: crate::intent::Direction, entry: f64, price: f64, spread: f64) -> bool {
    if !(entry.is_finite() && price.is_finite() && spread.is_finite() && spread > 0.0) {
        return false;
    }
    match direction {
        crate::intent::Direction::Long => price - entry > spread,
        crate::intent::Direction::Short => entry - price > spread,
    }
}

/// Decide the stop distance this trade should carry.
///
/// # Arguments (all **price-unit distances**, never pips)
///
/// - `spreads` — the readings to size against; see [`SpreadInputs`].
/// - `original_sl_distance` — the trade's own drawn stop distance. The result is
///   **never tighter** than this: shrinking past the operator's drawn level
///   would be inventing a trade they didn't ask for.
/// - `current_sl_distance` — where the stop sits *now*, which is what makes the
///   result a widen, a shrink, or a hold. For a not-yet-placed order this is the
///   original.
/// - `tp_distance` — take-profit distance from entry, for the R calculation.
/// - `min_r` — the trade's effective R-floor.
///
/// # Degenerate inputs
///
/// A non-finite `original_sl_distance` or `tp_distance` yields [`SlAction::Hold`]
/// at the original distance: unjudgeable geometry is never a reason to move a
/// live stop. Degenerate *spreads* are simply dropped from the `max`, exactly as
/// `sl_spread_floor_violation` treats an unjudgeable spread as no violation.
pub fn sl_target(
    spreads: SpreadInputs,
    original_sl_distance: f64,
    current_sl_distance: f64,
    tp_distance: f64,
    min_r: f64,
) -> SlTarget {
    // Unjudgeable geometry ⇒ change nothing.
    if !original_sl_distance.is_finite() || original_sl_distance <= 0.0 {
        return SlTarget {
            desired_sl_distance: original_sl_distance,
            r: f64::NAN,
            action: SlAction::Hold,
        };
    }

    // The floor: 10× the worst spread we know about, forward-looking included.
    let spread_floor = SL_MIN_SPREAD_MULTIPLE * spreads.worst();
    // Never tighter than drawn — the third term of the `max`.
    let desired = spread_floor.max(original_sl_distance);

    let r = if tp_distance.is_finite() && desired > 0.0 {
        tp_distance / desired
    } else {
        f64::NAN
    };

    // Below the R-floor the trade can't carry the cost of its own stop. Report
    // the desired distance anyway so the caller can log what it would have taken.
    if !r.is_finite() || r < min_r {
        return SlTarget {
            desired_sl_distance: desired,
            r,
            action: SlAction::BelowMinR,
        };
    }

    let action = if !current_sl_distance.is_finite() || current_sl_distance <= 0.0 {
        // No usable current stop to compare against — treat the desired
        // distance as a fresh target rather than inventing a direction.
        SlAction::Hold
    } else {
        let scale = desired.abs().max(current_sl_distance.abs()).max(1.0);
        if (desired - current_sl_distance).abs() <= RELATIVE_EPSILON * scale {
            SlAction::Hold
        } else if desired > current_sl_distance {
            SlAction::Widen
        } else {
            SlAction::Shrink
        }
    };

    SlTarget {
        desired_sl_distance: desired,
        r,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Direction;

    /// The floor with no forecast — today's reactive behaviour, unchanged.
    #[test]
    fn measured_only_reproduces_the_reactive_floor() {
        // spread 0.0001 → floor 0.0010. Drawn stop 0.0005 is tighter → widen.
        let out = sl_target(
            SpreadInputs::measured_only(0.0001),
            0.0005,
            0.0005,
            0.0100,
            1.0,
        );
        assert_eq!(out.action, SlAction::Widen);
        assert!((out.desired_sl_distance - 0.0010).abs() < 1e-12);
    }

    /// The headline feature: a calm *measured* spread but a spike forecast for
    /// the coming hour must widen the stop NOW, before the spike lands.
    ///
    /// Mutation check: drop `expected` from `worst()` and this goes red.
    #[test]
    fn forecast_widens_before_the_spike_arrives() {
        // Measured is calm (0.00015 → floor 0.0015) but the next hour forecasts
        // 0.00064 (the real EUR/USD 17:00 p90) → floor 0.0064.
        let spreads = SpreadInputs {
            last_candle: 0.00015,
            expected_this_hour: 0.00015,
            expected_next_hour: 0.00064,
        };
        let out = sl_target(spreads, 0.0020, 0.0020, 0.0200, 1.0);
        assert_eq!(out.action, SlAction::Widen);
        assert!(
            (out.desired_sl_distance - 0.0064).abs() < 1e-12,
            "must size off the FORECAST, got {}",
            out.desired_sl_distance
        );
    }

    /// The 20:55-fills-at-21:05 case, stated directly: sizing off only the
    /// current hour silently re-opens the gap the 30-min lead existed to close.
    #[test]
    fn expected_takes_the_worse_of_this_hour_and_next() {
        let spreads = SpreadInputs {
            last_candle: 0.0001,
            expected_this_hour: 0.0002,
            expected_next_hour: 0.0009,
        };
        assert!(
            (spreads.expected() - 0.0009).abs() < 1e-12,
            "the NEXT hour's spike must win"
        );
        // ...and symmetrically, when this hour is worse it wins.
        let spreads = SpreadInputs {
            last_candle: 0.0001,
            expected_this_hour: 0.0009,
            expected_next_hour: 0.0002,
        };
        assert!((spreads.expected() - 0.0009).abs() < 1e-12);
    }

    /// A measured spread WORSE than the forecast still wins — the forecast is a
    /// floor, not a cap. A worse-than-history night must not be under-sized.
    #[test]
    fn measured_wins_when_worse_than_forecast() {
        let spreads = SpreadInputs {
            last_candle: 0.0010,
            expected_this_hour: 0.0002,
            expected_next_hour: 0.0003,
        };
        let out = sl_target(spreads, 0.0020, 0.0020, 0.0500, 1.0);
        assert!((out.desired_sl_distance - 0.0100).abs() < 1e-12);
    }

    /// The stop is NEVER tightened past the operator's drawn level, however
    /// calm the market gets.
    ///
    /// Mutation check: remove `.max(original_sl_distance)` and this goes red.
    #[test]
    fn never_shrinks_past_the_drawn_stop() {
        // Spread is tiny; floor would be 0.0001, far tighter than the drawn
        // 0.0050. The drawn distance must survive.
        let out = sl_target(
            SpreadInputs::measured_only(0.00001),
            0.0050,
            0.0050,
            0.0200,
            1.0,
        );
        assert_eq!(out.action, SlAction::Hold);
        assert!((out.desired_sl_distance - 0.0050).abs() < 1e-12);
    }

    /// A stop previously widened for a spike is given back once the spread
    /// calms — but only as far as the original.
    #[test]
    fn shrinks_back_toward_the_original_when_the_spread_calms() {
        // Drawn 0.0020, currently widened to 0.0064, spread now calm.
        let out = sl_target(
            SpreadInputs::measured_only(0.00015),
            0.0020,
            0.0064,
            0.0200,
            1.0,
        );
        assert_eq!(out.action, SlAction::Shrink);
        assert!(
            (out.desired_sl_distance - 0.0020).abs() < 1e-12,
            "shrink lands on the drawn stop, not past it"
        );
    }

    /// Below the R-floor the trade is flagged, not silently placed. Under the
    /// stored-order design this parks rather than rejects.
    #[test]
    fn below_min_r_is_reported_not_placed() {
        // Floor forces 0.0100 but TP is only 0.0050 away → R = 0.5 < 1.0.
        let out = sl_target(
            SpreadInputs::measured_only(0.0010),
            0.0020,
            0.0020,
            0.0050,
            1.0,
        );
        assert_eq!(out.action, SlAction::BelowMinR);
        assert!(out.r < 1.0, "{}", out.r);
        assert!(
            (out.desired_sl_distance - 0.0100).abs() < 1e-12,
            "still reports what it WOULD have taken, for the log"
        );
    }

    /// The synthetic pre-check: the *same* function, fed the expected spread,
    /// answers "would this trade still be worth taking at the coming spread?".
    /// This is what replaces the per-instrument-hour boolean gate.
    #[test]
    fn same_function_answers_the_synthetic_pre_check() {
        // A tight scalp: 20-pip stop, 30-pip TP. Calm now, spike coming.
        let calm = SpreadInputs::measured_only(0.00015);
        let spiky = SpreadInputs {
            last_candle: 0.00015,
            expected_this_hour: 0.00064,
            expected_next_hour: 0.00064,
        };
        let now = sl_target(calm, 0.0020, 0.0020, 0.0030, 1.0);
        let coming = sl_target(spiky, 0.0020, 0.0020, 0.0030, 1.0);
        assert_ne!(
            now.action,
            SlAction::BelowMinR,
            "the scalp is fine at today's spread"
        );
        assert_eq!(
            coming.action,
            SlAction::BelowMinR,
            "...but cannot carry the coming spread — park it"
        );

        // The per-trade discrimination a boolean gate cannot express: a
        // wide-stop setup with a distant TP, SAME instrument, SAME minute,
        // trades straight through the spike.
        let patient = sl_target(spiky, 0.0080, 0.0080, 0.0400, 1.0);
        assert_ne!(
            patient.action,
            SlAction::BelowMinR,
            "a wide-stop setup clears 1R through the same spread hour"
        );
    }

    // ---- in_profit ---------------------------------------------------------

    /// The filter that makes shrinking a LIVE stop safe. "Green on mid" is not
    /// enough — the round-trip has to be covered.
    ///
    /// Mutation check: change `>` to `>=` or drop the spread term and the
    /// barely-green case flips.
    #[test]
    fn in_profit_requires_beating_the_spread_not_just_entry() {
        let spread = 0.0002;
        // Long, 1 pip above entry: green on mid, still underwater net.
        assert!(!in_profit(Direction::Long, 1.1000, 1.1001, spread));
        // Long, 5 pips above entry: genuinely green.
        assert!(in_profit(Direction::Long, 1.1000, 1.1005, spread));
        // Short mirrors.
        assert!(!in_profit(Direction::Short, 1.1000, 1.0999, spread));
        assert!(in_profit(Direction::Short, 1.1000, 1.0995, spread));
    }

    #[test]
    fn in_profit_is_false_for_a_losing_or_unjudgeable_trade() {
        assert!(!in_profit(Direction::Long, 1.1000, 1.0900, 0.0002));
        assert!(!in_profit(Direction::Short, 1.1000, 1.1100, 0.0002));
        // Degenerate spread → unjudgeable → never tighten.
        assert!(!in_profit(Direction::Long, 1.1000, 1.1050, 0.0));
        assert!(!in_profit(Direction::Long, 1.1000, 1.1050, f64::NAN));
    }

    // ---- degenerate inputs --------------------------------------------------

    /// A NaN forecast must not poison the `max` and take the whole floor with
    /// it — it is dropped, and the measured reading still applies.
    #[test]
    fn non_finite_forecast_is_dropped_not_propagated() {
        let spreads = SpreadInputs {
            last_candle: 0.0001,
            expected_this_hour: f64::NAN,
            expected_next_hour: f64::INFINITY,
        };
        assert_eq!(spreads.expected(), 0.0);
        let out = sl_target(spreads, 0.0005, 0.0005, 0.0100, 1.0);
        assert!(
            out.desired_sl_distance.is_finite(),
            "a NaN forecast must not produce a NaN stop"
        );
        assert!((out.desired_sl_distance - 0.0010).abs() < 1e-12);
    }

    #[test]
    fn degenerate_geometry_holds_rather_than_moving_a_live_stop() {
        for bad in [f64::NAN, 0.0, -0.001] {
            let out = sl_target(SpreadInputs::measured_only(0.0001), bad, bad, 0.01, 1.0);
            assert_eq!(out.action, SlAction::Hold, "original_sl_distance={bad}");
        }
    }

    /// A closed market (zero/negative spread) is unjudgeable, not a reason to
    /// widen — matching `sl_spread_floor_violation`'s fail-open discipline.
    #[test]
    fn degenerate_spread_falls_through_to_the_drawn_stop() {
        for bad in [0.0, -0.0002, f64::NAN] {
            let out = sl_target(
                SpreadInputs::measured_only(bad),
                0.0020,
                0.0020,
                0.0100,
                1.0,
            );
            assert_eq!(out.action, SlAction::Hold, "spread={bad}");
            assert!((out.desired_sl_distance - 0.0020).abs() < 1e-12);
        }
    }

    /// The floor multiple is a pure ratio of price-unit distances, so the
    /// verdict must not depend on an instrument's pip scale — an FX cross and
    /// an index with the same ratios resolve identically.
    #[test]
    fn verdict_is_invariant_to_price_scale() {
        // FX-like: spread 0.0001, drawn 5× spread, TP 30× spread.
        let fx = sl_target(
            SpreadInputs::measured_only(0.0001),
            5.0 * 0.0001,
            5.0 * 0.0001,
            30.0 * 0.0001,
            1.0,
        );
        // Index-like: spread 2.0 points, same ratios.
        let index = sl_target(
            SpreadInputs::measured_only(2.0),
            5.0 * 2.0,
            5.0 * 2.0,
            30.0 * 2.0,
            1.0,
        );
        assert_eq!(fx.action, index.action);
        assert!((fx.r - index.r).abs() < 1e-9, "{} vs {}", fx.r, index.r);
    }
}
