//! Break-even stop management (BUG-replay-no-breakeven-stop-at-50pct).
//!
//! The operator's rule, lifted from CLAUDE.md ("once profit reaches 50%,
//! immediately set SL to break-even"): once a candle **closes** past 50% of the
//! way from entry to take-profit (in the trade direction), move the active
//! stop-loss to **break-even** — the entry price exactly — so a leg that ran
//! most of the way to TP and then reverses scratches at 0R instead of taking a
//! full −1R.
//!
//! Like [`super::entry_level_veto`] (Bug #12) and the `pause_gate`, this is
//! **pure data + a truth-table baked onto the signed enter intent**, so the two
//! consumers — the offline replay (`simulate_fill`) and the live worker's
//! position cron — resolve and apply the stop move identically and can't drift.
//! The KV/broker side (the worker amending the open position, the replay
//! re-walking the candle path) is a thin wrapper around the helpers here.
//!
//! ## Semantics (confirmed with the operator, 2026-06-28)
//!
//! - **Arming basis is the candle CLOSE**, not an intrabar wick — a fakeout
//!   spike to the midpoint that closes back does not arm BE. Matches the
//!   operator's phrasing.
//! - **Threshold is a fraction of entry→TP** (default 0.5 = 50%). The
//!   [`Breakeven::arms_at`] level is `entry + threshold × (tp − entry)`,
//!   direction-agnostic (the sign of `tp − entry` carries the direction).
//! - **BE target is the entry price exactly** — a true 0R scratch.
//! - **Latched / one-way.** It arms once; the SL is moved and never moves back.
//!   On the arming bar the original SL still applies (the broker's resting stop
//!   handles intrabar) — the moved stop is live from the *next* bar.

use serde::{Deserialize, Serialize};

use super::Direction;

/// Default break-even threshold — arm once a candle closes 50% of the way from
/// entry to take-profit. The single tunable knob; baked onto the intent so a
/// future template can override it per-trade.
pub const DEFAULT_BREAKEVEN_THRESHOLD: f64 = 0.5;

/// Break-even stop management baked onto an enter intent. When present, the
/// stop-loss is moved to [`Breakeven::target_stop`] (the entry price) once a
/// candle closes past [`Breakeven::arms_at`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Breakeven {
    /// Fraction of the entry→TP distance a candle must close past before the
    /// stop moves to break-even. `0.5` = halfway. Must be in `(0, 1)`; values
    /// outside that are clamped by [`Breakeven::sane`] at use time so a
    /// degenerate baked value can never disable the SL or arm on the entry bar.
    pub threshold: f64,
}

impl Default for Breakeven {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_BREAKEVEN_THRESHOLD,
        }
    }
}

impl Breakeven {
    /// A `Breakeven` with the default 50% threshold.
    pub fn at_half() -> Self {
        Self::default()
    }

    /// The threshold, clamped to a sane `(0, 1)` open interval. A baked value
    /// of `0` would arm on the entry bar (every trade scratched instantly); a
    /// value `>= 1` would never arm (BE disabled). Neither is what an operator
    /// means by "move to BE at X%", so we clamp rather than trust the wire.
    fn sane(&self) -> f64 {
        if !self.threshold.is_finite() || self.threshold <= 0.0 || self.threshold >= 1.0 {
            DEFAULT_BREAKEVEN_THRESHOLD
        } else {
            self.threshold
        }
    }

    /// The price a candle must **close past** (in the trade direction) for BE to
    /// arm: `entry + threshold × (take_profit − entry)`. For a long
    /// (`tp > entry`) this is above entry; for a short (`tp < entry`) below.
    pub fn arms_at(&self, entry: f64, take_profit: f64) -> f64 {
        entry + self.sane() * (take_profit - entry)
    }

    /// Does a candle whose close is `close_price` arm break-even, given the
    /// trade `direction` and the [`Breakeven::arms_at`] `level`? "Past" is
    /// **inclusive** at the level — a close exactly on the midpoint arms.
    ///
    /// - Long: arms when `close_price >= level` (price ran up toward TP).
    /// - Short: arms when `close_price <= level` (price ran down toward TP).
    pub fn close_arms(&self, direction: Direction, level: f64, close_price: f64) -> bool {
        match direction {
            Direction::Long => close_price >= level,
            Direction::Short => close_price <= level,
        }
    }

    /// The stop-loss to set once armed — break-even, i.e. the entry price
    /// exactly. A method (not a bare field) so a future "entry ± spread buffer"
    /// variant has one place to change.
    pub fn target_stop(&self, entry: f64) -> f64 {
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_at_is_the_directional_midpoint() {
        let be = Breakeven::at_half();
        // Long: entry 1.0, TP 1.2 → midpoint 1.1.
        assert!((be.arms_at(1.0, 1.2) - 1.1).abs() < 1e-9);
        // Short: entry 1.2, TP 1.0 → midpoint 1.1.
        assert!((be.arms_at(1.2, 1.0) - 1.1).abs() < 1e-9);
    }

    #[test]
    fn arms_at_honours_a_custom_threshold() {
        let be = Breakeven { threshold: 0.8 };
        // 80% of the way from 1.0 to 1.2 = 1.16.
        assert!((be.arms_at(1.0, 1.2) - 1.16).abs() < 1e-9);
    }

    #[test]
    fn long_close_arms_at_or_above_level() {
        let be = Breakeven::at_half();
        let level = 1.1;
        assert!(be.close_arms(Direction::Long, level, 1.11), "above arms");
        assert!(
            be.close_arms(Direction::Long, level, 1.1),
            "exactly at the level arms (inclusive)"
        );
        assert!(
            !be.close_arms(Direction::Long, level, 1.09),
            "below does not arm a long"
        );
    }

    #[test]
    fn short_close_arms_at_or_below_level() {
        let be = Breakeven::at_half();
        let level = 1.1;
        assert!(be.close_arms(Direction::Short, level, 1.09), "below arms");
        assert!(
            be.close_arms(Direction::Short, level, 1.1),
            "exactly at the level arms (inclusive)"
        );
        assert!(
            !be.close_arms(Direction::Short, level, 1.11),
            "above does not arm a short"
        );
    }

    #[test]
    fn target_stop_is_exact_entry() {
        let be = Breakeven::at_half();
        assert!((be.target_stop(5.916) - 5.916).abs() < 1e-9);
    }

    /// Trade-075 leg-2 geometry from the bug report: short entry 5.916, TP
    /// 5.766. The 50% level is 5.841; a candle closing at 5.868 / 5.852 arms,
    /// and the BE stop is the 5.916 entry.
    #[test]
    fn trade_075_leg2_arms_and_targets_entry() {
        let be = Breakeven::at_half();
        let (entry, tp) = (5.916, 5.766);
        let level = be.arms_at(entry, tp);
        assert!((level - 5.841).abs() < 1e-9, "50% level = {level}");
        // The 06-24 03:00 close of 5.868 is NOT yet past 5.841 (a short arms
        // BELOW the level) — only later closes (5.852, 5.845) cross it.
        assert!(!be.close_arms(Direction::Short, level, 5.868));
        assert!(be.close_arms(Direction::Short, level, 5.840));
        assert!((be.target_stop(entry) - 5.916).abs() < 1e-9);
    }

    #[test]
    fn degenerate_thresholds_clamp_to_default() {
        // 0, >=1, and non-finite all fall back to 0.5 so a bad baked value can
        // never disable the SL (>=1 → never arm) or arm on the entry bar (0).
        for bad in [0.0, 1.0, 1.5, -0.2, f64::NAN, f64::INFINITY] {
            let be = Breakeven { threshold: bad };
            assert!(
                (be.arms_at(1.0, 1.2) - 1.1).abs() < 1e-9,
                "threshold {bad} should clamp to 0.5"
            );
        }
    }

    #[test]
    fn round_trips_through_json() {
        let be = Breakeven { threshold: 0.6 };
        let json = serde_json::to_string(&be).expect("serialise");
        let back: Breakeven = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(be, back);
    }
}
