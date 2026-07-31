//! Should a remembered stop actually be restored, and to what level?
//!
//! # The bug this exists to prevent
//!
//! System 2 (spread-hour widen) remembers a position's original stop, moves the
//! stop away from price for the duration of the spread hour, then restores the
//! remembered original **verbatim**. Verbatim is deliberate and correct for
//! *idempotency*: restoring twice lands on the same number, so a partial widen,
//! a missed tick, or a double-fire all stay consistent. Recomputing `current −
//! widen` would drift.
//!
//! But verbatim assumed System 2 is the **only** writer of that stop. It isn't.
//! System F (break-even, `cron/breakeven_watch.rs`) amends the *same* broker
//! handle on the *same* 900s cadence with no coordination, and System 2's
//! idempotency guard reads `record.applied` — "did I widen?" — not "has the stop
//! moved underneath me?".
//!
//! So this interleaving silently gives back a locked-in break-even:
//!
//! ```text
//!   t0  widen:      SL 1.0950 → 1.0930   (remembered original = 1.0950)
//!   t1  break-even: SL 1.0930 → 1.1000   (price ran; risk now zero)
//!   t2  restore:    SL 1.1000 → 1.0950   ← REVERTS the break-even
//! ```
//!
//! At `t2` the position is back to risking real money on a trade that had
//! already been made free. Nothing errors; the log even says "ok".
//!
//! # The rule
//!
//! **A restore may only move a stop away from break-even, never toward the
//! loss side.** Restoring is *giving back* a protective widen, so the only
//! legitimate direction is back toward the original — and only from a stop that
//! is still at-or-worse than that original. A stop that has since moved
//! *tighter* than the remembered original was moved by somebody else with better
//! information (break-even, a future trailing stop), and that decision wins.
//!
//! Expressed per direction, where "tighter" means closer to / beyond entry:
//!
//! - **Long** — stop sits below price. Widening moved it DOWN. So restore only
//!   if `current < original` (still widened); skip if `current > original`
//!   (someone raised it — that's a break-even or a trail).
//! - **Short** — mirrored. Widening moved it UP. Restore only if
//!   `current > original`; skip if `current < original`.
//!
//! # Why this is pure, and here
//!
//! The decision is a function of three numbers and a direction, so it is
//! unit-testable without a broker and **mutation-testable** (flip a comparison
//! and a test must go red). Living in `core` means the live cron and the offline
//! replay reach the same verdict from one implementation, per
//! `[[strategy_changes_in_both_replayer_and_worker]]`.

use crate::intent::Direction;

/// What the restore pass should do with one remembered stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RestoreDecision {
    /// Amend the stop back to this price. The value is the remembered original,
    /// verbatim — never recomputed — so restoring twice is idempotent.
    RestoreTo(f64),
    /// Leave the stop exactly where it is. Carries why, for the log line.
    Skip(SkipReason),
}

/// Why a remembered stop was left alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkipReason {
    /// The live stop is already **tighter** than the remembered original, so
    /// another system (break-even, or a future trailing stop) moved it after the
    /// widen. Restoring would move the stop back toward the loss side and undo
    /// that protection. This is the case the module exists for.
    AlreadyTighter,
    /// The live stop already equals the remembered original — the restore has
    /// happened (or the widen never landed). A no-op amend is pointless traffic.
    AlreadyAtOriginal,
    /// A non-finite input (`NaN`/`∞`) on either stop. Unjudgeable, so do
    /// nothing rather than amend to or from a garbage level — the same
    /// fail-closed discipline `sl_spread_floor_violation` applies to a
    /// degenerate spread.
    Unjudgeable,
}

/// Prices within this fraction of each other count as equal, so floating-point
/// round-tripping through the broker and JSON doesn't produce a pointless amend
/// (or, worse, read as "tighter" and skip a genuine restore).
///
/// Relative rather than absolute because instrument prices span ~0.6 (a minor
/// FX cross) to ~40000 (an index), and a fixed epsilon can't serve both. Matches
/// the tolerant-compare discipline the golden fixtures use
/// (`[[golden_fixture_compare_is_tolerant]]`).
const RELATIVE_EPSILON: f64 = 1e-9;

/// Are two stop prices the same level, allowing for float round-tripping?
fn same_level(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= RELATIVE_EPSILON * scale
}

/// The stop level as it **actually sits at the broker right now**, read back
/// from `list_open_positions`.
///
/// A newtype rather than a bare `f64` because [`restore_decision`]'s two price
/// arguments would otherwise be interchangeable at the call site: swapping them
/// compiles silently and inverts every verdict, restoring precisely the
/// break-evens this module exists to protect. With distinct types that swap is a
/// compile error, so the guarantee doesn't rest on a reviewer noticing argument
/// order — the same compile-time-key reasoning as [`crate::hold::HoldReason`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurrentStop(pub f64);

/// The pre-widen stop level System 2 remembered, and the only level a restore
/// may move back to. See [`CurrentStop`] for why this is a newtype.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OriginalStop(pub f64);

/// Decide whether to restore a widened stop to its remembered original, given
/// where the stop **actually sits right now** at the broker.
///
/// [`CurrentStop`] is the live level read back from the broker, NOT the level we
/// believe we set — reading it back is the entire point, since the whole failure
/// mode is another system having moved it.
///
/// # Direction
///
/// `direction` is the position's, and fixes which side is "tighter":
/// a long's stop is below price (tighter = higher), a short's above
/// (tighter = lower).
pub fn restore_decision(
    direction: Direction,
    current: CurrentStop,
    original: OriginalStop,
) -> RestoreDecision {
    let (current_stop, original_stop) = (current.0, original.0);
    if !current_stop.is_finite() || !original_stop.is_finite() {
        return RestoreDecision::Skip(SkipReason::Unjudgeable);
    }
    if same_level(current_stop, original_stop) {
        return RestoreDecision::Skip(SkipReason::AlreadyAtOriginal);
    }
    // "Tighter" = moved toward/past entry, i.e. the protective direction.
    // Long: entry is above the stop, so tighter is UP.
    // Short: entry is below the stop, so tighter is DOWN.
    let current_is_tighter = match direction {
        Direction::Long => current_stop > original_stop,
        Direction::Short => current_stop < original_stop,
    };
    if current_is_tighter {
        RestoreDecision::Skip(SkipReason::AlreadyTighter)
    } else {
        // Still at-or-wider than the original ⇒ this is our widen to give back.
        // Restore the remembered value VERBATIM — see the module docs on why
        // this must not be recomputed.
        RestoreDecision::RestoreTo(original_stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the bug this module exists to prevent -----------------------------

    /// The exact interleaving from the module docs: widen, then break-even,
    /// then restore. The break-even MUST survive.
    ///
    /// Mutation check: flip the `Long` comparison to `<` and this goes red.
    #[test]
    fn long_break_even_survives_a_restore() {
        // Long: original SL 1.0950, widened down to 1.0930, then break-even
        // moved it UP to 1.1000 (above the original — risk is now zero).
        let out = restore_decision(Direction::Long, CurrentStop(1.1000), OriginalStop(1.0950));
        assert_eq!(
            out,
            RestoreDecision::Skip(SkipReason::AlreadyTighter),
            "restoring would revert a break-even to a real-money stop"
        );
    }

    #[test]
    fn short_break_even_survives_a_restore() {
        // Short mirror: original SL 1.0950, widened UP, break-even moved it
        // DOWN to 1.0900 (below the original).
        let out = restore_decision(Direction::Short, CurrentStop(1.0900), OriginalStop(1.0950));
        assert_eq!(out, RestoreDecision::Skip(SkipReason::AlreadyTighter));
    }

    // ---- the normal path still works ---------------------------------------

    #[test]
    fn long_still_widened_is_restored_verbatim() {
        // Nobody else touched it: the stop is still where the widen put it
        // (1.0930, below the 1.0950 original) → give the widen back.
        let out = restore_decision(Direction::Long, CurrentStop(1.0930), OriginalStop(1.0950));
        assert_eq!(out, RestoreDecision::RestoreTo(1.0950));
    }

    #[test]
    fn short_still_widened_is_restored_verbatim() {
        // Short widen moved the stop UP to 1.0970; original was 1.0950.
        let out = restore_decision(Direction::Short, CurrentStop(1.0970), OriginalStop(1.0950));
        assert_eq!(out, RestoreDecision::RestoreTo(1.0950));
    }

    /// The restored value is the remembered original **exactly**, never
    /// recomputed — so a second restore is a no-op rather than a drift.
    #[test]
    fn restore_is_idempotent() {
        let first = restore_decision(Direction::Long, CurrentStop(1.0930), OriginalStop(1.0950));
        let RestoreDecision::RestoreTo(level) = first else {
            panic!("expected a restore, got {first:?}");
        };
        assert_eq!(level, 1.0950, "must be the remembered value bit-for-bit");
        // Feeding the restored level back in yields "already there", not a
        // second amend and not a drift.
        assert_eq!(
            restore_decision(Direction::Long, CurrentStop(level), OriginalStop(1.0950)),
            RestoreDecision::Skip(SkipReason::AlreadyAtOriginal),
        );
    }

    // ---- degenerate inputs --------------------------------------------------

    #[test]
    fn non_finite_is_unjudgeable_not_a_restore() {
        for (cur, orig) in [
            (f64::NAN, 1.0950),
            (1.0930, f64::NAN),
            (f64::INFINITY, 1.0950),
            (1.0930, f64::NEG_INFINITY),
        ] {
            assert_eq!(
                restore_decision(Direction::Long, CurrentStop(cur), OriginalStop(orig)),
                RestoreDecision::Skip(SkipReason::Unjudgeable),
                "cur={cur} orig={orig}",
            );
        }
    }

    #[test]
    fn float_round_trip_reads_as_already_at_original() {
        // A price that survived a JSON round-trip and came back 1 ULP off must
        // NOT read as "tighter" (which would skip a genuine restore) nor
        // trigger a pointless amend.
        let original = 1.0950_f64;
        let jittered = original + f64::EPSILON;
        assert_eq!(
            restore_decision(
                Direction::Long,
                CurrentStop(jittered),
                OriginalStop(original)
            ),
            RestoreDecision::Skip(SkipReason::AlreadyAtOriginal),
        );
        assert_eq!(
            restore_decision(
                Direction::Short,
                CurrentStop(jittered),
                OriginalStop(original)
            ),
            RestoreDecision::Skip(SkipReason::AlreadyAtOriginal),
        );
    }

    /// The epsilon is *relative*, so it must behave the same on an index at
    /// ~40000 as on an FX cross at ~0.65 — a fixed epsilon cannot do both.
    #[test]
    fn tolerance_scales_with_price() {
        // Index-scale: a 0.5-point genuine break-even move on a 40000 index is
        // far above the relative epsilon → still detected as tighter.
        assert_eq!(
            restore_decision(Direction::Long, CurrentStop(40000.5), OriginalStop(40000.0)),
            RestoreDecision::Skip(SkipReason::AlreadyTighter),
        );
        // ...while a 1-ULP jitter at that scale is absorbed.
        let idx = 40000.0_f64;
        assert_eq!(
            restore_decision(
                Direction::Long,
                CurrentStop(idx + idx * 1e-12),
                OriginalStop(idx)
            ),
            RestoreDecision::Skip(SkipReason::AlreadyAtOriginal),
        );
    }

    /// Direction is load-bearing: the SAME pair of numbers must produce
    /// OPPOSITE verdicts for a long and a short. This is the sign-bug guard —
    /// swap the match arms and it goes red.
    #[test]
    fn direction_inverts_the_verdict() {
        let (current, original) = (1.1000, 1.0950);
        assert_eq!(
            restore_decision(
                Direction::Long,
                CurrentStop(current),
                OriginalStop(original)
            ),
            RestoreDecision::Skip(SkipReason::AlreadyTighter),
            "for a LONG, a higher stop is tighter",
        );
        assert_eq!(
            restore_decision(
                Direction::Short,
                CurrentStop(current),
                OriginalStop(original)
            ),
            RestoreDecision::RestoreTo(original),
            "for a SHORT, a higher stop is still widened",
        );
    }
}
