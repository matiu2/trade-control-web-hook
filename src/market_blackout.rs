//! Market-hours entry blackout — System 1 (the reject gate). Reject a
//! brand-new entry that fires inside a per-instrument close→open gap, so a
//! resting stop order can never be left to trigger on the reopen liquidity
//! gap (the incident this feature fixes).
//!
//! The per-instrument no-entry windows (UTC minute-of-day ranges) are
//! derived daily by the 06:00 UTC cron (`src/cron/blackout_hours.rs`) from
//! the broker's session hours and stored in KV. The pure derivation +
//! `is_inside_any` predicate live in `trade_control_core::intent::blackout`
//! and are unit-tested there. This module holds only the worker-side glue:
//! turning `now` into a UTC minute-of-day, which the gate in `run_enter`
//! (src/lib.rs) feeds to `is_inside_any`.
//!
//! Reject, not delay — exactly like the spread blackout. The next signal
//! bar refires and re-checks; once the market has reopened the same entry
//! passes. Returning `ActionResult::Rejected` is a `Skip` in `seen_decision`
//! (no `mark_seen`), so this reject never poisons the intent id. See
//! CLAUDE.md "Replay protection scope".

use chrono::{DateTime, Timelike, Utc};

/// Minutes-of-day [0, 1440) for `now` in **UTC** — the coordinate the stored
/// [`trade_control_core::intent::NoEntryWindow`]s use. The deriver converts
/// the broker's Brisbane session hours to this same UTC minute-of-day axis,
/// so the gate compares like-for-like.
pub fn now_utc_minute_of_day(now: DateTime<Utc>) -> u32 {
    now.hour() * 60 + now.minute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 18, hour, minute, 0).unwrap()
    }

    #[test]
    fn midnight_is_zero() {
        assert_eq!(now_utc_minute_of_day(at(0, 0)), 0);
    }

    #[test]
    fn one_minute_past_midnight() {
        assert_eq!(now_utc_minute_of_day(at(0, 1)), 1);
    }

    #[test]
    fn noon_is_seven_twenty() {
        assert_eq!(now_utc_minute_of_day(at(12, 0)), 720);
    }

    #[test]
    fn last_minute_of_day() {
        // 23:59 = 1439, strictly inside [0, 1440).
        assert_eq!(now_utc_minute_of_day(at(23, 59)), 1439);
    }

    #[test]
    fn seconds_are_ignored() {
        let with_secs = Utc.with_ymd_and_hms(2026, 6, 18, 9, 30, 45).unwrap();
        assert_eq!(now_utc_minute_of_day(with_secs), 9 * 60 + 30);
    }
}
