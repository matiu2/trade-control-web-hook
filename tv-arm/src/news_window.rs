//! A resolved news / blackout **window** — a `[start, end)` interval at
//! real event-minute precision.
//!
//! History: news and blackout windows used to live on the chart as pairs of
//! drawn vertical lines (`pause`/`resume`, `news-start`/`news-end`), one alert
//! per line — a TradingView limitation that no longer applies now that the
//! engine is server-side Rust. Reading the lines back off the chart snapped
//! each anchor to its bar's timestamp (losing the true event minute, e.g. a
//! 14:30 event on an H1 chart) and — because start and end lines were pruned
//! independently against `[--start, trade-expiry]` — could split a window that
//! straddled the cursor into an orphaned half (the "1 start / 2 ends" abort).
//!
//! `NewsWindow` replaces both `(Drawing, Drawing)` pairs. It carries the real
//! `DateTime<Utc>` boundaries straight from the calendar planner
//! (`plan_calendar_bars_within`), so there is no draw + readback round-trip and
//! no bar-snapping. Windows are always internally consistent (`start <= end`),
//! so no pairing step and no split-pair failure mode.

use chrono::{DateTime, Utc};

/// A single resolved pause/blackout or news window.
///
/// `start` and `end` are wall-clock UTC instants (event minute ± the configured
/// buffer), not bar-snapped. Invariant: `start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewsWindow {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl NewsWindow {
    /// Construct a window. `start` and `end` are stored in ascending order, so
    /// a caller that passes them reversed still gets a well-formed window
    /// rather than a silently-negative interval.
    pub fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        if start <= end {
            Self { start, end }
        } else {
            Self {
                start: end,
                end: start,
            }
        }
    }

    /// Window open (inclusive).
    pub fn start(&self) -> DateTime<Utc> {
        self.start
    }

    /// Window close.
    pub fn end(&self) -> DateTime<Utc> {
        self.end
    }

    /// True when the whole window has closed at or before `as_of` — nothing
    /// left to pause / close-on-news for, so it can be dropped.
    pub fn is_past(&self, as_of: DateTime<Utc>) -> bool {
        self.end <= as_of
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn stores_boundaries_verbatim_when_ordered() {
        let w = NewsWindow::new(utc("2026-07-06T14:00:00Z"), utc("2026-07-06T15:00:00Z"));
        assert_eq!(w.start(), utc("2026-07-06T14:00:00Z"));
        assert_eq!(w.end(), utc("2026-07-06T15:00:00Z"));
    }

    #[test]
    fn reversed_inputs_are_normalised() {
        // A caller that hands start/end backwards still gets start <= end.
        let w = NewsWindow::new(utc("2026-07-06T15:00:00Z"), utc("2026-07-06T14:00:00Z"));
        assert_eq!(w.start(), utc("2026-07-06T14:00:00Z"));
        assert_eq!(w.end(), utc("2026-07-06T15:00:00Z"));
    }

    #[test]
    fn preserves_sub_bar_minute_precision() {
        // The whole point: a 14:30 event window is stored at the real minute,
        // not snapped to an H1 bar boundary.
        let w = NewsWindow::new(utc("2026-07-06T13:30:00Z"), utc("2026-07-06T15:30:00Z"));
        assert_eq!(w.start().to_rfc3339(), "2026-07-06T13:30:00+00:00");
        assert_eq!(w.end().to_rfc3339(), "2026-07-06T15:30:00+00:00");
    }

    #[test]
    fn is_past_is_end_inclusive() {
        let w = NewsWindow::new(utc("2026-07-06T14:00:00Z"), utc("2026-07-06T15:00:00Z"));
        // exactly at end → past (end-inclusive, matches old `<= as_of`).
        assert!(w.is_past(utc("2026-07-06T15:00:00Z")));
        assert!(w.is_past(utc("2026-07-06T16:00:00Z")));
        // inside the window → not past.
        assert!(!w.is_past(utc("2026-07-06T14:30:00Z")));
        // before it opens → not past.
        assert!(!w.is_past(utc("2026-07-06T13:00:00Z")));
    }
}
