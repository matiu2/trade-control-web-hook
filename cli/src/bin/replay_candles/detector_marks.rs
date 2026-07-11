//! Candle-detector marking for the replay: mark **every** bar on which the
//! signal detector prints a pattern, whether or not the plan actually acted on
//! it as an entry.
//!
//! Motivation (operator ask): the normal replay report only ever surfaces a
//! signal when it produced a *fired* intent (an enter). A golden candle the plan
//! couldn't act on — wrong phase, plan not yet `AwaitEntry`, a golden in a
//! multi-shot window the watermark skipped, or a golden *opposite*-direction
//! signal that only invalidates — is invisible. This makes "why didn't this
//! enter fire?" hard to debug. Marking every detected golden candle closes that
//! blind spot.
//!
//! **Parity.** The mark is computed with the SAME two calls the engine's
//! `latched_signal_at` makes per bar — [`detect_at`] over the mid window plus
//! [`wilder_atr`] for the golden test — so a marked golden is exactly what the
//! engine detected (`[[pine_rust_signal_detector_parity]]`): no re-implemented
//! detector to drift. Per the design decision the pattern set is
//! [`DetectFlags::default`] (all five on), independent of the plan's
//! `DetectorConfig`.

use clap::ValueEnum;
use trade_control_engine::intent::Direction;

/// Which detected directions to mark, relative to the plan's trade direction.
/// `none` on this axis (or the golden axis) disables marking entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DirectionFilter {
    /// Only signals in the plan's trade direction — the ones that could have
    /// been entries. The default: this is the "why didn't my entry fire" view.
    With,
    /// Only signals opposite the plan's trade direction (invalidation /
    /// opposing-reversal candidates).
    Against,
    /// Both directions.
    Both,
    /// Disable direction marking (turns the whole feature off).
    None,
}

/// Which golden-ness to mark. `none` (or `none` on the direction axis) disables
/// marking entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GoldenFilter {
    /// Only golden signals (size ≥ ATR at signal time). The default.
    Golden,
    /// Only non-golden signals.
    NonGolden,
    /// Both golden and non-golden.
    Both,
    /// Disable golden marking (turns the whole feature off).
    None,
}

/// The resolved detector-marking configuration, carried into the replay loop and
/// the report. Built from the two CLI flags plus the plan's trade direction (the
/// reference the `with`/`against` filter is relative to).
#[derive(Debug, Clone, Copy)]
pub struct DetectorMarkConfig {
    pub direction: DirectionFilter,
    pub golden: GoldenFilter,
    /// The plan's trade direction — `with` means matching this, `against` means
    /// the opposite.
    pub trade_direction: Direction,
}

impl DetectorMarkConfig {
    pub fn new(
        direction: DirectionFilter,
        golden: GoldenFilter,
        trade_direction: Direction,
    ) -> Self {
        Self {
            direction,
            golden,
            trade_direction,
        }
    }

    /// True when either axis is `none`: the feature is off, no bars are marked
    /// and no summary is printed.
    pub fn is_off(&self) -> bool {
        matches!(self.direction, DirectionFilter::None) || matches!(self.golden, GoldenFilter::None)
    }

    /// Does a detected signal with this direction + golden-ness pass the filter?
    /// Always false when the feature is off.
    pub fn accepts(&self, dir: Direction, is_golden: bool) -> bool {
        if self.is_off() {
            return false;
        }
        let dir_ok = match self.direction {
            DirectionFilter::With => dir == self.trade_direction,
            DirectionFilter::Against => dir != self.trade_direction,
            DirectionFilter::Both => true,
            DirectionFilter::None => false,
        };
        let golden_ok = match self.golden {
            GoldenFilter::Golden => is_golden,
            GoldenFilter::NonGolden => !is_golden,
            GoldenFilter::Both => true,
            GoldenFilter::None => false,
        };
        dir_ok && golden_ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: DirectionFilter, g: GoldenFilter) -> DetectorMarkConfig {
        DetectorMarkConfig::new(d, g, Direction::Long)
    }

    #[test]
    fn none_on_either_axis_is_off() {
        assert!(cfg(DirectionFilter::None, GoldenFilter::Golden).is_off());
        assert!(cfg(DirectionFilter::With, GoldenFilter::None).is_off());
        assert!(!cfg(DirectionFilter::With, GoldenFilter::Golden).is_off());
    }

    #[test]
    fn off_config_accepts_nothing() {
        let c = cfg(DirectionFilter::None, GoldenFilter::Golden);
        assert!(!c.accepts(Direction::Long, true));
        assert!(!c.accepts(Direction::Short, false));
    }

    #[test]
    fn default_with_golden_marks_only_trade_dir_golden() {
        // plan is Long; default view = with-direction golden.
        let c = cfg(DirectionFilter::With, GoldenFilter::Golden);
        assert!(c.accepts(Direction::Long, true), "long golden marked");
        assert!(
            !c.accepts(Direction::Long, false),
            "long non-golden skipped"
        );
        assert!(!c.accepts(Direction::Short, true), "short golden skipped");
    }

    #[test]
    fn against_filter_flips_direction() {
        let c = cfg(DirectionFilter::Against, GoldenFilter::Golden);
        assert!(c.accepts(Direction::Short, true), "opposite golden marked");
        assert!(
            !c.accepts(Direction::Long, true),
            "trade-dir golden skipped"
        );
    }

    #[test]
    fn both_axes_both_marks_every_signal() {
        let c = cfg(DirectionFilter::Both, GoldenFilter::Both);
        assert!(c.accepts(Direction::Long, true));
        assert!(c.accepts(Direction::Long, false));
        assert!(c.accepts(Direction::Short, true));
        assert!(c.accepts(Direction::Short, false));
    }

    #[test]
    fn non_golden_filter_selects_non_golden_only() {
        let c = cfg(DirectionFilter::Both, GoldenFilter::NonGolden);
        assert!(c.accepts(Direction::Long, false));
        assert!(!c.accepts(Direction::Long, true));
    }
}
