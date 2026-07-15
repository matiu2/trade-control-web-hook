//! [`NewsWindow`] — a resolved `[start, end)` time interval baked onto the plan.
//!
//! The plan-data version of `tv-arm`'s `NewsWindow`. A qualifying economic-news
//! event produces two of these (see `SCOPING-engine-v2-news.md` and the live
//! `trade-calendar-maker` crate):
//!
//! - a **pause window** `[event − before, event]` (8h for H1+, 3h for M15) — while
//!   `now` is inside it, entries are blocked (the standoff); and
//! - a **news window** `[event, event + 1h]` — does *not* block entries; it enables
//!   a counter-trend reversal candle to close an open trade (a later slice).
//!
//! This slice uses only the **pause** windows ([`TradePlan::pause_windows`]).
//!
//! The window is the unit: `start`/`end` are real wall-clock UTC instants at event
//! minute precision (not bar-snapped), and it is always internally consistent
//! (`start <= end`). That is deliberate — the old chart-drawn `(start-line,
//! end-line)` pairs could be pruned independently and split a window that
//! straddled the cursor into an orphaned half (the "1 start / 2 ends" abort). A
//! single `NewsWindow` can't split.

use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};

/// A resolved pause / news window — a `[start, end)` UTC interval.
///
/// Constructed via [`new`](Self::new), which orders its bounds so a caller that
/// passes them reversed still gets a well-formed window. Membership is
/// **start-inclusive, end-exclusive** ([`contains`](Self::contains)): a bar
/// exactly at `end` is *out* (the window has closed) — this is what makes the
/// pause auto-resume at the event time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsWindow {
    /// Window open (inclusive). UTC wall-clock, event-minute precision.
    pub start: DateTime<Utc>,
    /// Window close (exclusive). UTC wall-clock, event-minute precision.
    pub end: DateTime<Utc>,
}

impl NewsWindow {
    /// Construct a window, storing `start`/`end` in ascending order so a reversed
    /// pair still yields `start <= end` rather than a silently-negative interval.
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

    /// Is `t` inside the window? **Start-inclusive, end-exclusive**
    /// (`start <= t < end`). A bar exactly at `end` is *not* inside — the window
    /// has closed, so a pause auto-resumes there.
    pub fn contains(&self, t: DateTime<Utc>) -> bool {
        self.start <= t && t < self.end
    }

    /// Has the whole window closed at or before `as_of`? (`end <= as_of`.) Nothing
    /// left to pause for, so it can be dropped. End-inclusive, matching `tv-arm`'s
    /// prune test.
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
    fn new_normalises_reversed_bounds() {
        let w = NewsWindow::new(utc("2026-07-06T15:00:00Z"), utc("2026-07-06T14:00:00Z"));
        assert_eq!(w.start, utc("2026-07-06T14:00:00Z"));
        assert_eq!(w.end, utc("2026-07-06T15:00:00Z"));
    }

    #[test]
    fn contains_is_start_inclusive_end_exclusive() {
        let w = NewsWindow::new(utc("2026-07-06T14:00:00Z"), utc("2026-07-06T15:00:00Z"));
        assert!(
            !w.contains(utc("2026-07-06T13:59:59Z")),
            "before start: out"
        );
        assert!(w.contains(utc("2026-07-06T14:00:00Z")), "at start: in");
        assert!(w.contains(utc("2026-07-06T14:30:00Z")), "mid: in");
        // The auto-resume boundary: exactly at `end` is OUT.
        assert!(
            !w.contains(utc("2026-07-06T15:00:00Z")),
            "at end: out (window closed → pause resumes here)",
        );
        assert!(!w.contains(utc("2026-07-06T15:00:01Z")), "after end: out");
    }

    #[test]
    fn is_past_is_end_inclusive() {
        let w = NewsWindow::new(utc("2026-07-06T14:00:00Z"), utc("2026-07-06T15:00:00Z"));
        assert!(w.is_past(utc("2026-07-06T15:00:00Z")), "at end: past");
        assert!(w.is_past(utc("2026-07-06T16:00:00Z")), "after: past");
        assert!(!w.is_past(utc("2026-07-06T14:30:00Z")), "inside: not past");
    }

    #[test]
    fn preserves_sub_bar_minute_precision() {
        let w = NewsWindow::new(utc("2026-07-06T13:30:00Z"), utc("2026-07-06T15:30:00Z"));
        assert_eq!(w.start.to_rfc3339(), "2026-07-06T13:30:00+00:00");
        assert_eq!(w.end.to_rfc3339(), "2026-07-06T15:30:00+00:00");
    }

    /// `TradePlan.pause_windows` round-trips through serde, and its
    /// `#[serde(default)]` lets a plan JSON predating the field deserialize with an
    /// empty vec.
    #[test]
    fn pause_windows_wire_roundtrip_and_default() {
        use crate::TradePlan;
        use trade_control_core::broker::Granularity;
        use trade_control_core::intent::Direction;

        let plan = TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            granularity: Granularity::H1,
            lines: Vec::new(),
            levels: Vec::new(),
            markers: Vec::new(),
            pause_windows: vec![NewsWindow::new(
                utc("2026-07-06T06:00:00Z"),
                utc("2026-07-06T14:00:00Z"),
            )],
            rules: Vec::new(),
            cross_buffer_pct: 0.0,
            retest_atr_step: 0.0,
        };
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: TradePlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.pause_windows.len(),
            1,
            "window survives the round-trip"
        );
        assert_eq!(back.pause_windows[0].start, utc("2026-07-06T06:00:00Z"));

        // A plan JSON with no `pause_windows` key → empty vec via serde(default).
        let legacy = json.replace(
            "\"pause_windows\":[{\"start\":\"2026-07-06T06:00:00Z\",\"end\":\"2026-07-06T14:00:00Z\"}],",
            "",
        );
        assert!(
            !legacy.contains("pause_windows"),
            "key stripped for the test"
        );
        let restored: TradePlan = serde_json::from_str(&legacy).expect("deserialize legacy");
        assert!(restored.pause_windows.is_empty(), "missing key ⇒ empty vec");
    }
}
