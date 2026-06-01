//! Filter forex-factory events down to the set tv-news should
//! annotate, and dedupe candidates against vertical lines already on
//! the chart.
//!
//! Two pure helpers, both unit-testable without I/O:
//!
//! - [`filter_events`]: keep events whose currency is in the asset's
//!   `news_currencies` at 2★+ impact, OR a baseline currency (USD) at
//!   3★. Today the baseline is USD; a future revision may widen this.
//! - [`events_needing_drawing`]: drop events whose anchor timestamp is
//!   already covered by a tv-news vertical line on the chart within
//!   ±tolerance, so re-runs are idempotent.

use chrono::{DateTime, Utc};
use trade_control_cli::{EconomicEvent, Impact};

/// Keep the events tv-news should put on the chart.
///
/// - `news_currencies`: the asset's currencies (already upper-cased by
///   the caller). Events at `Impact::Medium` or higher in any of these
///   are kept.
/// - `baseline_currencies`: currencies whose 3★ events are always
///   annotated regardless of the asset. Today this is just `["USD"]`
///   (FOMC, NFP, CPI). Caller pre-uppercases.
///
/// Comparison is case-insensitive for safety even though both inputs
/// are expected upper-case by convention.
pub fn filter_events(
    events: &[EconomicEvent],
    news_currencies: &[String],
    baseline_currencies: &[String],
) -> Vec<EconomicEvent> {
    events
        .iter()
        .filter(|ev| keep_event(ev, news_currencies, baseline_currencies))
        .cloned()
        .collect()
}

fn keep_event(ev: &EconomicEvent, news: &[String], baseline: &[String]) -> bool {
    let ccy = ev.currency.to_uppercase();
    let in_news = news.iter().any(|c| c.eq_ignore_ascii_case(&ccy));
    let in_baseline = baseline.iter().any(|c| c.eq_ignore_ascii_case(&ccy));

    if in_news && ev.impact >= Impact::Medium {
        return true;
    }
    if in_baseline && ev.impact == Impact::High {
        return true;
    }
    false
}

/// Drop events whose anchor timestamp already has a tv-news vertical
/// line on the chart within ±`tolerance`. Used so a second `tv-news`
/// run on the same chart doesn't pile duplicate lines on top of the
/// first run's output.
///
/// `existing_news_anchor_secs` is the list of unix-second timestamps
/// from drawings already on the chart whose label looks like a
/// tv-news event marker (see `label::is_news_label`). Caller is
/// responsible for filtering the chart's drawing list down to that
/// set — this helper only cares about timestamps.
pub fn events_needing_drawing(
    events: Vec<EconomicEvent>,
    existing_news_anchor_secs: &[i64],
    tolerance: chrono::Duration,
) -> Vec<EconomicEvent> {
    let tol = tolerance.num_seconds();
    events
        .into_iter()
        .filter(|ev| !is_duplicate(ev, existing_news_anchor_secs, tol))
        .collect()
}

fn is_duplicate(ev: &EconomicEvent, existing: &[i64], tolerance_secs: i64) -> bool {
    let ts = ev.datetime.with_timezone(&Utc).timestamp();
    existing.iter().any(|e| (e - ts).abs() <= tolerance_secs)
}

/// The chart-anchor timestamp for an event — the `datetime` field
/// re-normalised to UTC. tv-news draws one vertical line per event at
/// this instant.
pub fn news_anchor(ev: &EconomicEvent) -> DateTime<Utc> {
    ev.datetime.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn ev(name: &str, currency: &str, impact: Impact, time_utc: &str) -> EconomicEvent {
        EconomicEvent {
            name: name.to_string(),
            currency: currency.to_string(),
            impact,
            datetime: Local.from_utc_datetime(&ts(time_utc).naive_utc()),
            actual: None,
            forecast: None,
            previous: None,
        }
    }

    #[test]
    fn keeps_2star_for_news_currency() {
        let events = vec![ev("CPI", "EUR", Impact::Medium, "2026-06-10T12:00:00Z")];
        let kept = filter_events(&events, &["EUR".into(), "USD".into()], &["USD".into()]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn drops_1star_for_news_currency() {
        let events = vec![ev("Speech", "EUR", Impact::Low, "2026-06-10T12:00:00Z")];
        let kept = filter_events(&events, &["EUR".into(), "USD".into()], &["USD".into()]);
        assert_eq!(kept.len(), 0);
    }

    #[test]
    fn keeps_3star_usd_even_for_non_usd_asset() {
        // SMI: news_currencies = [CHF, EUR]. USD 3★ should still land
        // because of the baseline.
        let events = vec![ev("FOMC", "USD", Impact::High, "2026-06-10T18:00:00Z")];
        let kept = filter_events(
            &events,
            &["CHF".into(), "EUR".into(), "USD".into()],
            &["USD".into()],
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn drops_2star_usd_when_usd_only_baseline() {
        // SMI: USD is in baseline but NOT in news_currencies (mock).
        // 2★ USD should drop because USD-only is baseline-3★.
        let events = vec![ev("PMI", "USD", Impact::Medium, "2026-06-10T14:00:00Z")];
        let kept = filter_events(&events, &["CHF".into(), "EUR".into()], &["USD".into()]);
        assert_eq!(kept.len(), 0);
    }

    #[test]
    fn keeps_2star_usd_when_usd_is_in_news_currencies() {
        // EURUSD: USD is in news_currencies, so 2★ USD is kept.
        let events = vec![ev("PMI", "USD", Impact::Medium, "2026-06-10T14:00:00Z")];
        let kept = filter_events(&events, &["EUR".into(), "USD".into()], &["USD".into()]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn drops_unrelated_currency() {
        let events = vec![ev("CPI", "JPY", Impact::High, "2026-06-10T12:00:00Z")];
        let kept = filter_events(&events, &["EUR".into(), "USD".into()], &["USD".into()]);
        assert_eq!(kept.len(), 0);
    }

    #[test]
    fn case_insensitive_currency_match() {
        let events = vec![ev("CPI", "eur", Impact::High, "2026-06-10T12:00:00Z")];
        let kept = filter_events(&events, &["EUR".into()], &[]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn dedupe_drops_event_with_existing_line_in_tolerance() {
        let events = vec![ev("CPI", "USD", Impact::High, "2026-06-10T12:30:00Z")];
        let event_ts = events[0].datetime.with_timezone(&Utc).timestamp();
        // Existing line 3 minutes off — within ±5 min tolerance.
        let existing = vec![event_ts + 180];
        let kept = events_needing_drawing(events, &existing, chrono::Duration::minutes(5));
        assert!(kept.is_empty(), "expected dedupe to drop the event");
    }

    #[test]
    fn dedupe_keeps_event_outside_tolerance() {
        let events = vec![ev("CPI", "USD", Impact::High, "2026-06-10T12:30:00Z")];
        let event_ts = events[0].datetime.with_timezone(&Utc).timestamp();
        // Existing line 7 minutes off — outside ±5 min tolerance.
        let existing = vec![event_ts + 420];
        let kept = events_needing_drawing(events, &existing, chrono::Duration::minutes(5));
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn dedupe_keeps_when_no_existing_lines() {
        let events = vec![ev("CPI", "USD", Impact::High, "2026-06-10T12:30:00Z")];
        let kept = events_needing_drawing(events, &[], chrono::Duration::minutes(5));
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn news_anchor_is_event_time_in_utc() {
        let e = ev("CPI", "USD", Impact::High, "2026-06-10T12:30:00Z");
        assert_eq!(news_anchor(&e), ts("2026-06-10T12:30:00Z"));
    }
}
