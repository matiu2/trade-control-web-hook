//! When a System-2 widened stop may be put back — the **gated** backstop.
//!
//! # The divergence this closes
//!
//! There are two ways a widened stop gets restored:
//!
//! 1. **Recovery** (the normal path) — the measured spread has dropped back to
//!    normal, so the shield is no longer needed.
//! 2. **The 12h safety backstop** (last resort) — a record is still `applied` a
//!    very long time after it opened, i.e. recovery never fired: a quote-error
//!    storm, a repeatedly-failing clear, or a mis-baked over-long mask.
//!
//! The backstop exists to unstick a *stuck record*, never to end a legitimate
//! block. The live side says so in its own docs — "it cannot force-restore into
//! an active block the way the old 3h ceiling did" — and earns that by checking
//! the block is over before it fires.
//!
//! The replay's reconstruction did not. It asked only `bar.time >= widen + 12h`,
//! so a bar that was *itself* inside a spread hour could satisfy the backstop and
//! restore the narrow stop into the very spike the widen was protecting against.
//!
//! **Wall-clock is the trap.** Twelve hours of wall-clock is not twelve hours of
//! market. Across a weekend the candle path jumps from Friday to Sunday, so the
//! next bar after the widen can be days later in wall-clock while being the
//! *immediately following bar* in market time. The backstop — sized to outlast
//! "any realistic block" — then fires having observed no market at all.
//!
//! Measured on the AUD/NZD 2026-06-11 strategy-v2 fixture: widen 06-12T20:00Z,
//! `safety_at` 06-13T08:00Z, and the next bar in the window is **06-14T21:00Z** —
//! past the timer, and carrying an **18-pip spread** because it is the NY-close
//! spread hour. The backstop restored the stop to 1.20734 on that bar,
//! `bid_l = 1.20645` took it out, and the fixture booked **−1.00R**. The widened
//! level 1.20553 is not reached by any bar in the whole window: with the shield
//! held, the trade runs to TP for **+1.18R**. A 2.18R swing, and the fixture
//! looked like an ordinary stop-out.
//!
//! # The rule
//!
//! [`backstop_restore_allowed`] is the one predicate both halves ask. The timer
//! being due is necessary but **not sufficient**: a bar that is itself a spread
//! hour can never be the restore bar. That is the same `!is_spread_hour` gate the
//! live call site applies, lifted to a named function so the replay cannot forget
//! it — which is exactly how it came to diverge in the first place.

use chrono::{DateTime, Utc};

use crate::spread_blackout::is_spread_hour;

/// May the 12h safety backstop restore a widened stop at `at`?
///
/// `timer_due` is the pure clock half (`now >= opened_at + 12h`, i.e.
/// [`crate::pending_lifecycle::backstop_due`]); this function adds the gate that
/// keeps it from firing inside an active block.
///
/// Returns `false` while `at` sits in one of `instrument`'s learned spread hours,
/// no matter how overdue the timer is. A stuck record stays stuck for the rest of
/// the block and clears on the first bar past it — strictly better than handing
/// the position a narrow stop mid-spike.
pub fn backstop_restore_allowed(instrument: &str, at: DateTime<Utc>, timer_due: bool) -> bool {
    timer_due && !is_spread_hour(instrument, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("bad test timestamp {s}: {e}"))
            .with_timezone(&Utc)
    }

    /// AUD/NZD is flagged at NY local 17:00 = 21:00Z during US DST. The exact bar
    /// that force-restored in the fixture must now be refused.
    #[test]
    fn the_backstop_may_not_restore_onto_a_spread_hour_bar() {
        let spike = t("2026-06-14T21:00:00Z");
        assert!(
            is_spread_hour("AUD_NZD", spike),
            "precondition: 21:00Z is AUD/NZD's flagged NY-close hour"
        );
        assert!(
            !backstop_restore_allowed("AUD_NZD", spike, true),
            "an overdue timer must NOT restore into an active spread hour — \
             this is the -1.00R the AUD/NZD 2026-06-11 fixture booked"
        );
    }

    /// The bar after the block is a legitimate restore.
    #[test]
    fn the_backstop_restores_on_the_first_bar_past_the_block() {
        let after = t("2026-06-14T22:00:00Z");
        assert!(
            !is_spread_hour("AUD_NZD", after),
            "precondition: 22:00Z is clear"
        );
        assert!(backstop_restore_allowed("AUD_NZD", after, true));
    }

    /// The gate only ever *withholds*. It can't manufacture a restore the timer
    /// hasn't earned.
    #[test]
    fn a_clear_bar_still_needs_the_timer_to_be_due() {
        let clear = t("2026-06-14T22:00:00Z");
        assert!(
            !backstop_restore_allowed("AUD_NZD", clear, false),
            "no timer, no restore — the gate narrows, never widens"
        );
    }

    /// An instrument with no flagged hours is governed by the timer alone, so the
    /// gate can't strand a widen that has no block to wait out.
    #[test]
    fn an_unflagged_instrument_is_governed_by_the_timer_alone() {
        let spike = t("2026-06-14T21:00:00Z");
        if !is_spread_hour("XYZ_ABC", spike) {
            assert!(backstop_restore_allowed("XYZ_ABC", spike, true));
        }
    }
}
