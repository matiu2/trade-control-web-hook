//! A **weekday-aware** market-hours blackout mask.
//!
//! # Why this replaces the minute-of-day window
//!
//! The old [`NoEntryWindow`](super::NoEntryWindow) is a *minute-of-day* window
//! with no concept of the weekday, so a gap that only really happens once a week
//! (a Friday→Monday weekend close) was — via the day-blind gate — applied to
//! that clock-time **every** weekday. That is exactly the bug that rejected a
//! legitimate mid-week EUR/USD entry at 17:00 UTC on a Tuesday (the broker's
//! session string carried a 5-minute daily housekeeping gap that buffered into a
//! phantom multi-hour daily blackout). See the
//! `market-hours-blackout-weekly-gap-bug` memory.
//!
//! [`WeekMask`] fixes the model: it blocks specific `(weekday, minute)` cells of
//! the week, so a weekend close blocks only Friday-evening→Monday-morning and a
//! genuine daily close blocks its own hour on the days the market actually
//! trades. The gate indexes the mask by the UTC instant's weekday and
//! minute-of-day — one array lookup, still zero timezone math in the caller.
//!
//! # Representation
//!
//! One bit per minute of the week: `7 × 1440 = 10_080` cells, day 0 = Monday
//! (matching [`chrono::Weekday::num_days_from_monday`]). Stored as a `[bool;
//! 10_080]` for clarity and O(1) lookup; ~10 KB, trivially cheap to bake.
//! Overlapping blocked spans simply set the same bits, so merging is free.

use chrono::{DateTime, Datelike, Timelike, Utc};

use super::MINUTES_PER_DAY;

/// Minutes in a week — the size of the mask.
pub const MINUTES_PER_WEEK: usize = 7 * MINUTES_PER_DAY as usize;

/// A blackout mask over the whole week at one-minute resolution. Cell
/// `day * 1440 + minute` is `true` when entry is blocked at that UTC weekday +
/// minute-of-day (day 0 = Monday).
///
/// Build one with [`WeekMask::empty`] then [`block_span`](Self::block_span) /
/// [`block_daily`](Self::block_daily); query it with
/// [`is_blocked_at`](Self::is_blocked_at). Serialises as its raw cell vector so a
/// baked/stored mask round-trips exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekMask {
    /// `blocked[day*1440 + minute]` — `true` = entry blocked. Length is always
    /// [`MINUTES_PER_WEEK`].
    blocked: Vec<bool>,
}

impl Default for WeekMask {
    fn default() -> Self {
        Self::empty()
    }
}

impl WeekMask {
    /// A mask that blocks nothing.
    pub fn empty() -> Self {
        Self {
            blocked: vec![false; MINUTES_PER_WEEK],
        }
    }

    /// The linear week-minute index for a `(weekday, minute-of-day)` pair.
    /// `weekday` is days-from-Monday (0..=6); `minute` is 0..1440. Both are
    /// reduced into range so caller arithmetic that overshoots wraps cleanly.
    fn cell(weekday: u32, minute: u32) -> usize {
        let wd = (weekday % 7) as usize;
        let m = (minute % MINUTES_PER_DAY) as usize;
        wd * MINUTES_PER_DAY as usize + m
    }

    /// Block a contiguous span of the week from `(from_weekday, from_min)` up to
    /// **but not including** `(to_weekday, to_min)`, walking forward minute by
    /// minute and **wrapping around the end of the week** (Sunday→Monday). This
    /// is how the weekend blackout is expressed: `block_span(Fri, close, Mon,
    /// open)` blocks Friday-evening through Monday-morning across the week
    /// boundary. A zero-length span (same start and end cell) blocks nothing.
    ///
    /// `from_weekday` / `to_weekday` are days-from-Monday (0=Mon … 6=Sun).
    pub fn block_span(&mut self, from_weekday: u32, from_min: u32, to_weekday: u32, to_min: u32) {
        let start = Self::cell(from_weekday, from_min);
        let end = Self::cell(to_weekday, to_min);
        // Walk [start, end) on the week ring. start == end ⇒ empty span (nothing
        // blocked), NOT the whole week — matching the half-open convention.
        let mut i = start;
        while i != end {
            self.blocked[i] = true;
            i = (i + 1) % MINUTES_PER_WEEK;
        }
    }

    /// Block a daily minute-of-day range `[from_min, to_min)` on **every** day of
    /// the week (a recurring daily close, e.g. a cash-index overnight gap). Wraps
    /// midnight per day when `from_min > to_min` (e.g. 22:00→02:00). This is the
    /// day-recurring counterpart to [`block_span`](Self::block_span): a daily
    /// close happens on each trading day, and blocking it on weekend days too is
    /// harmless (the market is already weekend-blocked there).
    pub fn block_daily(&mut self, from_min: u32, to_min: u32) {
        for wd in 0..7u32 {
            let mut m = from_min % MINUTES_PER_DAY;
            let to = to_min % MINUTES_PER_DAY;
            // Empty range (from == to) blocks nothing, matching half-open [).
            while m != to {
                self.blocked[Self::cell(wd, m)] = true;
                m = (m + 1) % MINUTES_PER_DAY;
            }
        }
    }

    /// Is entry blocked at this UTC instant? Reads the instant's weekday
    /// (days-from-Monday) and minute-of-day and returns the mask cell. Seconds
    /// are ignored (minute resolution). This is the single query the reject gate
    /// and sweep call — no timezone math, one array index.
    pub fn is_blocked_at(&self, now: DateTime<Utc>) -> bool {
        let weekday = now.weekday().num_days_from_monday();
        let minute = now.hour() * 60 + now.minute();
        self.blocked[Self::cell(weekday, minute)]
    }

    /// True when the mask blocks no minute at all — the fail-open case (an
    /// instrument with no baked blackout).
    pub fn is_empty(&self) -> bool {
        !self.blocked.iter().any(|b| *b)
    }

    /// Count of blocked minutes in the week — for tests / diagnostics.
    pub fn blocked_minutes(&self) -> usize {
        self.blocked.iter().filter(|b| **b).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A UTC instant on a known weekday. `2026-07-06` is a **Monday**, so
    /// `mon + n days` lands on a predictable weekday.
    fn at(day_offset: i64, hour: u32, minute: u32) -> DateTime<Utc> {
        let mon = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        mon + chrono::Duration::days(day_offset)
            + chrono::Duration::hours(hour as i64)
            + chrono::Duration::minutes(minute as i64)
    }

    #[test]
    fn empty_mask_blocks_nothing() {
        let m = WeekMask::empty();
        assert!(m.is_empty());
        assert!(!m.is_blocked_at(at(0, 12, 0)));
        assert_eq!(m.blocked_minutes(), 0);
    }

    /// THE BUG CASE: a weekend-only block must NOT block a mid-week bar at the
    /// same clock time. Block Fri 21:00 → Mon 22:00, then check Tuesday 17:00 UTC
    /// (the exact EUR/USD bar that was wrongly rejected) is ALLOWED.
    #[test]
    fn weekend_block_does_not_touch_midweek_same_clock_time() {
        let mut m = WeekMask::empty();
        // Fri (day 4) 21:00 → Mon (day 0) 22:00.
        m.block_span(4, 21 * 60, 0, 22 * 60);
        // Tuesday (day 1) 17:00 UTC — the bug bar. Must be allowed.
        assert!(
            !m.is_blocked_at(at(1, 17, 0)),
            "mid-week must not be blocked"
        );
        // And any weekday clock time that isn't the weekend span is allowed.
        assert!(!m.is_blocked_at(at(2, 21, 30)), "Wed 21:30 allowed");
    }

    /// The weekend span itself IS blocked, across the week boundary.
    #[test]
    fn weekend_span_blocks_friday_evening_through_monday_morning() {
        let mut m = WeekMask::empty();
        m.block_span(4, 21 * 60, 0, 22 * 60); // Fri 21:00 → Mon 22:00
        assert!(m.is_blocked_at(at(4, 21, 0)), "Fri 21:00 blocked (start)");
        assert!(m.is_blocked_at(at(4, 23, 30)), "Fri late blocked");
        assert!(m.is_blocked_at(at(5, 12, 0)), "Saturday blocked");
        assert!(m.is_blocked_at(at(6, 12, 0)), "Sunday blocked");
        assert!(
            m.is_blocked_at(at(0, 21, 59)),
            "Mon 21:59 blocked (in span)"
        );
        // Half-open: reopens exactly at Mon 22:00.
        assert!(!m.is_blocked_at(at(0, 22, 0)), "Mon 22:00 reopened");
        assert!(
            !m.is_blocked_at(at(4, 20, 59)),
            "Fri 20:59 before span allowed"
        );
    }

    /// A daily close blocks its hour on every day (here we only assert it hits
    /// several distinct weekdays at the same clock time — the recurring case).
    #[test]
    fn daily_block_hits_every_weekday_at_that_hour() {
        let mut m = WeekMask::empty();
        m.block_daily(19 * 60, 20 * 60); // block 19:00–20:00 daily
        for day in 0..7 {
            assert!(m.is_blocked_at(at(day, 19, 30)), "day {day} 19:30 blocked");
            assert!(!m.is_blocked_at(at(day, 18, 30)), "day {day} 18:30 allowed");
            assert!(!m.is_blocked_at(at(day, 20, 0)), "day {day} 20:00 reopened");
        }
    }

    /// A daily block that wraps midnight (22:00→02:00) blocks late one day and
    /// early the next.
    #[test]
    fn daily_block_wraps_midnight() {
        let mut m = WeekMask::empty();
        m.block_daily(22 * 60, 2 * 60);
        assert!(m.is_blocked_at(at(2, 23, 0)), "Wed 23:00 blocked");
        assert!(m.is_blocked_at(at(2, 1, 0)), "Wed 01:00 blocked (wrap)");
        assert!(!m.is_blocked_at(at(2, 3, 0)), "Wed 03:00 allowed");
    }

    /// A zero-length span blocks nothing (guards the `start == end` case so it
    /// doesn't accidentally block the whole week).
    #[test]
    fn zero_length_span_blocks_nothing() {
        let mut m = WeekMask::empty();
        m.block_span(4, 21 * 60, 4, 21 * 60);
        assert!(m.is_empty(), "empty span must not block the whole week");
    }

    /// Overlapping blocks merge for free (idempotent bit-setting).
    #[test]
    fn overlapping_blocks_are_idempotent() {
        let mut m = WeekMask::empty();
        m.block_daily(19 * 60, 21 * 60);
        let after_first = m.blocked_minutes();
        m.block_daily(20 * 60, 22 * 60); // overlaps 20:00–21:00
        // The union is 19:00–22:00 daily = 180 min × 7 days.
        assert_eq!(m.blocked_minutes(), 180 * 7);
        assert!(m.blocked_minutes() > after_first);
    }
}
