//! **Why a trade's resting orders are currently held** — the shared refcount
//! behind the resting-order lifecycle.
//!
//! Several independent conditions can each want a resting entry order pulled off
//! the broker: an instrument's baked spread-hour trough, a news standoff for the
//! trade, the instrument's market being closed. They **overlap** and they **lift
//! independently** — a spread hour running 06:30–08:00 with a news pause from
//! 07:00 leaves the pause still armed when the spread lifts at 08:00. Whoever
//! restores the order has to know the other holder is still there.
//!
//! So a reason does not directly cancel or restore. It **holds** and **releases**,
//! and the order is re-placed on the [`Holders::release`] that empties the set.
//!
//! # Why a typed key, not a string
//!
//! `hold("spread-hour")` paired with `release("spread_hour")` is a typo that
//! strands an order until the 12h backstop: the release silently no-ops and the
//! set never empties. [`HoldReason`] is a **closed enum**, so every site names a
//! variant the compiler checks — and adding a third reason is a compile error at
//! each place that must decide its release condition, which is the property worth
//! having. Nothing constructs a reason from a runtime string.
//!
//! # Why a set, not a counter
//!
//! The cron **re-evaluates the same conditions every ~5s**. A bare
//! `if spread_hour { count += 1 }` reaches 200 within the hour and never returns to
//! zero, so the order would never be restored. [`Holders::hold`] is **idempotent**:
//! re-holding a reason already present is a no-op, which is what makes a polling
//! driver safe. The count the operator reasons about is [`Holders::len`], and it
//! goes 1 → 2 → 1 → 0 as reasons overlap and lift.

use serde::{Deserialize, Serialize};

/// One reason a trade's resting orders are held.
///
/// **Closed on purpose** — there is deliberately no `Unknown(String)` arm. In code
/// the situation cannot arise (no reason is ever built from a runtime string), so
/// such a variant would be dead code plus a state that can't happen. The one
/// boundary the compiler doesn't cross is deserialization of a record body written
/// by a different build; that surfaces as a loud decode error on that record
/// rather than a silently-tolerated holder, and the 12h backstop still frees the
/// order. See `SCOPING-hold-refcount.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldReason {
    /// The instrument entered its baked spread-hour trough. Instrument-scoped.
    ///
    /// Releases on the documented ON/OFF asymmetry — the baked hour ending **or**
    /// the live spread recovering (which can un-block early, still inside the
    /// nominal hour). See `crate::spread_blackout`.
    SpreadHour,
    /// A news standoff (`pause`) is armed for this trade. Trade-scoped, because a
    /// pause is keyed `(trade_id, blackout_id)` and carries no instrument.
    ///
    /// Releases when no pause row remains for the trade.
    NewsPause,
    /// The instrument's market is closed — the daily close→open gap, or the
    /// weekend halt. Instrument-scoped, read from the baked
    /// [`WeekMask`](crate::intent::WeekMask).
    ///
    /// Held rather than cancelled-and-deleted because **a closed market always
    /// reopens**. The order must come off the broker (leaving it to rest is the
    /// reopen-gap incident: it triggers on the opening gap, at a price nobody
    /// chose) — but the *setup* is still valid, so it is restored when the
    /// session resumes. The sweep's other three reasons (`expired`,
    /// `bar-expiry`, `sl-breached`) are genuinely terminal and stay as
    /// cancel-and-delete; this one never was.
    ///
    /// Releases when the baked mask says the market has reopened.
    MarketHours,
}

impl HoldReason {
    /// Stable operator-facing label, for log lines and `status` output. Matches
    /// the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpreadHour => "spread-hour",
            Self::NewsPause => "news-pause",
            Self::MarketHours => "market-hours",
        }
    }
}

impl std::fmt::Display for HoldReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The set of reasons currently holding a trade's resting orders.
///
/// Set semantics over a **sorted `Vec`**: the stored `jsonb` array is stable, so
/// the record body doesn't churn between ticks and replay-fixture diffs stay
/// clean. A linear scan is free at the 2–5 reasons this will ever hold.
///
/// Invariant: **sorted and deduplicated at all times**. [`hold`](Self::hold) and
/// [`release`](Self::release) are the only mutators and both preserve it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Holders(Vec<HoldReason>);

impl Holders {
    /// An empty holder set — nothing wants the order held.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add `reason` to the set. **Idempotent** — returns `true` only if it was
    /// newly added, so a caller can distinguish "I just took the hold" (do the
    /// broker cancel) from "already held" (nothing to do). This is what makes the
    /// ~5s polling cron safe: re-holding every tick is a no-op.
    pub fn hold(&mut self, reason: HoldReason) -> bool {
        match self.0.binary_search(&reason) {
            Ok(_) => false,
            Err(at) => {
                self.0.insert(at, reason);
                true
            }
        }
    }

    /// Remove `reason` from the set.
    ///
    /// Returns [`Release::Emptied`] **only on the release that takes the set to
    /// zero** — that transition is the restore trigger. Releasing a reason that
    /// wasn't held, or one that leaves others behind, never reports `Emptied`, so
    /// the restore fires exactly once per hold episode and a tick that observes an
    /// already-empty set does not re-place anything.
    pub fn release(&mut self, reason: HoldReason) -> Release {
        let Ok(at) = self.0.binary_search(&reason) else {
            return Release::NotHeld;
        };
        self.0.remove(at);
        if self.0.is_empty() {
            Release::Emptied
        } else {
            Release::StillHeld
        }
    }

    /// Is `reason` currently holding?
    pub fn contains(&self, reason: HoldReason) -> bool {
        self.0.binary_search(&reason).is_ok()
    }

    /// How many reasons are holding — the operator-facing count.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when nothing holds the order.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The holding reasons, ascending. For log lines and `status`.
    pub fn iter(&self) -> impl Iterator<Item = HoldReason> + '_ {
        self.0.iter().copied()
    }

    /// Comma-joined labels for a log line, e.g. `"news-pause,spread-hour"`.
    /// `"none"` when empty, so a log line never renders as an empty string.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "none".to_string();
        }
        self.0
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// What a [`Holders::release`] did — specifically, whether it was the one that
/// emptied the set (the restore trigger).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// The set is now empty: **restore the order**. Fires exactly once per hold
    /// episode, on the transition.
    Emptied,
    /// Released, but other reasons still hold. Leave the order pulled.
    StillHeld,
    /// This reason wasn't holding — nothing changed. Notably NOT `Emptied` even if
    /// the set is empty, so an already-restored record can't restore twice.
    NotHeld,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_is_idempotent_so_a_polling_driver_is_safe() {
        let mut h = Holders::new();
        assert!(h.hold(HoldReason::SpreadHour), "first hold is new");
        // The cron re-evaluates every ~5s: 200 more holds must not accumulate.
        for _ in 0..200 {
            assert!(
                !h.hold(HoldReason::SpreadHour),
                "re-holding an existing reason is a no-op"
            );
        }
        assert_eq!(
            h.len(),
            1,
            "a counter would read 201 here and never reach 0"
        );
        // ...and ONE release still empties it. This is the property a bare
        // integer refcount would fail.
        assert_eq!(h.release(HoldReason::SpreadHour), Release::Emptied);
        assert!(h.is_empty());
    }

    #[test]
    fn overlapping_reasons_lift_independently() {
        let mut h = Holders::new();
        h.hold(HoldReason::SpreadHour);
        h.hold(HoldReason::NewsPause);
        assert_eq!(h.len(), 2);
        // The operator's case: the spread hour lifts while the pause is still on.
        assert_eq!(
            h.release(HoldReason::SpreadHour),
            Release::StillHeld,
            "the pause still holds — must NOT restore yet"
        );
        assert_eq!(h.len(), 1);
        assert!(h.contains(HoldReason::NewsPause));
        // Only when the last one lifts does the restore fire.
        assert_eq!(h.release(HoldReason::NewsPause), Release::Emptied);
        assert!(h.is_empty());
    }

    #[test]
    fn release_reports_emptied_only_on_the_transition() {
        let mut h = Holders::new();
        h.hold(HoldReason::NewsPause);
        assert_eq!(h.release(HoldReason::NewsPause), Release::Emptied);
        // A second tick observing the empty set must NOT re-trigger a restore.
        assert_eq!(
            h.release(HoldReason::NewsPause),
            Release::NotHeld,
            "an already-empty set must not report Emptied again"
        );
    }

    #[test]
    fn releasing_a_reason_that_never_held_is_not_a_restore() {
        let mut h = Holders::new();
        h.hold(HoldReason::SpreadHour);
        assert_eq!(
            h.release(HoldReason::NewsPause),
            Release::NotHeld,
            "releasing an unheld reason changes nothing"
        );
        assert_eq!(h.len(), 1, "the real holder survives");
        assert!(h.contains(HoldReason::SpreadHour));
    }

    #[test]
    fn serialises_as_a_stable_sorted_array() {
        let mut h = Holders::new();
        // Insert out of order; the body must still be deterministic.
        h.hold(HoldReason::NewsPause);
        h.hold(HoldReason::SpreadHour);
        let json = serde_json::to_string(&h).expect("serialise");
        assert_eq!(
            json, r#"["spread-hour","news-pause"]"#,
            "sorted by declaration order (SpreadHour < NewsPause), stable across ticks"
        );
        let back: Holders = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, h);
    }

    #[test]
    fn absent_field_decodes_to_empty() {
        // The `#[serde(default)]` path on HeldTradeRecord: a pre-v120 row has
        // no `holders` key at all.
        let h: Holders = serde_json::from_str("[]").expect("empty array");
        assert!(h.is_empty());
        assert_eq!(h.describe(), "none");
    }

    #[test]
    fn unknown_reason_is_a_loud_decode_error_not_a_silent_drop() {
        // The one boundary the compiler can't police: a body written by another
        // build. It must FAIL rather than decode to a holder-less record (which
        // would restore the order blind on the next tick).
        let err = serde_json::from_str::<Holders>(r#"["spread-hour","from-the-future"]"#);
        assert!(
            err.is_err(),
            "an unrecognised reason must be a decode error, never silently dropped"
        );
    }

    #[test]
    fn describe_lists_every_holder() {
        let mut h = Holders::new();
        assert_eq!(h.describe(), "none");
        h.hold(HoldReason::SpreadHour);
        assert_eq!(h.describe(), "spread-hour");
        h.hold(HoldReason::NewsPause);
        assert_eq!(h.describe(), "spread-hour,news-pause");
    }
}
