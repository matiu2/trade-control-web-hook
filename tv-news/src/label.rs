//! Build the chart-drawing label for a forex-factory event.
//!
//! Format: `<currency>-<stars>-star-<name-slug>` — e.g. `usd-3-star-fomc`.
//!
//! - `currency` is lower-cased.
//! - `stars` is 1, 2, or 3 (mapped from `Impact`).
//! - `name-slug` is the event name lower-cased, with each run of
//!   non-alphanumeric characters collapsed to a single `-`.
//!
//! The slug is then truncated to a sensible cap (so TradingView's
//! drawing-properties UI stays readable). Currency + impact alone don't
//! uniquely identify an event within a week, so the slug is load-bearing
//! for dedupe — the label-prefix matcher in `filter::events_needing_drawing`
//! relies on `<currency>-<stars>-star-` being stable.

use trade_control_cli::EconomicEvent;

/// Maximum length of the name-slug portion, after slugification.
///
/// 48 chars keeps the full label under TradingView's visible label
/// budget while still preserving most real event names (`non-farm-employment-change`
/// is 26, `fomc-statement` is 14). Truncation is by character count, not
/// word boundary — the goal is a stable, predictable label, not pretty
/// prose.
const MAX_SLUG_LEN: usize = 48;

/// Build the canonical drawing label for `ev`.
///
/// Examples:
/// - USD 3★ "FOMC Statement" → `usd-3-star-fomc-statement`
/// - EUR 2★ "ECB Press Conference" → `eur-2-star-ecb-press-conference`
pub fn news_label(ev: &EconomicEvent) -> String {
    let ccy = ev.currency.to_lowercase();
    let stars = ev.impact.stars();
    let slug = slugify(&ev.name, MAX_SLUG_LEN);
    if slug.is_empty() {
        format!("{ccy}-{stars}-star")
    } else {
        format!("{ccy}-{stars}-star-{slug}")
    }
}

/// Label prefix shared by every event of a given currency + impact.
/// Used by the dedupe phase to recognise that *some* news line is
/// already drawn near a candidate event's timestamp, without requiring
/// the slug to match exactly (the user may have hand-renamed it).
pub fn news_label_prefix(ev: &EconomicEvent) -> String {
    let ccy = ev.currency.to_lowercase();
    let stars = ev.impact.stars();
    format!("{ccy}-{stars}-star-")
}

/// Does `label` look like a tv-news event marker? True when it matches
/// the `<ccy>-<n>-star-...` shape with `n` ∈ {1,2,3} and a 2-or-more
/// letter currency prefix. Used to decide whether to harvest a
/// drawing's anchor timestamp into the dedupe set.
///
/// Also accepts the legacy `news-start` / `news-end` labels emitted by
/// the prior tv-news layout so older annotated charts keep deduping.
pub fn is_news_label(label: &str) -> bool {
    let l = label.trim().to_ascii_lowercase();
    if l == "news-start" || l == "news-end" {
        return true;
    }
    let mut parts = l.splitn(4, '-');
    let ccy = parts.next().unwrap_or("");
    let stars = parts.next().unwrap_or("");
    let star = parts.next().unwrap_or("");
    if ccy.len() < 2 || !ccy.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if !matches!(stars, "1" | "2" | "3") {
        return false;
    }
    star == "star"
}

/// Lower-case `name`, replacing runs of non-alphanumeric characters
/// with single `-`, trimming leading/trailing `-`, then truncating to
/// `max_len` characters and re-trimming a trailing `-` from the
/// truncation cut.
fn slugify(name: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = true; // suppress leading dashes
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > max_len {
        out = out.chars().take(max_len).collect();
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use trade_control_cli::Impact;

    fn ev(name: &str, currency: &str, impact: Impact) -> EconomicEvent {
        EconomicEvent {
            name: name.to_string(),
            currency: currency.to_string(),
            impact,
            datetime: Local.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap(),
            actual: None,
            forecast: None,
            previous: None,
        }
    }

    #[test]
    fn label_combines_currency_stars_and_name() {
        let e = ev("FOMC", "USD", Impact::High);
        assert_eq!(news_label(&e), "usd-3-star-fomc");
    }

    #[test]
    fn label_lowercases_currency() {
        let e = ev("CPI", "eur", Impact::Medium);
        assert_eq!(news_label(&e), "eur-2-star-cpi");
    }

    #[test]
    fn label_slugifies_spaces_and_punctuation() {
        let e = ev("Non-Farm Payrolls", "USD", Impact::High);
        assert_eq!(news_label(&e), "usd-3-star-non-farm-payrolls");
    }

    #[test]
    fn label_collapses_runs_of_punctuation() {
        let e = ev("ECB  Press   Conference", "EUR", Impact::Medium);
        assert_eq!(news_label(&e), "eur-2-star-ecb-press-conference");
    }

    #[test]
    fn label_trims_leading_and_trailing_punct() {
        let e = ev("  (CPI y/y)  ", "AUD", Impact::Medium);
        assert_eq!(news_label(&e), "aud-2-star-cpi-y-y");
    }

    #[test]
    fn label_truncates_long_names() {
        let long_name = "a".repeat(200);
        let e = ev(&long_name, "USD", Impact::High);
        let label = news_label(&e);
        // "usd-3-star-" prefix = 11 chars, slug capped at MAX_SLUG_LEN.
        assert_eq!(label.len(), 11 + MAX_SLUG_LEN);
    }

    #[test]
    fn label_handles_unnamed_event() {
        let e = ev("???", "JPY", Impact::Low);
        // All non-alphanumeric → empty slug → "jpy-1-star" with no trailing dash.
        assert_eq!(news_label(&e), "jpy-1-star");
    }

    #[test]
    fn label_stars_match_impact_level() {
        assert_eq!(news_label(&ev("x", "USD", Impact::Low)), "usd-1-star-x");
        assert_eq!(news_label(&ev("x", "USD", Impact::Medium)), "usd-2-star-x");
        assert_eq!(news_label(&ev("x", "USD", Impact::High)), "usd-3-star-x");
    }

    #[test]
    fn prefix_matches_label_start() {
        let e = ev("FOMC Statement", "USD", Impact::High);
        let label = news_label(&e);
        let prefix = news_label_prefix(&e);
        assert!(
            label.starts_with(&prefix),
            "{label} should start with {prefix}"
        );
        assert_eq!(prefix, "usd-3-star-");
    }

    #[test]
    fn is_news_label_accepts_canonical_format() {
        assert!(is_news_label("usd-3-star-fomc"));
        assert!(is_news_label("eur-2-star-cpi-y-y"));
        assert!(is_news_label("jpy-1-star-something"));
    }

    #[test]
    fn is_news_label_accepts_legacy_labels() {
        assert!(is_news_label("news-start"));
        assert!(is_news_label("news-end"));
        assert!(is_news_label("  NEWS-START  "));
    }

    #[test]
    fn is_news_label_rejects_unrelated() {
        assert!(!is_news_label("neckline"));
        assert!(!is_news_label("too-high"));
        assert!(!is_news_label(""));
        assert!(!is_news_label("usd-4-star-x"));
        assert!(!is_news_label("usd-3-not-star"));
        assert!(!is_news_label("usd-3"));
    }
}
