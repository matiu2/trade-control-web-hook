//! Cosmetic chart markers for the news events tv-arm actually reacts to.
//!
//! tv-arm folds High-impact calendar events over `[cursor, trade-expiry]` into
//! the signed `TradePlan` as pause/news control windows (see
//! [`crate::news_window`]). Those windows are gate machinery, not chart
//! annotations. To make debugging or replaying a trade easier, tv-arm *also*
//! draws (by default) one cosmetic vertical line per event alongside arming the
//! windows, so the operator can *see* exactly the events tv-arm armed against —
//! labelled with its currency, star rating and Brisbane-local time.
//! `--skip-calendar-bars` opts out of the whole calendar step (windows **and**
//! markers).
//!
//! This is deliberately narrower than tv-news's annotation:
//! - **tv-news** draws Medium+ events for the asset's currencies (3★ for USD
//!   baseline) over the *visible* window — a looser, broader set.
//! - **tv-arm** draws exactly the High-only, `[cursor, expiry]`-scoped set it
//!   bakes into the plan, *after* elapsed windows are pruned. So the drawn lines
//!   are the armed set, one-for-one — the whole point for replay debugging.
//!
//! The label format is tv-arm's own (`<CCY>-<n>-star-<HH:MM>` in Brisbane time),
//! distinct from tv-news's name-slug labels (`usd-3-star-fomc`). Events sharing a
//! chart bar collapse to one marker whose label space-joins each event
//! (`"USD-3-star-22:00 EUR-2-star-22:30"`), because TradingView renders only one
//! drawing per bar cell.

use chrono::{DateTime, FixedOffset, Utc};
use trade_control_cli::Impact;

/// Brisbane is UTC+10 year-round (no DST). All operator charts, journals and
/// calculations are in Brisbane time, so the marker label matches.
const BRISBANE_OFFSET_SECS: i32 = 10 * 3600;

/// One news event tv-arm reacts to, slimmed to just what a chart marker needs:
/// currency, star rating and the real event minute. Built from the kept
/// [`cli::CalendarBarRow`]s so it carries the *same* filter/scope as the
/// pause/news windows — draw these and you've drawn the armed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsMarker {
    /// Upper-cased currency code (`"USD"`, `"EUR"`).
    pub currency: String,
    /// Impact as a star count, 1–3 (`Impact::stars()`).
    pub stars: u8,
    /// The real event minute (UTC), un-snapped to any bar.
    pub event_time: DateTime<Utc>,
}

impl NewsMarker {
    /// Build from a currency, impact and event time. Currency is upper-cased so
    /// the label is stable regardless of the calendar's casing.
    pub fn new(currency: &str, impact: Impact, event_time: DateTime<Utc>) -> Self {
        Self {
            currency: currency.to_uppercase(),
            stars: impact.stars(),
            event_time,
        }
    }

    /// This event's Brisbane-local time formatted `HH:MM`.
    fn brisbane_hhmm(&self) -> String {
        let offset = FixedOffset::east_opt(BRISBANE_OFFSET_SECS)
            .expect("BRISBANE_OFFSET_SECS is a valid fixed offset");
        self.event_time
            .with_timezone(&offset)
            .format("%H:%M")
            .to_string()
    }

    /// The per-event label chunk, e.g. `USD-3-star-22:00`. Multiple of these are
    /// space-joined when several events share a bar.
    fn label_chunk(&self) -> String {
        format!(
            "{}-{}-star-{}",
            self.currency,
            self.stars,
            self.brisbane_hhmm()
        )
    }
}

/// Fallback bar width when a chart resolution can't be parsed — 1 hour. Only
/// used for grouping same-bar markers, so a wrong guess merely mis-groups the
/// (rare) two-events-in-a-bar case, never affects the plan.
pub const DEFAULT_BAR_SECS: i64 = 3600;

/// Parse a TradingView resolution string (`"15"`, `"60"`, `"240"`, `"D"`,
/// `"W"`, `"M"`, optional `S` suffix) to seconds per bar. `None` for an
/// unparseable value; the caller falls back to [`DEFAULT_BAR_SECS`].
///
/// A local copy of tv-news's `resolution_to_secs` (that lives in the tv-news
/// binary and isn't a shared crate). tv-arm only ever sees ≥15m charts, but the
/// full table is cheap and keeps the two tools' grouping identical.
pub fn resolution_to_secs(res: &str) -> Option<i64> {
    let s = res.trim();
    if s.is_empty() {
        return None;
    }
    // Suffix form: seconds (`15S`), weeks (`W`), months (`M`), days (`D`).
    let upper = s.to_uppercase();
    if let Some(num) = upper.strip_suffix('S') {
        let n: i64 = num.trim().parse().ok().filter(|n| *n > 0).unwrap_or(1);
        return Some(n);
    }
    if let Some(num) = upper.strip_suffix('D') {
        let n: i64 = if num.is_empty() { 1 } else { num.parse().ok()? };
        return (n > 0).then_some(n * 86_400);
    }
    if let Some(num) = upper.strip_suffix('W') {
        let n: i64 = if num.is_empty() { 1 } else { num.parse().ok()? };
        return (n > 0).then_some(n * 7 * 86_400);
    }
    if let Some(num) = upper.strip_suffix('M')
        && !num.is_empty()
    {
        // `M` with a leading number is months (e.g. `2M`). Bare `M` handled below.
        let n: i64 = num.parse().ok()?;
        return (n > 0).then_some(n * 30 * 86_400);
    }
    if upper == "M" {
        return Some(30 * 86_400);
    }
    // Plain minute count.
    let mins: i64 = s.parse().ok()?;
    (mins > 0).then_some(mins * 60)
}

/// One vertical line to draw: an anchor epoch (seconds) and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerLine {
    /// Line anchor time (unix seconds) — the first event's real minute in the bar.
    pub anchor_epoch: i64,
    /// The full label (one chunk, or space-joined chunks for a shared bar).
    pub label: String,
}

/// Collapse the armed markers into one line per chart bar.
///
/// Two events inside the same `bar_secs`-wide bar would render on top of each
/// other (TradingView draws one entity per bar cell), so they are merged into a
/// single line anchored at the *first* event's minute, with every event's chunk
/// space-joined into the label (`"USD-3-star-22:00 EUR-2-star-22:30"`).
///
/// Pure: takes the markers (assumed ascending by `event_time`, as the calendar
/// planner returns them) and a positive bar width, returns the lines to draw. A
/// non-positive `bar_secs` degrades to one line per marker rather than dividing
/// by zero.
pub fn news_marker_lines(markers: &[NewsMarker], bar_secs: i64) -> Vec<MarkerLine> {
    let mut lines: Vec<MarkerLine> = Vec::new();
    let mut last_key: Option<i64> = None;
    for m in markers {
        let epoch = m.event_time.timestamp();
        let key = if bar_secs > 0 {
            epoch.div_euclid(bar_secs)
        } else {
            // Degenerate width: give every marker its own bucket.
            epoch
        };
        match (last_key, lines.last_mut()) {
            (Some(k), Some(line)) if k == key => {
                line.label.push(' ');
                line.label.push_str(&m.label_chunk());
            }
            _ => {
                lines.push(MarkerLine {
                    anchor_epoch: epoch,
                    label: m.label_chunk(),
                });
                last_key = Some(key);
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    fn marker(ccy: &str, impact: Impact, ts: &str) -> NewsMarker {
        NewsMarker::new(ccy, impact, utc(ts))
    }

    #[test]
    fn label_is_currency_stars_and_brisbane_time() {
        // FOMC at 18:00 UTC = 04:00 Brisbane next day.
        let m = marker("usd", Impact::High, "2026-07-08T18:00:00Z");
        assert_eq!(m.label_chunk(), "USD-3-star-04:00");
    }

    #[test]
    fn currency_is_uppercased_and_stars_map_from_impact() {
        assert_eq!(
            marker("aud", Impact::Medium, "2026-07-06T01:30:00Z").label_chunk(),
            "AUD-2-star-11:30"
        );
        assert_eq!(
            marker("eur", Impact::Low, "2026-07-06T00:00:00Z").label_chunk(),
            "EUR-1-star-10:00"
        );
    }

    #[test]
    fn one_line_per_event_when_bars_dont_collide() {
        let markers = vec![
            marker("usd", Impact::High, "2026-07-06T12:00:00Z"),
            marker("eur", Impact::High, "2026-07-06T14:00:00Z"),
        ];
        let lines = news_marker_lines(&markers, 3600);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].label, "USD-3-star-22:00");
        assert_eq!(
            lines[0].anchor_epoch,
            utc("2026-07-06T12:00:00Z").timestamp()
        );
        assert_eq!(lines[1].label, "EUR-3-star-00:00");
    }

    #[test]
    fn events_sharing_a_bar_merge_into_one_line_space_joined() {
        // Two events 30 min apart on an H1 chart land in the same bar.
        let markers = vec![
            marker("usd", Impact::High, "2026-07-06T12:00:00Z"),
            marker("eur", Impact::Medium, "2026-07-06T12:30:00Z"),
        ];
        let lines = news_marker_lines(&markers, 3600);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].label, "USD-3-star-22:00 EUR-2-star-22:30");
        // Anchored at the first event's real minute.
        assert_eq!(
            lines[0].anchor_epoch,
            utc("2026-07-06T12:00:00Z").timestamp()
        );
    }

    #[test]
    fn three_events_across_two_bars_group_correctly() {
        let markers = vec![
            marker("usd", Impact::High, "2026-07-06T12:00:00Z"),
            marker("usd", Impact::High, "2026-07-06T12:15:00Z"),
            marker("gbp", Impact::High, "2026-07-06T13:05:00Z"),
        ];
        let lines = news_marker_lines(&markers, 3600);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].label, "USD-3-star-22:00 USD-3-star-22:15");
        assert_eq!(lines[1].label, "GBP-3-star-23:05");
    }

    #[test]
    fn empty_markers_yield_no_lines() {
        assert!(news_marker_lines(&[], 3600).is_empty());
    }

    #[test]
    fn non_positive_bar_secs_gives_one_line_per_marker() {
        let markers = vec![
            marker("usd", Impact::High, "2026-07-06T12:00:00Z"),
            marker("eur", Impact::Medium, "2026-07-06T12:30:00Z"),
        ];
        // bar_secs 0 must not panic (div-by-zero) and must not merge.
        let lines = news_marker_lines(&markers, 0);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn resolution_to_secs_parses_minutes_and_periods() {
        assert_eq!(resolution_to_secs("15"), Some(900));
        assert_eq!(resolution_to_secs("60"), Some(3600));
        assert_eq!(resolution_to_secs("240"), Some(14_400));
        assert_eq!(resolution_to_secs("D"), Some(86_400));
        assert_eq!(resolution_to_secs("W"), Some(7 * 86_400));
        assert_eq!(resolution_to_secs("M"), Some(30 * 86_400));
        assert_eq!(resolution_to_secs("2M"), Some(60 * 86_400));
        assert_eq!(resolution_to_secs(" 60 "), Some(3600));
    }

    #[test]
    fn resolution_to_secs_rejects_garbage() {
        assert_eq!(resolution_to_secs(""), None);
        assert_eq!(resolution_to_secs("abc"), None);
        assert_eq!(resolution_to_secs("0"), None);
    }

    #[test]
    fn brisbane_offset_wraps_past_midnight() {
        // 15:00 UTC = 01:00 Brisbane next day.
        let m = Utc.with_ymd_and_hms(2026, 7, 6, 15, 0, 0).unwrap();
        assert_eq!(
            NewsMarker::new("usd", Impact::High, m).label_chunk(),
            "USD-3-star-01:00"
        );
    }
}
