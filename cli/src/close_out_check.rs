//! The close-out verdict, shared by arm time and offline replay.
//!
//! # Why this is its own module
//!
//! Two consumers ask the same question at different moments:
//!
//! - **Arm time** (`trade_patterns::check_futures_close_out`) — reading a
//!   [`TradeSpec`](crate::TradeSpec), before a plan exists.
//! - **Offline replay** (`replay-candles`) — reading a signed `TradePlan`,
//!   long after arming.
//!
//! They hold different types and cannot share an input struct, but they must
//! reach the **same verdict**, for the same reason
//! `[[strategy_changes_in_both_replayer_and_worker]]` exists: a check that
//! lives in only one of them is a check the other silently skips. So the
//! decision is a pure function over the three things both can supply —
//! instrument, direction, and the last day the plan can still act — and each
//! caller does its own type-shaping and its own error rendering.
//!
//! # Why replay re-checks at all
//!
//! `tv-arm --plan-out` builds [`Lenient`](crate::BuildStrictness), so a
//! historical setup can still be replayed — which means the close-out guard
//! only **warns** there. A plan handed to the replay has therefore never had
//! this refused, and the replay is the next place a reader could be misled by
//! a backtest of a trade IBKR would have liquidated out from under them.
//!
//! There is an exact precedent: `replay_candles::inverted_window_error`
//! already refuses a window `--plan-out` armed happily, and says so.

use crate::futures_symbol::{self, FuturesContract};
use chrono::NaiveDate;
use trade_control_core::contract_calendar;
use trade_control_core::intent::Direction;

/// What the calendar says about a plan, once its instrument has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutVerdict {
    /// Not a futures contract — every CFD and spot instrument lands here, and
    /// there is nothing to check.
    NotFutures,
    /// A futures contract still inside its arming window.
    Armable {
        contract: FuturesContract,
        /// Last day a plan on this contract may be armed in this direction.
        arm_by: NaiveDate,
    },
    /// A futures contract whose window has closed: the plan can still be
    /// acting after IBKR may begin liquidating it.
    PastArmingWindow {
        contract: FuturesContract,
        /// The last day this plan could have been armed.
        arm_by: NaiveDate,
        /// The day from which IBKR may liquidate without notice.
        close_out: NaiveDate,
        /// The day the plan is still able to act — what pushed it past
        /// `arm_by`.
        acts_until: NaiveDate,
    },
    /// The contract parses but is not in the baked calendar.
    ///
    /// **A refusal, never "no constraint".** If the calendar cannot say when
    /// IBKR would liquidate, nothing downstream may assume it is safe — the
    /// fail-closed contract of `core::contract_calendar`.
    UnknownContract { contract: FuturesContract },
}

/// Ask the calendar about a plan that can still act until `acts_until`.
///
/// `reference_year` resolves the single-digit year in an `GCZ6`-style symbol
/// (pass the year the plan is being judged in); it is unused by the explicit
/// `GC 202612` form.
///
/// `acts_until` is deliberately **the last day the plan can still open a
/// position**, not "now": arming is a licence to enter later, so the question
/// is whether the *end* of the window clears the deadline. Arm time passes the
/// trade expiry; replay passes the end of its replay window.
pub fn verdict(
    instrument: &str,
    direction: Direction,
    acts_until: NaiveDate,
    reference_year: i32,
) -> CloseOutVerdict {
    let Some(contract) = futures_symbol::parse(instrument, reference_year) else {
        return CloseOutVerdict::NotFutures;
    };
    let armable = contract_calendar::is_armable_on(
        &contract.root,
        &contract.contract_month,
        direction,
        acts_until,
    );
    match armable {
        Some(true) => {
            // `is_armable_on` answered, so the row exists and both accessors
            // below resolve. Fall back to `NotFutures`'s neighbour rather than
            // unwrapping: an unknown here would mean the table changed under
            // us mid-call, and a refusal is the safe reading of that.
            match arm_by(&contract, direction) {
                Some(arm_by) => CloseOutVerdict::Armable { contract, arm_by },
                None => CloseOutVerdict::UnknownContract { contract },
            }
        }
        Some(false) => match (
            arm_by(&contract, direction),
            close_out(&contract, direction),
        ) {
            (Some(arm_by), Some(close_out)) => CloseOutVerdict::PastArmingWindow {
                contract,
                arm_by,
                close_out,
                acts_until,
            },
            _ => CloseOutVerdict::UnknownContract { contract },
        },
        None => CloseOutVerdict::UnknownContract { contract },
    }
}

fn arm_by(contract: &FuturesContract, direction: Direction) -> Option<NaiveDate> {
    contract_calendar::arm_by_deadline(&contract.root, &contract.contract_month, direction)
}

fn close_out(contract: &FuturesContract, direction: Direction) -> Option<NaiveDate> {
    contract_calendar::close_out_deadline(&contract.root, &contract.contract_month, direction)
}

impl CloseOutVerdict {
    /// Is this a verdict a caller must refuse or warn about?
    ///
    /// Both [`PastArmingWindow`](Self::PastArmingWindow) and
    /// [`UnknownContract`](Self::UnknownContract) are problems — the second
    /// because an unanswerable deadline is never safe.
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::PastArmingWindow { .. } | Self::UnknownContract { .. }
        )
    }

    /// The contract, when the instrument parsed as one.
    pub fn contract(&self) -> Option<&FuturesContract> {
        match self {
            Self::NotFutures => None,
            Self::Armable { contract, .. }
            | Self::PastArmingWindow { contract, .. }
            | Self::UnknownContract { contract } => Some(contract),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        s.parse().expect("test date")
    }

    /// The baked GC December 2026 row, for reference:
    ///   long  arm-by 2026-11-11, close-out 2026-11-25
    ///   short arm-by 2026-12-10, close-out 2026-12-24
    #[test]
    fn a_cfd_instrument_is_not_futures() {
        for name in ["EUR/USD", "EUR_USD", "Spot Gold", "US 500", "AUD/CAD"] {
            assert_eq!(
                verdict(name, Direction::Long, day("2026-11-20"), 2026),
                CloseOutVerdict::NotFutures,
                "{name} must not parse as futures"
            );
        }
    }

    #[test]
    fn a_contract_inside_its_window_is_armable() {
        let v = verdict("GC 202612", Direction::Long, day("2026-10-01"), 2026);
        assert!(
            matches!(v, CloseOutVerdict::Armable { ref arm_by, .. } if *arm_by == day("2026-11-11")),
            "expected armable with the baked long arm-by, got {v:?}"
        );
        assert!(!v.is_problem());
    }

    #[test]
    fn a_contract_past_its_window_is_flagged() {
        let v = verdict("GC 202612", Direction::Long, day("2026-11-20"), 2026);
        let CloseOutVerdict::PastArmingWindow {
            arm_by, close_out, ..
        } = &v
        else {
            panic!("expected PastArmingWindow, got {v:?}");
        };
        assert_eq!(*arm_by, day("2026-11-11"));
        assert_eq!(*close_out, day("2026-11-25"));
        assert!(v.is_problem());
    }

    /// The month-early trap, at the verdict layer: on one day in between, the
    /// same contract is closed to longs and open to shorts. A
    /// direction-agnostic check would have to be wrong about one of them.
    #[test]
    fn long_and_short_differ_on_the_same_contract_and_day() {
        let mid = day("2026-11-20");
        let long = verdict("GC 202612", Direction::Long, mid, 2026);
        let short = verdict("GC 202612", Direction::Short, mid, 2026);
        assert!(long.is_problem(), "long must be past its window: {long:?}");
        assert!(
            !short.is_problem(),
            "short must still be armable: {short:?}"
        );
    }

    #[test]
    fn an_unlisted_contract_is_a_refusal_not_a_pass() {
        // CL parses as a root but is not one of the four traded contracts.
        let v = verdict("CL 202612", Direction::Long, day("2026-01-01"), 2026);
        assert!(
            matches!(v, CloseOutVerdict::UnknownContract { .. }),
            "an unlisted contract must refuse, got {v:?}"
        );
        assert!(
            v.is_problem(),
            "an unanswerable deadline is never safe to ignore"
        );
    }

    /// The arm-by date itself is still armable — the safety margin already
    /// carries the head-room, so there is no reason to also lose its last day.
    #[test]
    fn the_arm_by_day_itself_is_still_armable() {
        let v = verdict("GC 202612", Direction::Long, day("2026-11-11"), 2026);
        assert!(!v.is_problem(), "the arm-by day must be inclusive: {v:?}");

        let next = verdict("GC 202612", Direction::Long, day("2026-11-12"), 2026);
        assert!(next.is_problem(), "the day after must not be: {next:?}");
    }

    #[test]
    fn the_ibkr_local_symbol_form_resolves_too() {
        let v = verdict("GCZ6", Direction::Long, day("2026-11-20"), 2026);
        assert!(
            matches!(v, CloseOutVerdict::PastArmingWindow { ref contract, .. }
                if contract.contract_month == "202612"),
            "GCZ6 must resolve to the December row, got {v:?}"
        );
    }
}
