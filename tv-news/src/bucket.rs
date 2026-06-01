//! Group news events that land in the same chart bar.
//!
//! TradingView only renders one drawing per anchor cell at a given
//! resolution, so two events 5 minutes apart on an H1 chart end up
//! either stacked invisibly or both clipped to the same bar. Instead
//! of fighting that, we bucket events by `floor(anchor / bar_secs)`
//! and emit one vertical line per bucket whose label lists every
//! event in it.
//!
//! Pure module — takes events + a bar width and returns groups; the
//! pipeline calls it after filtering and dedupe and then draws the
//! buckets.
//!
//! Label format inside a bucket: each event's full label
//! (`<ccy>-<n>-star-<slug>`), joined with `, ` between events,
//! breaking onto a new line every `EVENTS_PER_LINE` (3) events. This
//! keeps the TV drawing-properties text box readable even when a
//! triple-news clash lands on the same bar (e.g. NFP + average
//! earnings + unemployment, all USD 3★ at the same minute).

use chrono::{DateTime, Utc};
use trade_control_cli::EconomicEvent;

use crate::filter::news_anchor;
use crate::label::news_label;

/// Group events that share a bar bucket. Buckets are keyed by
/// `floor(anchor_secs / bar_secs)`, so two events inside the same TV
/// bar share a key regardless of their exact second-offset within the
/// bar.
///
/// Returns groups in chronological order of the first event's anchor.
/// Within each group, events keep their input order (the caller is
/// expected to feed events already sorted by time — `fetch_events_for_range`
/// returns them that way).
///
/// `bar_secs` must be positive. A zero or negative value would make
/// the bucket index ill-defined; the caller is responsible for
/// supplying a sane fallback (see `resolution::DEFAULT_BAR_SECS`).
pub fn bucket_events(events: Vec<EconomicEvent>, bar_secs: i64) -> Vec<EventBucket> {
    if bar_secs <= 0 || events.is_empty() {
        return events.into_iter().map(EventBucket::single).collect();
    }
    let mut buckets: Vec<EventBucket> = Vec::new();
    for ev in events {
        let anchor = news_anchor(&ev);
        let key = anchor.timestamp().div_euclid(bar_secs);
        match buckets.last_mut() {
            Some(b) if b.key == key => b.events.push(ev),
            _ => buckets.push(EventBucket::new(key, ev)),
        }
    }
    buckets
}

/// One bucket of events that share a chart bar.
#[derive(Debug, Clone)]
pub struct EventBucket {
    /// `floor(anchor / bar_secs)` — buckets with the same key would
    /// collide on the chart and must be merged into one drawing.
    pub key: i64,
    /// The events in this bucket, in original input order.
    pub events: Vec<EconomicEvent>,
}

impl EventBucket {
    fn new(key: i64, ev: EconomicEvent) -> Self {
        Self {
            key,
            events: vec![ev],
        }
    }

    fn single(ev: EconomicEvent) -> Self {
        Self {
            key: 0,
            events: vec![ev],
        }
    }

    /// Anchor timestamp for the bucket's vertical line — the first
    /// event's anchor. Keeps the line on a real event time rather
    /// than the bar boundary, which reads better when the operator
    /// hovers the line for a tooltip.
    pub fn anchor(&self) -> DateTime<Utc> {
        // Safe: `bucket_events` never creates an empty bucket.
        news_anchor(&self.events[0])
    }

    /// Build the drawing label for this bucket. Single-event buckets
    /// keep their plain `<ccy>-<n>-star-<slug>` form. Multi-event
    /// buckets concatenate every event's label, joined with `, ` and
    /// a newline every [`EVENTS_PER_LINE`].
    pub fn label(&self) -> String {
        bucket_label(&self.events)
    }
}

/// Events per visual line in a multi-event bucket label.
const EVENTS_PER_LINE: usize = 3;

fn bucket_label(events: &[EconomicEvent]) -> String {
    if events.len() == 1 {
        return news_label(&events[0]);
    }
    let labels: Vec<String> = events.iter().map(news_label).collect();
    let mut out = String::new();
    for (idx, label) in labels.iter().enumerate() {
        if idx > 0 {
            // Separator between events; newline every EVENTS_PER_LINE.
            if idx % EVENTS_PER_LINE == 0 {
                out.push_str(",\n");
            } else {
                out.push_str(", ");
            }
        }
        out.push_str(label);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use trade_control_cli::Impact;

    fn ev(name: &str, currency: &str, impact: Impact, time_utc: &str) -> EconomicEvent {
        let dt: DateTime<Utc> = time_utc.parse().unwrap();
        EconomicEvent {
            name: name.to_string(),
            currency: currency.to_string(),
            impact,
            datetime: Local.from_utc_datetime(&dt.naive_utc()),
            actual: None,
            forecast: None,
            previous: None,
        }
    }

    #[test]
    fn singleton_bucket_uses_plain_label() {
        let events = vec![ev("FOMC", "USD", Impact::High, "2026-06-10T18:00:00Z")];
        let buckets = bucket_events(events, 3600);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].events.len(), 1);
        assert_eq!(buckets[0].label(), "usd-3-star-fomc");
    }

    #[test]
    fn two_events_in_same_h1_bar_merge() {
        let events = vec![
            ev("NFP", "USD", Impact::High, "2026-06-10T12:30:00Z"),
            ev("Avg Earnings", "USD", Impact::High, "2026-06-10T12:30:00Z"),
        ];
        let buckets = bucket_events(events, 3600);
        assert_eq!(buckets.len(), 1, "same H1 bar should be one bucket");
        assert_eq!(
            buckets[0].label(),
            "usd-3-star-nfp, usd-3-star-avg-earnings"
        );
    }

    #[test]
    fn events_in_different_h1_bars_stay_separate() {
        let events = vec![
            ev("CPI", "USD", Impact::High, "2026-06-10T12:30:00Z"),
            ev("PPI", "USD", Impact::Medium, "2026-06-10T13:30:00Z"),
        ];
        let buckets = bucket_events(events, 3600);
        assert_eq!(buckets.len(), 2);
    }

    #[test]
    fn four_events_in_same_bar_break_after_three() {
        let events = vec![
            ev("A", "USD", Impact::High, "2026-06-10T12:00:00Z"),
            ev("B", "USD", Impact::High, "2026-06-10T12:10:00Z"),
            ev("C", "USD", Impact::High, "2026-06-10T12:20:00Z"),
            ev("D", "USD", Impact::High, "2026-06-10T12:50:00Z"),
        ];
        let buckets = bucket_events(events, 3600);
        assert_eq!(buckets.len(), 1);
        // Three on the first line, then a newline, then the fourth.
        let expected = "usd-3-star-a, usd-3-star-b, usd-3-star-c,\nusd-3-star-d";
        assert_eq!(buckets[0].label(), expected);
    }

    #[test]
    fn anchor_is_first_event_in_bucket() {
        let first = "2026-06-10T12:15:00Z";
        let second = "2026-06-10T12:45:00Z";
        let events = vec![
            ev("A", "USD", Impact::High, first),
            ev("B", "USD", Impact::High, second),
        ];
        let buckets = bucket_events(events, 3600);
        assert_eq!(buckets[0].anchor(), first.parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn daily_bar_merges_all_same_day_events() {
        let events = vec![
            ev("Morning", "EUR", Impact::Medium, "2026-06-10T08:00:00Z"),
            ev("Lunch", "EUR", Impact::Medium, "2026-06-10T12:00:00Z"),
            ev("Close", "EUR", Impact::Medium, "2026-06-10T16:00:00Z"),
        ];
        let buckets = bucket_events(events, 86_400);
        assert_eq!(buckets.len(), 1, "all three within one day-bar");
    }

    #[test]
    fn bar_secs_zero_falls_back_to_one_per_bucket() {
        let events = vec![
            ev("A", "USD", Impact::High, "2026-06-10T12:00:00Z"),
            ev("B", "USD", Impact::High, "2026-06-10T12:00:00Z"),
        ];
        let buckets = bucket_events(events, 0);
        assert_eq!(buckets.len(), 2);
    }
}
