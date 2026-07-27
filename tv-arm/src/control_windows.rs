//! The calendar-derived **control windows** of an arm: blackout (pause/resume)
//! windows, news (news-start/news-end) windows, and the cosmetic markers that
//! annotate them — plus the prune that keeps all three consistent.
//!
//! ## Why these three live together
//!
//! They arrive together (one `calendar_windows` call), are pruned together
//! against one as-of instant, and are consumed together (one pause bundle per
//! blackout window, one news bundle per news window, one drawn marker per news
//! event). Held as three loose `Vec`s on `Roles` they could drift: prune the
//! news windows and forget the markers and the chart claims tv-arm is watching
//! an event it isn't. The lock-step is an *invariant*, so it gets a type that
//! owns it — [`ControlWindows::drop_past`] is the only way to shrink the set,
//! and it prunes all three or none.
//!
//! ## Why it's separate from `Roles`
//!
//! `Roles` is "what is drawn on the chart". These windows are **not drawn** —
//! since PR1b they come straight from the economic calendar at real
//! event-minute precision, and the old draw-then-read-back round-trip (which
//! bar-snapped every boundary) is gone. Keeping them on `Roles` implied a chart
//! provenance they no longer have, and it made the arming path take
//! `&mut Roles` purely so it could prune two of ten fields.
//!
//! That mutation is the thing this module removes. `Roles` is now built once and
//! read; the windows are built once, pruned once at construction, and read. A
//! frozen-spec arm (which has no chart at all) can construct these directly.

use chrono::{DateTime, Utc};
use tracing::info;

use crate::news_marker::NewsMarker;
use crate::news_window::NewsWindow;

/// The "as-of" instant control windows are pruned against, plus where it came
/// from (for the drop log line).
///
/// In a live `--register-plan` arm this is wall-clock `now`; in an offline /
/// replay `--plan-out` build it's the chart's replay cursor (visible range right
/// edge), so blackouts still *upcoming* relative to the cursor survive a
/// historical replay. See `BUG-tv-arm-stale-blackout-*`.
#[derive(Clone, Copy, Debug)]
pub struct AsOf {
    /// The instant "now" is taken to be for pruning purposes.
    pub at: DateTime<Utc>,
    /// Which rule picked `at` — `wallclock` / `start-flag` / `as-of-flag` /
    /// `replay-cursor`. Logged so a surprising prune is traceable to its cause.
    pub source: &'static str,
}

impl AsOf {
    /// Wall-clock as-of — a live arm.
    pub fn wallclock(at: DateTime<Utc>) -> Self {
        Self {
            at,
            source: "wallclock",
        }
    }
}

/// Blackout + news windows and their cosmetic markers, pruned to the live set.
///
/// Construct with [`Self::new`] (which prunes) or [`Self::empty`]
/// (`--skip-calendar-bars`, or a calendar failure). The fields are read-only
/// from outside: the lock-step between `news` and `markers` is an invariant, and
/// a caller that could `push` to one without the other would break it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlWindows {
    blackout: Vec<NewsWindow>,
    news: Vec<NewsWindow>,
    markers: Vec<NewsMarker>,
}

impl ControlWindows {
    /// Build from the raw calendar output, dropping everything already elapsed
    /// as of `as_of`.
    ///
    /// Pruning at construction is deliberate: there is no window in which an
    /// un-pruned set is observable, so no caller can forget to prune. A past
    /// window has nothing left to pause / close-on-news for, and feeding one to
    /// `build_pause_from_spec` would hard-fail with "refusing to arm a stale
    /// blackout".
    pub fn new(
        blackout: Vec<NewsWindow>,
        news: Vec<NewsWindow>,
        markers: Vec<NewsMarker>,
        as_of: AsOf,
    ) -> Self {
        let mut windows = Self {
            blackout,
            news,
            markers,
        };
        windows.drop_past(as_of);
        windows
    }

    /// No control windows at all: `--skip-calendar-bars`, or a calendar
    /// resolution that failed (which warns and continues rather than aborting
    /// the arm).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Blackout (pause/resume) windows — one pause bundle each.
    pub fn blackout(&self) -> &[NewsWindow] {
        &self.blackout
    }

    /// News (news-start/news-end) windows — one news bundle each.
    pub fn news(&self) -> &[NewsWindow] {
        &self.news
    }

    /// The events to draw cosmetic markers for. Always in lock-step with
    /// [`Self::news`].
    pub fn markers(&self) -> &[NewsMarker] {
        &self.markers
    }

    /// True when the trade has at least one news window, i.e. the enter should
    /// carry `close_on_news`.
    pub fn has_news(&self) -> bool {
        !self.news.is_empty()
    }

    /// Drop windows whose interval has already fully closed (`end <= as_of.at`),
    /// then re-align the markers.
    ///
    /// The visible-window filter in `classify` only removes lines that are
    /// *off-screen*; when the operator arms off a chart showing historical bars
    /// (an old H&S whose trade-expiry is in the past), the news/blackout windows
    /// are genuinely in scope yet have elapsed in wall-clock terms. Dropping
    /// them here, once, is what makes the log line, `close_on_news`, and both
    /// bundle builders agree on one live-only view.
    fn drop_past(&mut self, as_of: AsOf) {
        for (kind, windows) in [("blackout", &mut self.blackout), ("news", &mut self.news)] {
            let before = windows.len();
            windows.retain(|w| !w.is_past(as_of.at));
            let dropped = before - windows.len();
            if dropped > 0 {
                info!(
                    kind,
                    dropped,
                    as_of = %as_of.at.to_rfc3339(),
                    source = as_of.source,
                    "dropping control window(s) whose interval already closed (end <= as_of)"
                );
            }
        }
        self.retain_markers_matching_news(as_of);
    }

    /// Keep the cosmetic markers exactly in lock-step with the surviving news
    /// windows, so a drawn marker always corresponds to an armed window.
    ///
    /// A news window is `[event_time, event_time + after]`, so a marker survives
    /// iff a surviving news window *opens* at its event minute. That correctly
    /// keeps a marker whose event minute has already passed but whose
    /// post-release window is still open — the rule is drawn == armed, not
    /// drawn == future.
    fn retain_markers_matching_news(&mut self, as_of: AsOf) {
        let live_event_secs: std::collections::HashSet<i64> =
            self.news.iter().map(|w| w.start().timestamp()).collect();
        let before = self.markers.len();
        self.markers
            .retain(|m| live_event_secs.contains(&m.event_time.timestamp()));
        let dropped = before - self.markers.len();
        if dropped > 0 {
            info!(
                dropped,
                as_of = %as_of.at.to_rfc3339(),
                source = as_of.source,
                "dropping news marker(s) whose news window already closed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_cli::Impact;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        utc("2026-06-08T12:00:00Z")
    }

    /// A window from unix-second offsets relative to `now`.
    fn win(from_secs: i64, to_secs: i64) -> NewsWindow {
        let t = now().timestamp();
        NewsWindow::new(
            DateTime::from_timestamp(t + from_secs, 0).expect("in range"),
            DateTime::from_timestamp(t + to_secs, 0).expect("in range"),
        )
    }

    fn marker(currency: &str, offset_secs: i64) -> NewsMarker {
        let t = now().timestamp();
        NewsMarker::new(
            currency,
            Impact::High,
            DateTime::from_timestamp(t + offset_secs, 0).expect("in range"),
        )
    }

    /// One live and one elapsed window of each kind: only the live ones survive.
    /// An elapsed window has nothing left to act on, and `build_pause_from_spec`
    /// would reject it as a stale arm.
    #[test]
    fn construction_drops_elapsed_windows() {
        let live = win(1800, 3600);
        let past = win(-7200, -3600);

        let windows = ControlWindows::new(
            vec![past, live],
            vec![past, live],
            vec![],
            AsOf::wallclock(now()),
        );

        assert_eq!(windows.blackout(), [live]);
        assert_eq!(windows.news(), [live]);
    }

    /// The boundary: the gate is `end <= as_of`, so a window ending exactly at
    /// the as-of instant is elapsed. Mirrors `build_pause_from_spec`'s own check
    /// — if this drifted, a window we kept would be rejected downstream.
    #[test]
    fn a_window_ending_exactly_at_as_of_is_elapsed() {
        let live = win(0, 1); // ends 1s out
        let windows = ControlWindows::new(
            vec![],
            vec![win(-60, 0), live],
            vec![],
            AsOf::wallclock(now()),
        );
        assert_eq!(windows.news(), [live]);
    }

    /// Markers track the surviving *windows*, not wall-clock: an event minute
    /// that has already passed but whose post-release window is still open keeps
    /// its marker, because the chart must show what is actually armed.
    #[test]
    fn a_marker_survives_while_its_window_is_open_even_past_its_event_minute() {
        // Event 60s ago, window [event, event+1800] → still open.
        let windows = ControlWindows::new(
            vec![],
            vec![win(-7200, -5400), win(-60, 1740)],
            vec![marker("EUR", -7200), marker("USD", -60)],
            AsOf::wallclock(now()),
        );

        assert_eq!(windows.news().len(), 1, "only the open window survives");
        assert_eq!(windows.markers().len(), 1);
        assert_eq!(windows.markers()[0].currency, "USD");
    }

    /// The invariant the type exists to hold: every surviving marker has a
    /// surviving news window opening at its event minute, and no window is left
    /// unannotated. Asserted as a set property rather than by index, so it
    /// survives any reordering inside the prune.
    #[test]
    fn markers_and_news_windows_stay_in_lockstep() {
        let windows = ControlWindows::new(
            vec![],
            vec![win(-7200, -5400), win(-60, 1740), win(3600, 5400)],
            vec![
                marker("EUR", -7200), // window elapsed → dropped
                marker("USD", -60),   // window open → kept
                marker("GBP", 3600),  // window future → kept
                marker("JPY", 7_200), // no window opens then at all → dropped
            ],
            AsOf::wallclock(now()),
        );

        let window_opens: std::collections::HashSet<i64> = windows
            .news()
            .iter()
            .map(|w| w.start().timestamp())
            .collect();
        let marker_events: std::collections::HashSet<i64> = windows
            .markers()
            .iter()
            .map(|m| m.event_time.timestamp())
            .collect();

        assert!(
            marker_events.is_subset(&window_opens),
            "every marker must annotate an armed window: {marker_events:?} vs {window_opens:?}"
        );
        assert_eq!(
            windows.markers().len(),
            2,
            "the elapsed and the unmatched marker are both dropped"
        );
    }

    /// A marker with no matching window is dropped even when nothing elapsed —
    /// the rule is "annotates an armed window", not "is in the future".
    #[test]
    fn an_unmatched_marker_is_dropped_with_no_elapsed_windows() {
        let windows = ControlWindows::new(
            vec![],
            vec![win(3600, 5400)],
            vec![marker("GBP", 3600), marker("JPY", 4000)],
            AsOf::wallclock(now()),
        );
        assert_eq!(windows.news().len(), 1);
        assert_eq!(windows.markers().len(), 1);
        assert_eq!(windows.markers()[0].currency, "GBP");
    }

    /// `has_news` is what drives `close_on_news` on the enter, so it must read
    /// the *pruned* set — an all-elapsed calendar must not arm a news close.
    #[test]
    fn has_news_reads_the_pruned_set() {
        let all_past = ControlWindows::new(
            vec![],
            vec![win(-7200, -3600)],
            vec![],
            AsOf::wallclock(now()),
        );
        assert!(
            !all_past.has_news(),
            "every window elapsed → no close_on_news"
        );

        let live = ControlWindows::new(vec![], vec![win(60, 3600)], vec![], AsOf::wallclock(now()));
        assert!(live.has_news());
    }

    /// `empty` is the `--skip-calendar-bars` / calendar-failure state: nothing
    /// armed, nothing drawn, no news close.
    #[test]
    fn empty_arms_nothing() {
        let windows = ControlWindows::empty();
        assert!(windows.blackout().is_empty());
        assert!(windows.news().is_empty());
        assert!(windows.markers().is_empty());
        assert!(!windows.has_news());
    }

    /// Pruning is against `as_of`, not wall-clock: an offline replay whose
    /// cursor sits in the past keeps windows that are elapsed vs today but still
    /// upcoming vs the cursor. This is the stale-blackout replay bug.
    #[test]
    fn a_past_cursor_keeps_windows_upcoming_relative_to_it() {
        let cursor = AsOf {
            at: now() - chrono::Duration::days(14),
            source: "replay-cursor",
        };
        // Elapsed vs `now`, but 14 days ahead of the cursor.
        let upcoming_vs_cursor = win(-3600, -1800);

        let windows = ControlWindows::new(
            vec![upcoming_vs_cursor],
            vec![upcoming_vs_cursor],
            vec![],
            cursor,
        );

        assert_eq!(windows.blackout(), [upcoming_vs_cursor]);
        assert_eq!(windows.news(), [upcoming_vs_cursor]);
    }
}
